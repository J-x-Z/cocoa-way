#!/usr/bin/env python3
"""Offline regressions for the exact production stdio Perl, not a model relay.

No Cocoa, Waypipe, containers, third-party modules, or repository outputs. The
fake commands only emit diagnostics/sleep; real pipes and sockets exercise the relay. Waits
are bounded correctness guards, never end-to-end latency measurements.
"""

import hashlib
import os
from pathlib import Path
import re
import select
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unittest


SOURCE = Path(__file__).resolve().parents[1] / "src/container_sessions.rs"
HEADER = struct.Struct("!BII")
PAYLOAD = bytes(range(256)) * (32 * 1024)


def production_script():
    matches = re.findall(
        r'fn guest_relay_perl\(\) -> &\x27static str \{\s*r#"(.*?)"#\s*\}',
        SOURCE.read_text(), re.S,
    )
    if len(matches) != 1:
        raise AssertionError("could not identify the exact production stdio script")
    return matches[0]


SCRIPT = production_script()


def read_socket(peer, size, timeout=4):
    deadline = time.monotonic() + timeout
    data = bytearray()
    while len(data) < size:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("peer read deadline")
        peer.settimeout(remaining)
        part = peer.recv(min(size - len(data), 65536))
        if not part:
            raise EOFError("peer closed before preceding DATA drained")
        data.extend(part)
    return bytes(data)


def worker(action):
    errors = []

    def run():
        try:
            action()
        except Exception as error:
            errors.append(error)

    thread = threading.Thread(target=run, daemon=True)
    thread.start()
    return thread, errors


class Relay:
    def __init__(self, command="import time; time.sleep(60)"):
        self.directory = tempfile.TemporaryDirectory(prefix="cw-stdio-", dir="/tmp")
        self.path = Path(self.directory.name) / "p"
        env = dict(os.environ, COCOA_WAY_RELAY_TEST_STATS="1",
                   XDG_RUNTIME_DIR=str(Path(self.directory.name) / "runtime"))
        self.process = subprocess.Popen(
            ["perl", "-e", SCRIPT, str(self.path), sys.executable, "-c", command],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            env=env, bufsize=0, start_new_session=True,
        )
        for pipe in (self.process.stdin, self.process.stdout, self.process.stderr):
            os.set_blocking(pipe.fileno(), False)
        self.peers = []
        self.stderr = b""

    def __enter__(self):
        return self

    def __exit__(self, *_):
        # Kill only this fixture's private group, including a fake command left
        # by a regressed relay. No application/container processes are involved.
        try:
            os.killpg(self.process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        self.process.wait(timeout=2)
        for peer in self.peers:
            peer.close()
        for pipe in (self.process.stdin, self.process.stdout, self.process.stderr):
            if pipe is not None:
                pipe.close()
        self.directory.cleanup()

    def write(self, data, timeout=5):
        deadline = time.monotonic() + timeout
        offset = 0
        fd = self.process.stdin.fileno()
        while offset < len(data):
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not select.select([], [fd], [], max(remaining, 0))[1]:
                raise TimeoutError("fixture pipe write deadline")
            try:
                offset += os.write(fd, memoryview(data)[offset:offset + 65536])
            except BlockingIOError:
                continue

    def frame(self, kind, stream, data=b"", timeout=5):
        self.write(HEADER.pack(kind, stream, len(data)) + data, timeout)

    def read(self, size, timeout=4):
        fd = self.process.stdout.fileno()
        deadline = time.monotonic() + timeout
        data = bytearray()
        while len(data) < size:
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not select.select([fd], [], [], max(remaining, 0))[0]:
                raise TimeoutError("fixture pipe read deadline")
            try:
                part = os.read(fd, min(size - len(data), 65536))
            except BlockingIOError:
                continue
            if not part:
                raise EOFError("relay stdout EOF")
            data.extend(part)
        return bytes(data)

    def receive(self):
        kind, stream, size = HEADER.unpack(self.read(9))
        if size > 16 * 1024 * 1024:
            raise AssertionError("invalid/unframed production stdout")
        return kind, stream, self.read(size)

    def peer(self):
        deadline = time.monotonic() + 4
        while True:
            peer = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            peer.settimeout(4)
            peer.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
            peer.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 4096)
            try:
                peer.connect(str(self.path))
                break
            except (FileNotFoundError, ConnectionRefusedError):
                peer.close()
                if time.monotonic() >= deadline:
                    raise TimeoutError("relay listener did not start")
                time.sleep(0.002)
        self.peers.append(peer)
        kind, stream, data = self.receive()
        assert kind == 1 and not data, (kind, stream, data)
        return peer, stream

    def close_input(self):
        if self.process.stdin is not None:
            self.process.stdin.close()
            self.process.stdin = None

    def wait(self, timeout=3):
        try:
            status = self.process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            raise TimeoutError("production stdio relay did not exit before fixture deadline") from None
        deadline = time.monotonic() + 2
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not select.select([self.process.stderr], [], [], max(remaining, 0))[0]:
                raise TimeoutError("fixture stderr did not close")
            part = os.read(self.process.stderr.fileno(), 65536)
            if not part:
                break
            self.stderr += part
        return status

    def finish(self):
        self.close_input()
        assert self.wait() == 0, self.stderr.decode(errors="replace")
        return self.stats()

    def stats(self):
        match = re.search(rb"cocoa-way stdio relay stats: ([^\n]+)", self.stderr)
        assert match, self.stderr.decode(errors="replace")
        stats = {key.decode(): int(value) for key, value in
                 (field.split(b"=") for field in match[1].split())}
        assert stats["output_high"] <= 256 * 1024, stats
        assert stats["peer_high"] <= 256 * 1024, stats
        assert stats["input_high"] <= 4 * 1024 * 1024, stats
        assert stats["output_chunks_high"] <= 256, stats
        assert stats["peer_chunks_high"] <= 256, stats
        return stats


class StdioReadAdmission(unittest.TestCase):
    def check_admission(self, setup, expected_size, expected_data):
        # Seed real production queues without draining stdout or depending on
        # kernel pipe sizes. Exercise both the production poll mask and read arm.
        imports = SCRIPT[:SCRIPT.index("my $sock_path = shift @ARGV;")]
        state = SCRIPT[SCRIPT.index("my $chunk_size ="):SCRIPT.index("RELAY: while (1) {")]
        watch_start = SCRIPT.index("  for my $p (values %peers) {\n    my $events =")
        watch = SCRIPT[watch_start:SCRIPT.index("\n  my $ready =", watch_start)]
        read_start = SCRIPT.index("\n    my $id = $peer_ids{fileno($fh)};") + 1
        read = SCRIPT[read_start:SCRIPT.index("\n  }\n}\nfor my $id", read_start)]
        script = imports + r"""
use Socket qw(AF_UNIX SOCK_STREAM PF_UNSPEC);
socketpair(my $server, my $writer, AF_UNIX, SOCK_STREAM, PF_UNSPEC) or die "socketpair: $!";
binmode $server;
binmode $writer;
""" + state + setup + f"""
my $expected_size = {expected_size};
my $expected_data = pack('H*', '{expected_data.hex()}');
""" + r"""
my @before = map { $_->[0] } @output;
my $before_bytes = $output_bytes;
$peers{7} = { fh => $server, queue => [], bytes => 0, closing => 0, notify => 0 };
$peer_ids{fileno($server)} = 7;
syswrite($writer, 'tiny') == 4 or die "fixture write: $!";
die "wrong read size" unless peer_read_size() == $expected_size;
my $poll = IO::Poll->new();
""" + watch + r"""
my $admitted = (($poll->mask($server) || 0) & POLLIN) != 0;
die "wrong poll admission" unless $admitted == ($expected_size > 0);
if ($admitted) {
  $poll->poll(0);
  die "tiny peer not ready" unless $poll->events($server) & POLLIN;
}
# Also exercise the final guard for a stale ready handle with no capacity.
for my $fh ($server) {
""" + read + r"""
}
die "peer spuriously closed" if $peers{7}{closing};
die "queue overflow" if $output_bytes > $output_limit || @output > $chunks_limit;
for my $i (0 .. $#before) {
  die "FIFO changed" unless $output[$i][0] eq $before[$i] && $output[$i][1] == 0;
}
my $added = length($expected_data) ? 1 : 0;
die "wrong frame count" unless @output == @before + $added;
die "wrong byte count" unless $output_bytes == $before_bytes + ($added ? 9 + length($expected_data) : 0);
die "wrong read cursor" unless $read_cursor == ($added ? 8 : 0);
if ($added) {
  die "wrong DATA frame" unless $output[-1][0] eq pack('CNN', 2, 7, length($expected_data)) . $expected_data;
}
if (length($expected_data) < 4) {
  my $left = '';
  my $n = sysread($server, $left, 4);
  die "peer bytes lost" unless defined $n && $left eq substr('tiny', length($expected_data));
}
print "ok\n";
"""
        result = subprocess.run(["perl", "-e", script], capture_output=True, timeout=5)
        self.assertEqual(result.returncode, 0, result.stderr.decode(errors="replace"))
        self.assertEqual(result.stdout, b"ok\n")

    def test_three_full_frames_admit_tiny_peer_without_draining_output(self):
        self.check_admission(
            "send_frame(2, 1, 'b' x $chunk_size) for 1 .. 3;\n",
            65500, b"tiny",
        )

    def test_no_header_or_payload_capacity_does_not_read_or_close_peer(self):
        for remaining in (0, 8, 9):
            with self.subTest(remaining=remaining):
                self.check_admission(
                    "send_frame(2, 1, 'b' x $chunk_size) for 1 .. 3;\n"
                    f"send_frame(2, 1, 'c' x ($output_limit - $output_bytes - 9 - {remaining}));\n",
                    0, b"",
                )

    def test_one_payload_byte_fills_capacity_without_overflow(self):
        self.check_admission(
            "send_frame(2, 1, 'b' x $chunk_size) for 1 .. 3;\n"
            "send_frame(2, 1, 'c' x ($output_limit - $output_bytes - 9 - 10));\n",
            1, b"t",
        )

    def test_max_payload_and_chunk_limit_are_retained(self):
        with self.subTest(limit="payload"):
            self.check_admission("", 65536, b"tiny")
        with self.subTest(limit="chunks"):
            self.check_admission(
                "send_frame(2, 1, '') for 1 .. $chunks_limit;\n", 0, b"",
            )


class StdioBackpressure(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        subprocess.run(["perl", "-c", "-e", SCRIPT], check=True,
                       capture_output=True, timeout=5)

    def test_stdout_backpressure_keeps_stdin_live_and_bytes_intact(self):
        with Relay() as relay:
            a, aid = relay.peer()
            b, bid = relay.peer()

            def upload():
                a.sendall(PAYLOAD)
                a.shutdown(socket.SHUT_WR)

            thread, errors = worker(upload)
            time.sleep(0.1)
            self.assertTrue(thread.is_alive(), "fixture did not saturate stdout")
            relay.frame(2, bid, b"independent host input")
            self.assertEqual(read_socket(b, 22), b"independent host input")
            b.sendall(b"reply")
            digest, count, reply, closed = hashlib.sha256(), 0, bytearray(), False
            deadline = time.monotonic() + 6
            while count < len(PAYLOAD) or reply != b"reply" or not closed:
                self.assertLess(time.monotonic(), deadline)
                kind, stream, data = relay.receive()
                if kind == 2 and stream == aid:
                    digest.update(data)
                    count += len(data)
                elif kind == 2 and stream == bid:
                    reply.extend(data)
                elif kind == 3 and stream == aid:
                    self.assertFalse(closed, "duplicate CLOSE")
                    self.assertEqual(count, len(PAYLOAD), "CLOSE overtook DATA")
                    closed = True
                else:
                    self.fail(f"unexpected frame {kind}/{stream}")
            thread.join(timeout=2)
            self.assertFalse(thread.is_alive())
            self.assertFalse(errors, errors)
            self.assertEqual(count, len(PAYLOAD))
            self.assertEqual(digest.digest(), hashlib.sha256(PAYLOAD).digest())
            stats = relay.finish()
            self.assertGreater(stats["output_pauses"], 0)
            self.assertGreater(stats["partial_writes"], 0)

    def test_peer_backpressure_keeps_stdout_live_and_preserves_same_lane_hol(self):
        with Relay() as relay:
            a, aid = relay.peer()
            b, bid = relay.peer()

            def upload():
                relay.frame(2, aid, PAYLOAD)
                relay.frame(3, aid)
                relay.frame(2, bid, b"after bulk")

            thread, errors = worker(upload)
            time.sleep(0.1)
            b.sendall(b"independent guest output")
            self.assertEqual(relay.receive(), (2, bid, b"independent guest output"))
            b.settimeout(0.1)
            # This is a documented residual, not permission to reorder/drop A.
            with self.assertRaises(socket.timeout):
                b.recv(1)
            self.assertEqual(read_socket(a, len(PAYLOAD)), PAYLOAD)
            a.settimeout(3)
            self.assertEqual(a.recv(1), b"", "CLOSE did not drain preceding DATA")
            self.assertEqual(read_socket(b, 10), b"after bulk")
            thread.join(timeout=2)
            self.assertFalse(thread.is_alive())
            self.assertFalse(errors, errors)
            self.assertGreater(relay.finish()["input_pauses"], 0)

    def test_blocked_stdout_has_production_deadline(self):
        with Relay() as relay:
            a, _ = relay.peer()
            a.settimeout(13)
            thread, _ = worker(lambda: a.sendall(PAYLOAD))
            self.assertEqual(relay.wait(timeout=13), 1)
            self.assertIn(b"queue/frame deadline exceeded", relay.stderr)
            self.assertGreater(relay.stats()["output_pauses"], 0)
            thread.join(timeout=2)
            self.assertFalse(thread.is_alive())

    def test_blocked_peer_has_production_deadline(self):
        with Relay() as relay:
            _, aid = relay.peer()
            thread, _ = worker(lambda: relay.frame(2, aid, PAYLOAD, timeout=13))
            self.assertEqual(relay.wait(timeout=13), 1)
            self.assertIn(b"queue/frame deadline exceeded", relay.stderr)
            self.assertGreater(relay.stats()["input_pauses"], 0)
            thread.join(timeout=2)
            self.assertFalse(thread.is_alive())

    def test_incomplete_frame_has_production_deadline(self):
        with Relay() as relay:
            relay.write(HEADER.pack(2, 99, 10) + b"x")
            self.assertEqual(relay.wait(timeout=13), 1)
            self.assertIn(b"queue/frame deadline exceeded", relay.stderr)
            relay.stats()

    def test_fragmented_headers_zero_data_unknown_peer_and_close(self):
        with Relay() as relay:
            a, aid = relay.peer()
            relay.frame(2, 9999, b"unknown peer is drained")
            relay.frame(2, aid)
            payload = bytes(range(256)) * 8
            encoded = HEADER.pack(2, aid, len(payload)) + payload + HEADER.pack(3, aid, 0)
            for byte in encoded[:9]:
                relay.write(bytes([byte]))
            relay.write(encoded[9:])
            self.assertEqual(read_socket(a, len(payload)), payload)
            a.settimeout(3)
            self.assertEqual(a.recv(1), b"")
            relay.finish()

    def test_clean_stdin_eof_drains_accepted_data(self):
        with Relay() as relay:
            a, aid = relay.peer()
            payload = PAYLOAD[:128 * 1024]
            relay.frame(2, aid, payload)
            relay.close_input()
            self.assertEqual(read_socket(a, len(payload)), payload)
            a.settimeout(3)
            self.assertEqual(a.recv(1), b"")
            self.assertEqual(relay.wait(), 0)
            relay.stats()

    def test_peer_churn_reclaims_admission_and_emits_one_close(self):
        with Relay() as relay:
            for _ in range(140):
                a, aid = relay.peer()
                a.close()
                self.assertEqual(relay.receive(), (3, aid, b""))
            relay.finish()

    def test_tiny_frames_keep_queues_bounded_and_ordered(self):
        with Relay() as relay:
            a, aid = relay.peer()
            b, bid = relay.peer()
            payload = bytes(range(256)) * 32
            encoded = b"".join(HEADER.pack(2, aid, 1) + bytes([byte]) for byte in payload)
            thread, errors = worker(lambda: relay.write(encoded))
            time.sleep(0.1)
            b.sendall(b"reverse")
            self.assertEqual(relay.receive(), (2, bid, b"reverse"))
            self.assertEqual(read_socket(a, len(payload)), payload)
            thread.join(timeout=2)
            self.assertFalse(thread.is_alive())
            self.assertFalse(errors, errors)
            relay.finish()

    def test_closed_stdout_and_uncooperative_child_have_bounded_teardown(self):
        command = ("import signal,time; signal.signal(signal.SIGTERM,signal.SIG_IGN); "
                   "print('child ready',flush=True); time.sleep(60)")
        with Relay(command) as relay:
            # Wait until the command installed its TERM disposition before EOF.
            ready, _, _ = select.select([relay.process.stderr], [], [], 3)
            self.assertTrue(ready)
            relay.stderr += os.read(relay.process.stderr.fileno(), 4096)
            self.assertIn(b"child ready", relay.stderr)
            a, _ = relay.peer()
            relay.process.stdout.close()
            relay.process.stdout = None
            # Exercise a failed pending write, not idle-pipe HUP reporting,
            # which differs between poll implementations on macOS and Linux.
            a.sendall(b"pending output")
            self.assertEqual(relay.wait(timeout=3), 1)

    def test_invalid_and_truncated_frames_fail_explicitly(self):
        for data in (HEADER.pack(99, 1, 0), HEADER.pack(3, 1, 1),
                     HEADER.pack(2, 1, 16 * 1024 * 1024 + 1),
                     b"\x02\x00", HEADER.pack(2, 1, 10) + b"x"):
            with self.subTest(data=data), Relay() as relay:
                relay.write(data)
                relay.close_input()
                self.assertEqual(relay.wait(), 1)
                self.assertRegex(relay.stderr, b"invalid|truncated")

    def test_child_diagnostics_do_not_share_framed_stdio(self):
        command = ("import fcntl,os,sys; assert sys.stdin.read()==''; "
                   "assert not (fcntl.fcntl(1,fcntl.F_GETFL)&os.O_NONBLOCK); "
                   "print('child diagnostic',flush=True)")
        with Relay(command) as relay:
            self.assertEqual(relay.wait(), 0)
            self.assertIn(b"child diagnostic", relay.stderr)
            with self.assertRaises(EOFError):
                relay.read(1)


if __name__ == "__main__":
    unittest.main(verbosity=2)
