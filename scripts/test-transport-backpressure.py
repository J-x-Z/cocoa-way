#!/usr/bin/env python3
"""Exercise the exact embedded V2 Perl relay without Cocoa, Waypipe or containers.

Input-delivery RTT uses an independent fake-guest sideband ACK, timed entirely
by this host. Relay RTT uses the normal shared return lane and includes its HOL.
Baseline Perl is extracted independently from git show at a pinned revision.
"""

import argparse
import contextlib
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time


ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "src/container_sessions.rs"
HEADER = struct.Struct("!BII")
TOKEN = struct.Struct("!Q")
BLOCK = bytes(range(256)) * 256
READY = b"COCOA_WAY_TRANSPORT_V2_READY\n"


def connect(path):
    deadline = time.monotonic() + 5
    while True:
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            sock.connect(str(path))
            return sock
        except (FileNotFoundError, ConnectionRefusedError):
            sock.close()
            if time.monotonic() > deadline:
                raise
            time.sleep(0.002)


def recv_exact(sock, length):
    data = bytearray()
    while len(data) < length:
        try:
            part = sock.recv(length - len(data))
        except (InterruptedError, socket.timeout):
            continue
        if not part:
            raise EOFError(f"EOF after {len(data)} of {length} bytes")
        data.extend(part)
    return bytes(data)


def payload_chunks(length):
    index = 0
    while length:
        size = min(length, len(BLOCK))
        # Distinct block headers make whole-chunk duplication/reordering visible.
        yield (TOKEN.pack(index) + BLOCK[8:])[:size]
        length -= size
        index += 1


def payload_bytes(length):
    return b"".join(payload_chunks(length))


def payload_hash(length):
    digest = hashlib.sha256()
    for part in payload_chunks(length):
        digest.update(part)
    return digest.hexdigest()


def fake_child(sideband_path, peer_path):
    sideband = connect(sideband_path)
    send_lock = threading.Lock()

    def event(kind, **values):
        with send_lock:
            sideband.sendall((json.dumps({"event": kind, **values}) + "\n").encode())

    bulk = connect(peer_path)
    bulk.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 16384)
    probe = connect(peer_path)

    def probes():
        try:
            while True:
                token = recv_exact(probe, TOKEN.size)
                event("probe", sequence=TOKEN.unpack(token)[0])
                probe.sendall(token)
        except (EOFError, OSError):
            pass

    threading.Thread(target=probes, daemon=True).start()
    event("ready")

    def produce(length):
        try:
            for part in payload_chunks(length):
                bulk.sendall(part)
            bulk.shutdown(socket.SHUT_WR)
            event("bulk_sent", bytes=length)
        except OSError as error:
            event("child_error", message=str(error))

    def receiver(token, delay, abort_after=None):
        peer = connect(peer_path)
        peer.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
        event("churn_connected", token=token)
        time.sleep(delay)
        event("churn_reading", token=token)
        digest = hashlib.sha256()
        count = 0
        while True:
            part = peer.recv(8192)
            if not part:
                break
            digest.update(part)
            count += len(part)
            if abort_after is not None and count >= abort_after:
                break
        peer.close()
        event("churn_done", token=token, bytes=count, sha256=digest.hexdigest())

    def flood(count):
        for _ in range(count):
            peer = connect(peer_path)
            peer.sendall(b"x")
            peer.close()
        event("flood_done", count=count)

    for line in sideband.makefile("rb"):
        command = json.loads(line)
        if command["command"] == "bulk":
            threading.Thread(target=produce, args=(command["bytes"],), daemon=True).start()
        elif command["command"] == "churn":
            threading.Thread(
                target=receiver,
                args=(command["token"], command.get("delay", 0), command.get("abort_after")),
                daemon=True,
            ).start()
        elif command["command"] == "guest_probe":
            probe.sendall(TOKEN.pack(command["sequence"]))
        elif command["command"] == "flood":
            threading.Thread(target=flood, args=(command["count"],), daemon=True).start()
    return 0


class Relay:
    def __init__(self, script, stall_ms=0, signals=False, collect_aux=False):
        # Keep Unix paths short even if TMPDIR is a long macOS per-user path.
        self.temp = tempfile.TemporaryDirectory(prefix="cw-bp-", dir="/tmp")
        base = Path(self.temp.name)
        self.peer_path, self.transport_path = base / "p", base / "t"
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.listener.bind(str(base / "s"))
        self.listener.listen(1)
        self.listener.settimeout(5)
        env = dict(os.environ)
        env.pop("COCOA_WAY_AUDIO_SOCKET", None)
        env["XDG_RUNTIME_DIR"] = str(base)
        env["COCOA_WAY_RELAY_TEST_STATS"] = "1"
        if signals:
            env["COCOA_WAY_RELAY_TEST_SIGNALS"] = "1"
        self.process = subprocess.Popen(
            ["perl", "-e", script, str(self.peer_path), str(self.transport_path),
             sys.executable, str(Path(__file__).resolve()), "--fake-child", str(base / "s"),
             str(self.peer_path)],
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            env=env, start_new_session=True,
        )
        self.condition = threading.Condition()
        self.events, self.opens, self.closes = [], [], []
        self.echoes, self.sent = {}, {}
        self.echo_order, self.sideband_probe_order = [], []
        self.bulk_hash, self.bulk_bytes = hashlib.sha256(), 0
        self.echo_buffer = b""
        self.stop = threading.Event()
        self.stall_started = threading.Event()
        self.stall_ms, self.stall_start_ns, self.stall_end_ns = stall_ms, None, None
        self.errors, self.stderr, self.rss = [], [], []
        self.frame_count = 0
        self.collect_aux, self.aux = collect_aux, {}
        self.send_lock = threading.Lock()
        self.threads = []
        assert self.process.stdout.readline() == READY, "relay failed before READY"
        self.transport = connect(self.transport_path)
        self.transport.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
        # A bounded guest queue must be allowed to backpressure a large host
        # frame across the longest injected stall; a short sendall timeout
        # would truncate the test frame rather than test relay correctness.
        self.transport.settimeout(10)
        # Exercise handshake fragmentation in every actual relay invocation.
        for byte in b"CWV2\x01":
            self.transport.sendall(bytes([byte]))
            time.sleep(0.0005)
        self.sideband, _ = self.listener.accept()
        for fn in (self.read_sideband, self.read_transport, self.read_stderr, self.sample_rss):
            thread = threading.Thread(target=fn, daemon=True)
            thread.start()
            self.threads.append(thread)
        self.wait_event("ready")
        self.wait_open()
        self.wait_open()

    def read_sideband(self):
        try:
            for line in self.sideband.makefile("rb"):
                timestamp = time.monotonic_ns()
                event = json.loads(line)
                event["host_receive_ns"] = timestamp
                with self.condition:
                    if event["event"] == "probe":
                        self.sideband_probe_order.append(event["sequence"])
                    self.events.append(event)
                    self.condition.notify_all()
        except OSError:
            pass

    def read_transport(self):
        try:
            while not self.stop.is_set():
                kind, stream, length = HEADER.unpack(recv_exact(self.transport, HEADER.size))
                if length > 16 * 1024 * 1024:
                    raise AssertionError("oversized output frame")
                data = recv_exact(self.transport, length)
                timestamp = time.monotonic_ns()
                with self.condition:
                    self.frame_count += 1
                    if kind == 1:
                        assert length == 0
                        self.opens.append(stream)
                    elif kind == 3:
                        assert length == 0
                        self.closes.append(stream)
                    elif kind == 2 and stream == 1:
                        self.bulk_hash.update(data)
                        self.bulk_bytes += length
                    elif kind == 2 and stream == 2:
                        self.echo_buffer += data
                        while len(self.echo_buffer) >= TOKEN.size:
                            sequence = TOKEN.unpack(self.echo_buffer[:TOKEN.size])[0]
                            self.echo_buffer = self.echo_buffer[TOKEN.size:]
                            assert sequence not in self.echoes, "duplicate probe echo"
                            self.echoes[sequence] = timestamp
                            self.echo_order.append(sequence)
                    elif kind == 2 and self.collect_aux:
                        count, digest = self.aux.setdefault(stream, [0, hashlib.sha256()])
                        digest.update(data)
                        self.aux[stream][0] = count + length
                    else:
                        raise AssertionError(f"unexpected frame {kind}/{stream}/{length}")
                    self.condition.notify_all()
                if kind == 2 and stream == 1 and self.stall_ms and self.stall_start_ns is None:
                    self.stall_start_ns = time.monotonic_ns()
                    self.stall_started.set()
                    time.sleep(self.stall_ms / 1000)
                    self.stall_end_ns = time.monotonic_ns()
        except (EOFError, OSError):
            pass
        except Exception as error:
            self.errors.append(repr(error))

    def read_stderr(self):
        for line in self.process.stderr:
            self.stderr.append(line.decode(errors="replace").rstrip())

    def sample_rss(self):
        while not self.stop.is_set():
            result = subprocess.run(
                ["ps", "-o", "rss=", "-p", str(self.process.pid)],
                capture_output=True, text=True, check=False,
            )
            if result.stdout.strip():
                self.rss.append({"host_ns": time.monotonic_ns(), "kib": int(result.stdout)})
            self.stop.wait(0.05)

    def wait_for(self, predicate, timeout=10):
        deadline = time.monotonic() + timeout
        with self.condition:
            while True:
                value = predicate()
                if value is not None:
                    return value
                if self.errors:
                    raise AssertionError(self.errors)
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError(f"relay wait timed out: {self.stderr[-5:]}")
                self.condition.wait(min(remaining, 0.1))

    def wait_event(self, kind, **fields):
        def find():
            for index, event in enumerate(self.events):
                if event["event"] == kind and all(event.get(k) == v for k, v in fields.items()):
                    return self.events.pop(index)
            return None
        return self.wait_for(find)

    def wait_open(self):
        return self.wait_for(lambda: self.opens.pop(0) if self.opens else None)

    def command(self, command, **values):
        self.sideband.sendall((json.dumps({"command": command, **values}) + "\n").encode())

    def frame(self, kind, stream, data=b"", fragmented=False):
        encoded = HEADER.pack(kind, stream, len(data)) + data
        with self.send_lock:
            if fragmented:
                for index in range(HEADER.size):
                    self.transport.sendall(encoded[index:index + 1])
                    time.sleep(0.0005)
                for index in range(HEADER.size, len(encoded), 997):
                    self.transport.sendall(encoded[index:index + 997])
            else:
                self.transport.sendall(encoded)

    def probe(self, sequence):
        self.sent[sequence] = time.monotonic_ns()
        self.frame(2, 2, TOKEN.pack(sequence))

    def finish(self):
        self.stop.set()
        # Closing the private command channel lets the fake child exit naturally,
        # including after fatal Perl errors. Never signal an unrelated process group.
        with contextlib.suppress(OSError):
            self.sideband.shutdown(socket.SHUT_RDWR)
        self.sideband.close()
        with contextlib.suppress(OSError):
            self.transport.shutdown(socket.SHUT_RDWR)
        self.transport.close()
        try:
            self.process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            self.process.terminate()
            self.process.wait(timeout=3)
        self.listener.close()
        for thread in self.threads:
            thread.join(timeout=1)
        self.temp.cleanup()

    def stats(self):
        for line in self.stderr:
            if line.startswith("cocoa-way transport v2 stats: "):
                return {key: int(value) for key, value in re.findall(r"(\w+)=(\d+)", line)}
        return None


def percentiles(values):
    ordered = sorted(values)
    def percentile(p):
        return ordered[max(0, math.ceil(len(ordered) * p) - 1)] if ordered else None
    return {"count": len(values), "p50_ms": percentile(0.5), "p95_ms": percentile(0.95),
            "p99_ms": percentile(0.99), "max_ms": max(values) if values else None}


def check_bounds(stats):
    limits = {"output_high": 256 * 1024, "input_high": 4 * 1024 * 1024,
              "peer_high": 256 * 1024, "peers_high": 128,
              "output_frames_high": 4096, "peer_chunks_high": 5}
    if stats is None:
        return None
    for key, limit in limits.items():
        assert stats[key] <= limit, f"{key}={stats[key]} > {limit}"
    return {"passed": True, "limits": limits, "observed": stats}


def memory_summary(samples, start_ns=None, end_ns=None):
    plateau = [s["kib"] for s in samples
               if start_ns is not None and start_ns + 500_000_000 <= s["host_ns"] < end_ns]
    return {"peak_rss_kib": max((s["kib"] for s in samples), default=None),
            "plateau_samples": len(plateau),
            "plateau_min_kib": min(plateau) if plateau else None,
            "plateau_max_kib": max(plateau) if plateau else None}


def backpressure(script, stall_ms, bulk_bytes, interval_ms, signals=False):
    relay = Relay(script, stall_ms, signals)
    result = {"requested_stall_ms": stall_ms, "bulk_expected_bytes": bulk_bytes}
    try:
        relay.probe(0)
        relay.wait_event("probe", sequence=0)
        relay.wait_for(lambda: relay.echoes.get(0))
        relay.command("bulk", bytes=bulk_bytes)
        assert relay.stall_started.wait(5), "bulk never reached host"
        time.sleep(min(0.003, stall_ms / 4000))
        count = max(128, math.ceil(stall_ms / interval_ms) + 32)
        arrivals = {}
        for sequence in range(1, count + 1):
            relay.probe(sequence)
            if signals:
                os.kill(relay.process.pid, signal.SIGUSR1)
            time.sleep(interval_ms / 1000)
        for sequence in range(1, count + 1):
            arrivals[sequence] = relay.wait_event("probe", sequence=sequence)["host_receive_ns"]
        relay.wait_for(lambda: True if len(relay.echoes) == count + 1 else None)
        relay.wait_for(lambda: True if relay.bulk_bytes == bulk_bytes else None)
        relay.wait_for(lambda: True if 1 in relay.closes else None)
        assert not relay.errors
        assert relay.bulk_hash.hexdigest() == payload_hash(bulk_bytes), "bulk corruption"
        assert relay.echo_order == list(range(count + 1)), "probe echo reordering"
        assert relay.sideband_probe_order == list(range(count + 1)), "probe delivery reordering"
        samples = []
        for sequence in range(1, count + 1):
            sent = relay.sent[sequence]
            samples.append({"sequence": sequence, "host_send_ns": sent,
                            "sideband_ack_ns": arrivals[sequence],
                            "relay_echo_ns": relay.echoes[sequence],
                            "during_stall": relay.stall_start_ns <= sent < relay.stall_end_ns,
                            "delivery_rtt_ms": (arrivals[sequence] - sent) / 1e6,
                            "relay_rtt_ms": (relay.echoes[sequence] - sent) / 1e6})
        result.update(samples=samples, stall_start_ns=relay.stall_start_ns,
                      stall_end_ns=relay.stall_end_ns,
                      actual_stall_ms=(relay.stall_end_ns - relay.stall_start_ns) / 1e6,
                      bulk_received_bytes=relay.bulk_bytes, bulk_sha256=relay.bulk_hash.hexdigest(),
                      byte_integrity=True, probe_integrity=True, bulk_close_seen=True)
    finally:
        relay.finish()
        result.update(relay_exit_code=relay.process.returncode, relay_stats=relay.stats(),
                      rss_samples=relay.rss, stderr=relay.stderr, errors=relay.errors)
        result["queue_bounds"] = check_bounds(relay.stats())
        result["memory"] = memory_summary(relay.rss, relay.stall_start_ns, relay.stall_end_ns)
        if signals:
            assert relay.stats()["signals"] > 0 and relay.stats()["eintr"] > 0, relay.stats()
    return result


def peer_backpressure(script):
    relay = Relay(script)
    result = {}
    errors = []
    try:
        relay.command("churn", token=0, delay=0.5)
        relay.wait_event("churn_connected", token=0)
        stream = relay.wait_open()
        size = 16 * 1024 * 1024
        def writer():
            try:
                relay.frame(2, stream, payload_bytes(size))
                relay.frame(3, stream)
            except Exception as error:
                errors.append(repr(error))
                relay.errors.append(repr(error))
        thread = threading.Thread(target=writer)
        thread.start()
        time.sleep(0.01)
        sent = {}
        for sequence in range(1000, 1150):
            sent[sequence] = time.monotonic_ns()
            relay.command("guest_probe", sequence=sequence)
            time.sleep(0.002)
        reading = relay.wait_event("churn_reading", token=0)["host_receive_ns"]
        relay.wait_for(lambda: True if len(relay.echoes) == len(sent) else None)
        done = relay.wait_event("churn_done", token=0)
        thread.join(timeout=5)
        assert not thread.is_alive() and not errors, errors
        assert done["bytes"] == size and done["sha256"] == payload_hash(size), done
        assert relay.echo_order == list(sent), "reverse probe reordering"
        samples = [{"sequence": seq, "host_send_ns": timestamp,
                    "host_receive_ns": relay.echoes[seq], "during_peer_stall": timestamp < reading,
                    "rtt_ms": (relay.echoes[seq] - timestamp) / 1e6} for seq, timestamp in sent.items()]
        result.update(samples=samples, byte_integrity=True, requested_peer_stall_ms=500,
                      guest_probe_boundary="host sideband command -> guest sends probe -> relay host receipt",
                      summary=percentiles([s["rtt_ms"] for s in samples if s["during_peer_stall"]]))
    finally:
        relay.finish()
        result.update(relay_stats=relay.stats(), queue_bounds=check_bounds(relay.stats()),
                      rss_samples=relay.rss, memory=memory_summary(relay.rss), stderr=relay.stderr)
    return result


def tiny_frame_pressure(script, count=4096):
    relay = Relay(script, 2000, collect_aux=True)
    result = {"streams": count}
    try:
        relay.command("bulk", bytes=65536)
        assert relay.stall_started.wait(5)
        relay.command("flood", count=count)
        relay.wait_event("flood_done", count=count)
        relay.wait_for(lambda: True if len(relay.aux) == count and len(relay.closes) == count + 1 else None)
        assert all(n == 1 and digest.hexdigest() == hashlib.sha256(b"x").hexdigest()
                   for n, digest in relay.aux.values()), "tiny-frame corruption"
        assert len(set(relay.closes)) == len(relay.closes), "duplicate CLOSE"
        result.update(byte_integrity=True, close_integrity=True,
                      bulk_integrity=relay.bulk_bytes == 65536 and relay.bulk_hash.hexdigest() == payload_hash(65536))
        assert result["bulk_integrity"]
    finally:
        relay.finish()
        result.update(relay_stats=relay.stats(), queue_bounds=check_bounds(relay.stats()),
                      rss_samples=relay.rss, stderr=relay.stderr,
                      memory=memory_summary(relay.rss, relay.stall_start_ns, relay.stall_end_ns))
    return result


def framing_and_churn(script, strict, churn_count):
    relay = Relay(script)
    result = {"checks": [], "churn_count": churn_count}
    try:
        for token in range(churn_count):
            # The large first frame exceeds both socket buffers and the new per-peer
            # high-water mark; CLOSE immediately after it must not drop queued bytes.
            size = 16 * 1024 * 1024 if token == 0 else 65537
            relay.command("churn", token=token, delay=0.1 if token == 0 else 0)
            relay.wait_event("churn_connected", token=token)
            stream = relay.wait_open()
            relay.frame(2, stream, b"")
            data = payload_bytes(size)
            relay.frame(2, stream, data, fragmented=token == 1)
            relay.frame(3, stream)
            relay.frame(3, stream)
            done = relay.wait_event("churn_done", token=token)
            assert done["bytes"] == size and done["sha256"] == payload_hash(size), done
        result["checks"].append({"name": "maximum_frame_partial_writes_fragmentation_order_close_churn",
                                 "passed": True})
        if strict:
            relay.command("churn", token=churn_count, delay=0.05, abort_after=8192)
            relay.wait_event("churn_connected", token=churn_count)
            stream = relay.wait_open()
            relay.frame(2, stream, payload_bytes(16 * 1024 * 1024))
            done = relay.wait_event("churn_done", token=churn_count)
            assert done["bytes"] >= 8192 and done["bytes"] < 16 * 1024 * 1024
            relay.wait_for(lambda: True if stream in relay.closes else None)
            relay.probe(10000)
            relay.wait_event("probe", sequence=10000)
            relay.wait_for(lambda: relay.echoes.get(10000))
            assert relay.process.poll() is None, "peer reset killed the transport"
            result["checks"].append({"name": "peer_reset_mid_frame_discard_and_other_stream_survives",
                                     "passed": True})
        assert not relay.errors
    finally:
        relay.finish()
        result.update(rss_samples=relay.rss, relay_stats=relay.stats(), stderr=relay.stderr,
                      queue_bounds=check_bounds(relay.stats()), memory=memory_summary(relay.rss))
    for name, packet in (
        ("oversized_frame", HEADER.pack(2, 2, 16 * 1024 * 1024 + 1)),
        ("truncated_header", HEADER.pack(2, 2, 64)[:4]),
        ("truncated_payload", HEADER.pack(2, 2, 64) + b"short"),
        ("clean_eof", b""),
    ):
        relay = Relay(script)
        try:
            relay.transport.sendall(packet)
            with contextlib.suppress(OSError):
                relay.transport.shutdown(socket.SHUT_WR)
            code = relay.process.wait(timeout=5)
            expected_rejection = name != "clean_eof"
            passed = (code != 0) if expected_rejection else (code == 0)
            result["checks"].append({"name": name, "passed": passed, "exit_code": code,
                                     "required": strict or name in ("oversized_frame", "clean_eof")})
            if strict or name in ("oversized_frame", "clean_eof"):
                assert passed, f"{name}: exit {code}"
        finally:
            relay.finish()
    return result


def disconnect_under_backpressure(script, strict):
    relay = Relay(script)
    writer_errors = []
    result = {"required": strict}
    thread = None
    cancel_writer = threading.Event()
    writer_blocked = threading.Event()
    try:
        relay.command("churn", token=0, delay=60)
        relay.wait_event("churn_connected", token=0)
        stream = relay.wait_open()
        relay.transport.setblocking(False)
        def writer():
            packet = HEADER.pack(2, stream, 16 * 1024 * 1024) + payload_bytes(1024 * 1024)
            offset = 0
            try:
                # Deliberately disconnect during an incomplete, backpressured frame.
                while offset < len(packet) and not cancel_writer.is_set():
                    try:
                        offset += relay.transport.send(packet[offset:offset + 65536])
                    except BlockingIOError:
                        writer_blocked.set()
                        cancel_writer.wait(0.001)
            except OSError as error:
                writer_errors.append(str(error))
            result["host_bytes_sent_before_cancel"] = offset
        thread = threading.Thread(target=writer)
        thread.start()
        time.sleep(0.05)
        start = time.monotonic_ns()
        cancel_writer.set()
        relay.transport.shutdown(socket.SHUT_RDWR)
        relay.transport.close()
        try:
            code = relay.process.wait(timeout=1)
            prompt = True
        except subprocess.TimeoutExpired:
            code, prompt = None, False
        result.update(prompt_exit=prompt, exit_code=code,
                      host_write_backpressure_seen=writer_blocked.is_set(),
                      disconnect_to_exit_ms=(time.monotonic_ns() - start) / 1e6)
    finally:
        relay.finish()
        if thread is not None:
            thread.join(timeout=1)
            assert not thread.is_alive(), "test writer did not terminate after cleanup"
        result.update(stderr=relay.stderr, writer_errors=writer_errors)
    return result


def extract(source):
    match = re.search(r'fn guest_publish_socket_relay_perl\(\) -> &\x27static str \{\s*r#"(.*?)"#\s*\}',
                      source, re.DOTALL)
    if not match:
        raise ValueError("cannot locate exact embedded V2 Perl")
    return match.group(1)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fake-child", nargs=2, metavar=("SIDEBAND", "PEER"))
    parser.add_argument("--variant", choices=("baseline", "current", "both"), default="both")
    parser.add_argument("--baseline-ref", default="4936390a16d5ee334203e32953f5939bed8c0c4b")
    parser.add_argument("--trials", type=int, default=5)
    parser.add_argument("--bulk-mib", type=int, default=8)
    parser.add_argument("--interval-ms", type=float, default=1)
    parser.add_argument("--churn", type=int, default=64)
    parser.add_argument("--output", default="results.json")
    parser.add_argument("--skip-checks", action="store_true")
    parser.add_argument("--checks-only", action="store_true")
    parser.add_argument("--signals", action="store_true")
    args = parser.parse_args()
    if args.fake_child:
        return fake_child(*args.fake_child)
    if args.trials < 1 or args.bulk_mib < 1 or args.interval_ms <= 0 or args.churn < 2:
        parser.error("trials/bulk/interval must be positive; churn must be at least two")
    destination = ROOT / "target/latency-audit"
    destination.mkdir(parents=True, exist_ok=True)
    baseline_source = subprocess.run(
        ["git", "show", f"{args.baseline_ref}:src/container_sessions.rs"], cwd=ROOT,
        capture_output=True, text=True, check=True,
    ).stdout
    scripts = {"baseline": extract(baseline_source), "current": extract(SOURCE.read_text())}
    result = {"platform": platform.platform(), "python": sys.version,
              "clock": "host time.monotonic_ns; no host/guest clock subtraction",
              "delivery_rtt_boundary": "host send -> fake guest receipt -> independent sideband host ACK",
              "relay_rtt_boundary": "host send -> fake guest echo -> shared transport host receipt",
              "stall_boundary": "after first complete bulk DATA frame; no transport reads until timer ends",
              "baseline_git_ref": args.baseline_ref, "arguments": vars(args), "variants": {}}
    output = destination / Path(args.output).name
    try:
        names = ("baseline", "current") if args.variant == "both" else (args.variant,)
        for name in names:
            script = scripts[name]
            (destination / f"{name}.pl").write_text(script)
            digest = hashlib.sha256(script.encode()).hexdigest()
            (destination / f"{name}-{digest[:16]}.pl").write_text(script)
            syntax = subprocess.run(["perl", "-c", "-e", script], capture_output=True, text=True)
            assert syntax.returncode == 0, syntax.stderr
            variant = {"perl_sha256": digest,
                       "perl_syntax": syntax.stderr.strip(), "runs": []}
            result["variants"][name] = variant
        for stall in (() if args.checks_only else (10, 100, 500)):
            for trial in range(args.trials):
                # Alternate pair order to reduce drift from unrelated host load.
                for name in names if trial % 2 == 0 else names[::-1]:
                    script = scripts[name]
                    variant = result["variants"][name]
                    run = backpressure(script, stall, args.bulk_mib * 1024 * 1024,
                                       args.interval_ms, args.signals and name == "current")
                    run["trial"] = trial
                    variant["runs"].append(run)
                    print(f"{name} stall={stall} trial={trial} "
                          f"delivery={percentiles([s['delivery_rtt_ms'] for s in run['samples']])}", flush=True)
                    output.write_text(json.dumps(result, indent=2) + "\n")
            for name in names:
                variant = result["variants"][name]
                samples = [sample for run in variant["runs"] if run["requested_stall_ms"] == stall
                           for sample in run["samples"]]
                variant.setdefault("summary", {})[str(stall)] = {
                    "delivery_all": percentiles([s["delivery_rtt_ms"] for s in samples]),
                    "delivery_during_stall": percentiles([s["delivery_rtt_ms"] for s in samples if s["during_stall"]]),
                    "relay_during_stall": percentiles([s["relay_rtt_ms"] for s in samples if s["during_stall"]]),
                }
        for name in names:
            script = scripts[name]
            variant = result["variants"][name]
            if not args.skip_checks:
                variant["framing_and_churn"] = framing_and_churn(script, name == "current", args.churn)
                variant["peer_backpressure"] = peer_backpressure(script)
                variant["memory_pressure"] = backpressure(script, 2000, 64 * 1024 * 1024, 10)
                variant["tiny_frame_pressure"] = tiny_frame_pressure(script)
                variant["disconnect_under_backpressure"] = disconnect_under_backpressure(script, name == "current")
                print(f"{name} disconnect: {variant['disconnect_under_backpressure']}", flush=True)
                output.write_text(json.dumps(result, indent=2) + "\n")
                if name == "current":
                    assert variant["disconnect_under_backpressure"]["prompt_exit"], "disconnect did not wake relay"
            output.write_text(json.dumps(result, indent=2) + "\n")
        result["completed"] = True
    except Exception as error:
        result["completed"] = False
        result["failure"] = repr(error)
        raise
    finally:
        output.write_text(json.dumps(result, indent=2) + "\n")
        print(f"Results: {output}", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
