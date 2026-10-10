#!/usr/bin/env python3
"""Bounded host CoreAnimation <-> Linux CLOCK_MONOTONIC calibration sidecar.

Guest: python3 measure-clock-offset.py --server --bind 0.0.0.0 --port 45871
Host:  python3 measure-clock-offset.py --host GUEST_IP --port 45871 --output offset.json
Unix:  use --unix PATH instead of TCP arguments on either end (needs a shared/
       forwarded socket, not magically a shared clock or guest filesystem).
Tests: python3 measure-clock-offset.py --self-test  (no sockets/host clock/files)

Define offset theta = guest - host. For h0=host send, g1=guest receive,
g2=guest send, h3=host receive, nonnegative path delays imply
theta in [g2-h3, g1-h0] under a constant-offset model. Asymmetric delays are NOT
assumed equal. Cross-clock differences occur only in calibration constraints or
latency corrected by an explicit offset interval, never as raw latency metrics.

Transport boundaries are application timestamps, NOT kernel/on-wire timestamps:
receive is after the complete frame, before decoding; send is before sendall
(guest reply serialization is included). Host uses ctypes CACurrentMediaTime,
the same epoch as MTLDrawable.presentedTime and latency_trace::now_ns(). Guest
uses clock_gettime_ns(CLOCK_MONOTONIC), not wall time or MONOTONIC_RAW.

Each timestamp's declared --clock-error-ns budget and arithmetic/resolution error
widen the constraints. --drift-ppm is an ASSUMED bound on relative offset change,
not a measured guarantee. Bounds are transported to a common host reference;
disjoint intervals invalidate the calibration. Observed endpoint-average drift
is reported as an interval, not a fitted point estimate. --valid-for permits
bounded future extrapolation, conditional on that assumption and matching host/
guest clock identity. Suspend, guest restart/migration, time-namespace changes,
or clock discontinuities invalidate it. Later probes must match the saved clock
identity and host validity window. Bound the guest receipt's physical host-time
bracket (e.g. host send..reply receive), and widen offset by WORST elapsed-time
drift over that bracket, not only drift at the input timestamp.

At most 10,000 samples and 4,096 bytes/frame. Each whole exchange has a deadline,
including partial-frame reads; server has an absolute lifetime and handles one
client. SIGINT/SIGTERM close sockets and remove ONLY the Unix socket we created.
Client saves collected samples even on timeout/interruption. No authentication:
bind only on a trusted diagnostic connection. JSON is a standalone sidecar,
not trace JSONL and not evidence linking any input to any surface commit.
"""

import argparse
import ctypes
from fractions import Fraction
import ipaddress
import json
import math
import os
import signal
import socket
import struct
import sys
import tempfile
import time
import unittest
import uuid


MAX_FRAME_BYTES = 4096
MAX_SAMPLES = 10_000
MIN_SAMPLES = 100
U64_MAX = (1 << 64) - 1
QUARTZCORE = "/System/Library/Frameworks/QuartzCore.framework/QuartzCore"


class StopRequested(Exception):
    def __init__(self, signum):
        self.signum = signum
        super().__init__(f"interrupted by signal {signum}")


def is_u64(value):
    return type(value) is int and 0 <= value <= U64_MAX


def drift_ns(distance_ns, ppm):
    rate = Fraction(str(ppm)) / 1_000_000
    return (abs(distance_ns) * rate.numerator + rate.denominator - 1) // rate.denominator


def host_clock(error_budget_ns):
    if sys.platform != "darwin":
        raise ValueError("client requires macOS QuartzCore; no substitute clock is allowed")
    library = ctypes.CDLL(QUARTZCORE)
    clock = library.CACurrentMediaTime
    clock.argtypes = []
    clock.restype = ctypes.c_double

    def now():
        seconds = clock()
        ns = seconds * 1_000_000_000
        if not math.isfinite(ns) or not 0 <= ns < (1 << 64):
            raise ValueError("invalid CACurrentMediaTime")
        rounding = math.ceil(math.ulp(seconds) * 1_000_000_000 + math.ulp(ns)) + 1
        return int(ns), error_budget_ns + rounding

    return now


def offset_interval(sample, reference_host_ns, ppm):
    """Lower constraint is at h3; upper at h0. Transport each independently."""
    h0, h3 = sample["host_send_ns"], sample["host_recv_ns"]
    g1, g2 = sample["guest_recv_ns"], sample["guest_send_ns"]
    lower = g2 - h3 - sample["host_recv_error_ns"] - sample["guest_error_ns"]
    upper = g1 - h0 + sample["host_send_error_ns"] + sample["guest_error_ns"]
    return [lower - drift_ns(abs(reference_host_ns - h3) + sample["host_recv_error_ns"], ppm),
            upper + drift_ns(abs(reference_host_ns - h0) + sample["host_send_error_ns"], ppm)]


def make_sample(seq, h0, g1, g2, h3, host_send_error_ns=0, host_recv_error_ns=0,
                guest_error_ns=0, ppm=0):
    sample = {"seq": seq, "host_send_ns": h0, "guest_recv_ns": g1,
              "guest_send_ns": g2, "host_recv_ns": h3,
              "host_send_error_ns": host_send_error_ns,
              "host_recv_error_ns": host_recv_error_ns, "guest_error_ns": guest_error_ns}
    if is_u64(h0) and is_u64(h3) and h3 >= h0:
        sample["rtt_ns"] = h3 - h0
    values = [h0, g1, g2, h3, host_send_error_ns, host_recv_error_ns, guest_error_ns]
    if not all(is_u64(value) for value in values):
        sample["invalid"] = "non-u64 timestamp/error"
        return sample
    if h3 < h0 or g2 < g1:
        sample["invalid"] = "reversed same-clock timestamps"
        return sample
    sample["guest_processing_ns"] = g2 - g1
    sample["raw_offset_interval_ns"] = [g2 - h3, g1 - h0]
    if g2 - h3 > g1 - h0:
        sample["invalid"] = "invalid raw offset interval (guest processing exceeds host RTT)"
        return sample
    sample["anchor_host_ns"] = (h0 + h3) // 2
    sample["offset_interval_ns_at_anchor"] = offset_interval(sample, sample["anchor_host_ns"], ppm)
    return sample


def one_way_bounds(sample, offset_at_reference, reference_host_ns, ppm):
    """Both guest events are physically bracketed by this sample's h0/h3."""
    h0, h3 = sample["host_send_ns"], sample["host_recv_ns"]
    distance = max(abs(h0 - reference_host_ns) + sample["host_send_error_ns"],
                   abs(h3 - reference_host_ns) + sample["host_recv_error_ns"])
    drift = drift_ns(distance, ppm)
    lower, upper = offset_at_reference[0] - drift, offset_at_reference[1] + drift
    forward = sample["guest_recv_ns"] - h0
    reverse = h3 - sample["guest_send_ns"]
    f_error = sample["host_send_error_ns"] + sample["guest_error_ns"]
    r_error = sample["host_recv_error_ns"] + sample["guest_error_ns"]
    rtt_upper = sample["rtt_ns"] + sample["host_send_error_ns"] + sample["host_recv_error_ns"]
    bounds = {
        "host_to_guest_ns": [max(0, forward - upper - f_error), min(rtt_upper, forward - lower + f_error)],
        "guest_to_host_ns": [max(0, reverse + lower - r_error), min(rtt_upper, reverse + upper + r_error)],
    }
    if any(hi < lo for lo, hi in bounds.values()):
        raise ValueError("calibrated one-way interval is empty")
    return bounds


def quantiles(values):
    ordered = sorted(values)
    return {name: ordered[math.ceil(len(ordered) * p) - 1]
            for name, p in (("p50", .50), ("p95", .95), ("p99", .99))} if ordered else None


def summarize(samples, ppm, valid_for_ns):
    valid = [s for s in samples if "invalid" not in s]
    for sample in samples:
        sample.pop("one_way_bounds_ns", None)
    for sample in valid:
        sample["offset_interval_ns_at_anchor"] = offset_interval(sample, sample["anchor_host_ns"], ppm)
    rtts = [s["rtt_ns"] for s in samples if "rtt_ns" in s]
    result = {"valid_samples": len(valid), "invalid_samples": len(samples) - len(valid),
              "rtt_samples": len(rtts), "rtt_ns": quantiles(rtts),
              "assumed_max_abs_offset_drift_ppm": ppm,
              "drift_budget_is_not_a_measured_guarantee": True}
    if not valid:
        result["consistent"] = False
        return result
    start = min(s["host_send_ns"] for s in valid)
    end = max(s["host_recv_ns"] for s in valid)
    reference = (start + end) // 2
    intervals = [offset_interval(s, reference, ppm) for s in valid]
    lower, upper = max(i[0] for i in intervals), min(i[1] for i in intervals)
    result.update({"reference_host_ns": reference, "measurement_host_interval_ns": [start, end],
                   "offset_interval_ns": [lower, upper], "consistent": lower <= upper})
    first, last = valid[0], valid[-1]
    elapsed = last["anchor_host_ns"] - first["anchor_host_ns"]
    if elapsed > 0:
        a, b = first["offset_interval_ns_at_anchor"], last["offset_interval_ns_at_anchor"]
        result["observed_average_drift_ppm_interval"] = [(b[0] - a[1]) * 1_000_000 / elapsed,
                                                        (b[1] - a[0]) * 1_000_000 / elapsed]
    if lower <= upper:
        midpoint = (lower + upper) // 2
        result.update({"offset_midpoint_ns": midpoint, "offset_midpoint_error_ns": max(midpoint - lower, upper - midpoint),
                       "conditional_valid_host_interval_ns": [start, end + valid_for_ns],
                       "extrapolation_error_ns_at_valid_until": drift_ns(end + valid_for_ns - reference, ppm)})
        for sample in valid:
            sample["one_way_bounds_ns"] = one_way_bounds(sample, [lower, upper], reference, ppm)
        for direction in ("host_to_guest_ns", "guest_to_host_ns"):
            lows = quantiles([s["one_way_bounds_ns"][direction][0] for s in valid])
            highs = quantiles([s["one_way_bounds_ns"][direction][1] for s in valid])
            result[f"{direction}_quantile_bounds"] = {name: [lows[name], highs[name]] for name in lows}
    return result


def deadline_timeout(sock, deadline):
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise TimeoutError("whole-frame/exchange deadline expired")
    sock.settimeout(remaining)


def recv_exact(sock, size, deadline):
    data = bytearray()
    while len(data) < size:
        deadline_timeout(sock, deadline)
        chunk = sock.recv(size - len(data))
        if not chunk:
            if not data:
                raise EOFError("peer closed")
            raise ValueError("truncated frame")
        data.extend(chunk)
    return bytes(data)


def read_frame(sock, deadline):
    size = struct.unpack("!I", recv_exact(sock, 4, deadline))[0]
    if not 0 < size <= MAX_FRAME_BYTES:
        raise ValueError("frame length outside bound")
    try:
        return recv_exact(sock, size, deadline)
    except EOFError as error:
        raise ValueError("truncated frame body") from error


def encode(row):
    body = json.dumps(row, separators=(",", ":"), allow_nan=False).encode("utf-8")
    if not 0 < len(body) <= MAX_FRAME_BYTES:
        raise ValueError("outgoing frame length outside bound")
    return struct.pack("!I", len(body)) + body


def decode(body):
    def reject_noninteger(number):
        raise ValueError(f"noninteger/nonfinite protocol number: {number[:32]}")

    row = json.loads(body, parse_float=reject_noninteger, parse_constant=reject_noninteger)
    if not isinstance(row, dict) or type(row.get("v")) is not int or row["v"] != 1 or not is_u64(row.get("seq")):
        raise ValueError("invalid protocol version/sequence")
    return row


def endpoint(args, server):
    if args.unix:
        return socket.AF_UNIX, args.unix
    address = ipaddress.ip_address(args.bind if server else args.host)
    family = socket.AF_INET6 if address.version == 6 else socket.AF_INET
    return family, (str(address), args.port)


def tune(sock, family):
    if family != socket.AF_UNIX:
        sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)


def guest_identity(error_budget_ns):
    if sys.platform != "linux" or not hasattr(time, "CLOCK_MONOTONIC"):
        raise ValueError("server requires Linux CLOCK_MONOTONIC")
    identity = {"clock": "guest_monotonic", "server_id": uuid.uuid4().hex,
                "machine": socket.gethostname(), "pid": os.getpid(),
                "clock_error_ns": error_budget_ns + math.ceil(time.clock_getres(time.CLOCK_MONOTONIC) * 1e9)}
    try:
        with open("/proc/sys/kernel/random/boot_id", encoding="ascii") as stream:
            identity["boot_id"] = stream.read(128).strip()
        identity["time_namespace"] = os.readlink("/proc/self/ns/time")
    except OSError:
        identity["identity_incomplete"] = True
    return identity


def run_server(args):
    identity = guest_identity(args.clock_error_ns)
    family, address = endpoint(args, True)
    owned_inode = None
    lifetime = time.monotonic() + args.server_timeout
    try:
        with socket.socket(family, socket.SOCK_STREAM) as listener:
            # Never unlink a preexisting Unix socket or reuse somebody else's listener.
            if family == socket.AF_UNIX:
                # Do not let a signal arrive between bind and recording ownership.
                previous_mask = signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGINT, signal.SIGTERM})
                try:
                    listener.bind(address)
                    stat = os.stat(address, follow_symlinks=False)
                    owned_inode = stat.st_dev, stat.st_ino
                finally:
                    signal.pthread_sigmask(signal.SIG_SETMASK, previous_mask)
            else:
                listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                listener.bind(address)
            listener.listen(1)
            print(f"clock-offset server listening on {address!r}", file=sys.stderr, flush=True)
            deadline_timeout(listener, lifetime)
            connection, _ = listener.accept()
            with connection:
                tune(connection, family)
                for expected in range(MAX_SAMPLES):
                    deadline = min(lifetime, time.monotonic() + args.timeout)
                    try:
                        body = read_frame(connection, deadline)
                    except EOFError:
                        return 0
                    recv_ns = time.clock_gettime_ns(time.CLOCK_MONOTONIC)
                    request = decode(body)
                    if request["seq"] != expected:
                        raise ValueError("unexpected request sequence")
                    reply = {"v": 1, "seq": expected, "recv_ns": recv_ns, "guest": identity}
                    reply["send_ns"] = time.clock_gettime_ns(time.CLOCK_MONOTONIC)
                    packet = encode(reply)
                    deadline_timeout(connection, deadline)
                    connection.sendall(packet)
        return 0
    finally:
        if owned_inode is not None:
            try:
                stat = os.stat(address, follow_symlinks=False)
                if (stat.st_dev, stat.st_ino) == owned_inode:
                    os.unlink(address)
            except FileNotFoundError:
                pass


def save_json(path, result):
    """Publish a complete result atomically; never leave a partial JSON document."""
    directory = os.path.dirname(os.path.abspath(path))
    fd, temporary = tempfile.mkstemp(prefix=".clock-offset-", dir=directory)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as stream:
            json.dump(result, stream, indent=2, allow_nan=False)
            stream.write("\n")
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def run_client(args):
    now = host_clock(args.clock_error_ns)
    family, address = endpoint(args, False)
    samples, errors = [], []
    guest = None
    exit_code = 0
    seq = None
    pending = {}
    try:
        with socket.socket(family, socket.SOCK_STREAM) as connection:
            tune(connection, family)
            connection.settimeout(args.timeout)
            connection.connect(address)
            for seq in range(args.samples):
                pending = {"seq": seq}
                packet = encode({"v": 1, "seq": seq})
                deadline = time.monotonic() + args.timeout
                h0, h0_error = now()
                pending.update({"host_send_ns": h0, "host_send_error_ns": h0_error})
                deadline_timeout(connection, deadline)
                connection.sendall(packet)
                body = read_frame(connection, deadline)
                h3, h3_error = now()
                pending.update({"host_recv_ns": h3, "host_recv_error_ns": h3_error})
                if time.monotonic() >= deadline:
                    raise TimeoutError("reply arrived after exchange deadline")
                reply = decode(body)
                identity = reply.get("guest")
                if reply["seq"] != seq or not isinstance(identity, dict) or identity.get("clock") != "guest_monotonic":
                    raise ValueError("mismatched sequence/guest clock")
                if not isinstance(identity.get("server_id"), str) or not is_u64(identity.get("clock_error_ns")):
                    raise ValueError("missing guest identity/error budget")
                if guest is not None and identity != guest:
                    raise ValueError("guest identity changed during calibration")
                guest = identity
                samples.append(make_sample(seq, h0, reply.get("recv_ns"), reply.get("send_ns"), h3,
                                           h0_error, h3_error, identity["clock_error_ns"], args.drift_ppm))
                pending = {}
                if seq + 1 < args.samples:
                    time.sleep(args.interval_ms / 1000)
    except (OSError, ValueError, EOFError, RecursionError, StopRequested) as error:
        errors.append({"seq": seq, **pending, "type": type(error).__name__, "message": str(error)[:1024]})
        exit_code = 128 + error.signum if isinstance(error, StopRequested) else 1

    # Signals cannot interrupt publication after sockets have already been closed.
    previous_mask = signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGINT, signal.SIGTERM})
    try:
        return finish_client(args, samples, errors, guest, exit_code)
    finally:
        signal.pthread_sigmask(signal.SIG_SETMASK, previous_mask)


def finish_client(args, samples, errors, guest, exit_code):
    summary = summarize(samples, args.drift_ppm, math.ceil(args.valid_for * 1e9))
    usable = not errors and summary["consistent"] and summary["valid_samples"] >= MIN_SAMPLES
    result = {"schema": 1, "kind": "clock_offset_calibration", "calibration_id": uuid.uuid4().hex,
              "created_unix_ns": time.time_ns(), "usable": usable,
              "offset_sign": "guest_monotonic_ns minus host_ca_ns",
              "host": {"clock": "host_ca", "implementation": "ctypes QuartzCore.CACurrentMediaTime",
                       "machine": socket.gethostname(), "pid": os.getpid(),
                       "timestamp_error_budget_ns": args.clock_error_ns},
              "guest": guest, "transport": {"unix": args.unix, "host": args.host, "port": args.port},
              "config": {"samples_requested": args.samples, "timeout_s": args.timeout,
                         "interval_ms": args.interval_ms, "valid_for_s": args.valid_for,
                         "max_frame_bytes": MAX_FRAME_BYTES, "max_samples": MAX_SAMPLES},
              "assumptions": ["nonnegative application-boundary path delays; no equal-path-delay assumption",
                              "declared timestamp error and relative drift budgets are assumptions, not accuracy guarantees",
                              "same host/guest boot and time namespace, no suspend/migration/clock discontinuity",
                              "future receipt needs matching clock identity and drift-widened offset within validity window"],
              "summary": summary, "samples": samples, "errors": errors}
    save_json(args.output, result)
    print(f"Saved {len(samples)} samples to {args.output}; valid={summary['valid_samples']} usable={usable}")
    if summary.get("consistent"):
        print(f"Guest-minus-host offset at host_ca={summary['reference_host_ns']}: {summary['offset_interval_ns']} ns; "
              f"midpoint error <= {summary['offset_midpoint_error_ns']} ns (conditional)")
    if summary["rtt_ns"]:
        print(f"Host RTT n={summary['rtt_samples']} " + " ".join(f"{key.upper()}={ns / 1e6:.3f}ms" for key, ns in summary["rtt_ns"].items()))
    if not usable:
        print("Calibration is NOT usable; inspect errors/invalid samples/interval consistency.", file=sys.stderr)
    return exit_code or (0 if usable else 1)


class StaticTests(unittest.TestCase):
    def test_known_asymmetric_delay_signs_and_corrected_bounds(self):
        # True theta=1000; forward=100, guest work=20, reverse=300.
        sample = make_sample(0, 1000, 2100, 2120, 1420)
        self.assertEqual(sample["raw_offset_interval_ns"], [700, 1100])
        self.assertEqual(sample["rtt_ns"], 420)
        self.assertEqual(one_way_bounds(sample, [990, 1010], 1210, 0),
                         {"host_to_guest_ns": [90, 110], "guest_to_host_ns": [290, 310]})
        self.assertEqual(one_way_bounds(sample, [700, 1100], 1210, 0)["host_to_guest_ns"], [0, 400])

    def test_negative_offset(self):
        sample = make_sample(0, 10_000, 9100, 9120, 10_420)
        self.assertEqual(sample["raw_offset_interval_ns"], [-1300, -900])

    def test_invalid_intervals_and_types(self):
        for times in ((100, 200, 220, 110), (110, 200, 220, 100), (100, 220, 200, 130), (True, 200, 220, 130)):
            self.assertIn("invalid", make_sample(0, *times))
        sample = make_sample(0, 100, 200, 220, 110)
        self.assertEqual(summarize([sample], 0, 0)["valid_samples"], 0)

    def test_drift_and_precision_widening(self):
        sample = make_sample(0, 1000, 2100, 2120, 1420, 2, 3, 5)
        self.assertEqual(offset_interval(sample, 1_001_000, 1000), [-308, 2108])
        self.assertEqual(drift_ns(1, .1), 1)

    def test_disjoint_calibration_rejected(self):
        first = make_sample(0, 1000, 2100, 2120, 1420)
        second = make_sample(1, 2000, 6100, 6120, 2420)
        self.assertFalse(summarize([first, second], 0, 0)["consistent"])

    def test_linear_drift_and_asymmetry_remain_inside_bounds(self):
        samples = []
        reference = 1_000_000_000
        true_offset = lambda h: 500_000 + (h - reference) // 10_000  # +100 ppm
        for i in range(100):
            h0 = reference + i * 10_000_000
            forward, work, reverse = 10_000 + i * 37, 20_000, 40_000 - i * 11
            g1 = h0 + forward + true_offset(h0 + forward)
            g2 = h0 + forward + work + true_offset(h0 + forward + work)
            sample = make_sample(i, h0, g1, g2, h0 + forward + work + reverse, 1, 1, 1, 150)
            samples.append(sample)
        summary = summarize(samples, 150, 0)
        lower, upper = summary["offset_interval_ns"]
        self.assertLessEqual(lower, true_offset(summary["reference_host_ns"]))
        self.assertGreaterEqual(upper, true_offset(summary["reference_host_ns"]))
        for i, sample in enumerate(samples):
            forward, reverse = 10_000 + i * 37, 40_000 - i * 11
            f, r = sample["one_way_bounds_ns"]["host_to_guest_ns"], sample["one_way_bounds_ns"]["guest_to_host_ns"]
            self.assertTrue(f[0] <= forward <= f[1])
            self.assertTrue(r[0] <= reverse <= r[1])

    def test_nonfinite_protocol_numbers_rejected(self):
        for number in ("NaN", "Infinity", "1e999", "1.0"):
            with self.assertRaises(ValueError):
                decode(('{"v":1,"seq":0,"recv_ns":' + number + '}').encode())

    def test_summary_records_uncertainty_and_quantiles(self):
        samples = [make_sample(i, 1000 + i * 1000, 2100 + i * 1000, 2120 + i * 1000, 1420 + i * 1000)
                   for i in range(100)]
        summary = summarize(samples, 100, 1_000_000)
        self.assertTrue(summary["consistent"])
        self.assertEqual(summary["rtt_ns"], {"p50": 420, "p95": 420, "p99": 420})
        self.assertGreater(summary["extrapolation_error_ns_at_valid_until"], 0)
        self.assertIn("observed_average_drift_ppm_interval", summary)
        self.assertIn("one_way_bounds_ns", samples[0])
        json.dumps({"summary": summary, "samples": samples}, allow_nan=False)

    def test_framing_is_bounded_without_sockets(self):
        class MemorySocket:
            def __init__(self, data):
                self.data = bytearray(data)

            def settimeout(self, _timeout):
                pass

            def recv(self, size):
                chunk = bytes(self.data[:min(size, 2)])
                del self.data[:len(chunk)]
                return chunk

        deadline = time.monotonic() + 10
        packet = encode({"v": 1, "seq": 7})
        self.assertEqual(decode(read_frame(MemorySocket(packet), deadline))["seq"], 7)
        with self.assertRaises(ValueError):
            read_frame(MemorySocket(struct.pack("!I", MAX_FRAME_BYTES + 1)), deadline)
        with self.assertRaises((ValueError, EOFError)):
            read_frame(MemorySocket(packet[:-1]), deadline)
        with self.assertRaises(TimeoutError):
            read_frame(MemorySocket(packet), time.monotonic() - 1)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--server", action="store_true")
    parser.add_argument("--bind", default="127.0.0.1")
    parser.add_argument("--host")
    parser.add_argument("--port", type=int, default=45871)
    parser.add_argument("--unix")
    parser.add_argument("--samples", type=int, default=200)
    parser.add_argument("--timeout", type=float, default=5)
    parser.add_argument("--server-timeout", type=float, default=120)
    parser.add_argument("--interval-ms", type=float, default=2)
    parser.add_argument("--clock-error-ns", type=int, default=1000)
    parser.add_argument("--drift-ppm", type=float, default=1000)
    parser.add_argument("--valid-for", type=float, default=5)
    parser.add_argument("--output", default="clock-offset.json")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        unittest.main(argv=[sys.argv[0]])
        return 0
    if args.unix and args.host:
        parser.error("--unix and --host are mutually exclusive")
    if not args.server and not args.unix and not args.host:
        parser.error("client requires --host IP or --unix PATH")
    if args.server and args.host:
        parser.error("use --bind, not --host, in server mode")
    if not 1 <= args.port <= 65535 or not MIN_SAMPLES <= args.samples <= MAX_SAMPLES:
        parser.error("port must be 1..65535 and samples 100..10000")
    if not all(math.isfinite(v) and v >= 0 for v in (args.timeout, args.server_timeout, args.interval_ms, args.drift_ppm, args.valid_for)):
        parser.error("time/drift parameters must be finite and nonnegative")
    if args.timeout == 0 or args.server_timeout == 0 or args.drift_ppm >= 1_000_000 or not 0 <= args.clock_error_ns <= 1_000_000_000:
        parser.error("timeouts must be positive; drift < 1000000 ppm; clock error 0..1000000000 ns")
    if args.timeout > 3600 or args.server_timeout > 86400 or args.interval_ms > 60000 or args.valid_for > 86400:
        parser.error("timeout <= 3600s, server lifetime/validity <= 86400s, interval <= 60000ms")

    def interrupted(signum, _frame):
        raise StopRequested(signum)

    previous = {sig: signal.signal(sig, interrupted) for sig in (signal.SIGINT, signal.SIGTERM)}
    try:
        return run_server(args) if args.server else run_client(args)
    except StopRequested as error:
        print(str(error), file=sys.stderr)
        return 128 + error.signum
    except (OSError, ValueError, EOFError, RecursionError) as error:
        print(f"clock calibration failed: {error}", file=sys.stderr)
        return 1
    finally:
        for sig, handler in previous.items():
            signal.signal(sig, handler)


if __name__ == "__main__":
    sys.exit(main())
