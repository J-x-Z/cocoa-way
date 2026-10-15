#!/usr/bin/env python3
"""Analyze explicit keyboard input/probe joins, not timestamp-nearest causality.

python3 scripts/analyze-input-probe.py --host HOST.jsonl --guest GUEST.jsonl \
    --clock CLOCK.json --guest-clock-verified [--tag TAG] [--session SESSION] [--json]
python3 scripts/analyze-input-probe.py --self-test

--guest-clock-verified is the caller's assertion that calibration and probe use
the SAME Guest CLOCK_MONOTONIC epoch/boot/time namespace and the same Host boot,
without suspend/migration/discontinuity. Server/probe PIDs need not match. Missing
identity metadata is disclosed, never silently treated as proof. Calibration's
drift/error budgets remain assumptions, not a measured accuracy guarantee.

HostNativeInput -> Forwarded uses an explicit session/correlation ID. Forwarded
-> Guest input_received uses a UNIQUE (protocol serial, opaque forwarded time)
tuple, Guest event == "key" only. Decimal u32 source tags may be strings; opaque
time is NEVER interpreted as a clock. Guest runs must have a unique start record.

For theta = Guest - Host, intersect ALL raw sample constraints transported to H.
An exact rational prefix/suffix index costs O(n log n) setup, O(log n) per event,
and O(n) bounded memory. Never extrapolate only the aggregate reference interval.
The receipt residual is G-H-[U,L], widened by timestamp error. Divide bounds by
(1 +/- relative drift) to cover clock-rate change DURING the unknown latency;
do not merely subtract an offset midpoint. Require the whole resulting receipt
bracket to remain inside conditional validity. Quantile bounds are nearest-rank
quantiles of lower/upper endpoints, not quantiles of midpoint estimates.

CWIP v1 is exactly the 120-byte little-endian RGB-decoded payload documented in
test-client/src/bin/input-latency-probe.rs. Validate PID/frame/seat/sequence and
serial/time/code/state/receipt/handler/pre-commit against actual Guest records.
Reject bit3, applied count != 1, or non-singleton cause ranges for INPUT metrics.
Exact frame-marker joins still allow pre-commit -> Host stage bounds for coalesced
frames, without assigning them to an input. Pre-commit includes marker writing
and queueing; Host arrival/compose/submission are NOT actual presentation.
Host frame stages
must be unique and explicitly correlated; repeated commits/markers are ambiguous.
Only actual MetalPresented host_ca timestamps yield presentation durations.

Winit observation != device/hardware arrival; Guest receipt is backend delivery.
CoreAnimation actual presentation != photons. Synchronous Guest stderr writes
can block and perturb later input/handler/render timing; no overhead correction
is claimed. Incomplete Host final stats imply unknown loss totals. No target
achievement or hardware input-to-photon conclusion follows from these reports.

Read-only, stdlib only. Hard cap 250,000 input/calibration records, bounded lines
and calibration bytes. Capacity/malformed structured records abort ALL metrics:
never truncate a prefix in a way that could conceal a later duplicate.
"""

import argparse
from bisect import bisect_right
from collections import Counter, defaultdict
from fractions import Fraction
import io
import json
import math
import re
import struct
import sys
import unittest


MAX_RECORDS = 250_000
MAX_LINE_BYTES = 16_384
MAX_CLOCK_BYTES = 16 * 1024 * 1024
PROBE = "cocoa-way.input-latency-probe"
STAGES = ("HostNativeInput", "Forwarded", "SurfaceCommit", "ComposeStarted", "CPUSubmitted", "MetalPresented")
METAL_STAGES = ("DrawableAcquireStarted", "DrawableAcquireFinished", "ComposeFinished",
                "HostInputHandled",
                "GPUStarted", "GPUCompleted", "GPUCompletionCallback", "GPUCommandFailed",
                "MetalPresentedCallback", "FrameCallbackDone")
HOST_FIELDS = set("schema kind session_id clock queue_capacity dropped rejected final stage timestamp_ns correlation_id source_event_serial source_event_time window_id probe_marker".split())
GUEST_FIELDS = set(("schema probe tag pid record guest_monotonic_ns event event_sequence seat_global protocol_serial "
                    "forwarded_event_time_ms_opaque evdev_key evdev_code raw_state handler_start_ns receipt_ns "
                    "frame_sequence render_start_ns pre_commit_ns commit_queued_ns has_new_input_cause cause_kind "
                    "cause_event_sequence cause_forwarded_event_time_ms_opaque cause_evdev_code cause_raw_state "
                    "cause_protocol_serial cause_seat_global cause_receipt_ns cause_handler_ns applied_visual_events "
                    "first_applied_event_sequence last_applied_event_sequence width height continuous_animation "
                    "clock clock_id clock_units clock_epoch receipt_point boot_id time_namespace observed_input_events "
                    "key_events frames_committed uncommitted_visual_events reason").split())


def uint(value, bits=64):
    return type(value) is int and 0 <= value < (1 << bits)


def u32_tag(value):
    if type(value) is str and re.fullmatch(r"0|[1-9][0-9]{0,9}", value):
        value = int(value)
    return value if uint(value, 32) else None


def id_key(value):
    if uint(value) or (type(value) is str and 0 < len(value.encode()) <= 256):
        return type(value).__name__, value
    return None


def buckets(rows, key):
    result = defaultdict(list)
    for row in rows:
        identity = key(row)
        if identity is not None:
            result[identity].append(row)
    return result


def one(index, key):
    rows = index.get(key, [])
    return rows[0] if len(rows) == 1 else None


class Budget:
    def __init__(self, maximum=MAX_RECORDS):
        if not 1 <= maximum <= MAX_RECORDS:
            raise ValueError("record limit must be 1..250000")
        self.maximum, self.seen = maximum, 0

    def take(self, count=1):
        self.seen += count
        if self.seen > self.maximum:
            raise ValueError("record capacity exceeded; no metrics emitted (unread suffix may contain duplicates)")


def read_rows(stream, fields, budget, guest=False):
    rows, diagnostics = [], Counter()
    while True:
        line = stream.readline(MAX_LINE_BYTES + 1)
        if not line:
            break
        if not line.strip():
            continue
        budget.take()
        if len(line) > MAX_LINE_BYTES:
            raise ValueError("oversized JSONL/stderr line; no metrics emitted")
        if guest and not line.lstrip().startswith(b"{"):
            diagnostics["non_json_stderr_lines"] += 1
            if line.strip() == b"cocoa-way container relay: using stdio compatibility transport":
                diagnostics["transport_stdio_log_records"] += 1
            if line.strip() == b"cocoa-way container relay: using publish-socket transport v2":
                diagnostics["transport_publish_socket_v2_log_records"] += 1
            if line.strip() == b"cocoa-way transport v2: host channel connected":
                diagnostics["transport_v2_connected_log_records"] += 1
            continue
        row = json.loads(line)
        if not isinstance(row, dict) or type(row.get("schema")) is not int or row["schema"] != 1:
            raise ValueError("unsupported structured record/schema")
        compact = {}
        for key in fields & row.keys():
            value = row[key]
            if key == "probe_marker" and value is not None:
                value = bytes(value) if isinstance(value, list) and len(value) == 120 and all(uint(x, 8) for x in value) else b""
            elif isinstance(value, (dict, list, float)) or (isinstance(value, str) and len(value.encode()) > 256):
                raise ValueError("non-scalar/oversized record tag")
            compact[key] = value
        rows.append(compact)
    return rows, diagnostics


def transport_evidence(diagnostics):
    stdio = diagnostics["transport_stdio_log_records"] > 0
    v2 = diagnostics["transport_publish_socket_v2_log_records"] > 0
    return "mixed_or_ambiguous" if stdio and v2 else "stdio" if stdio else "publish_socket_v2" if v2 else "unknown"


def load_clock(path, budget):
    with open(path, "rb") as stream:
        raw = stream.read(MAX_CLOCK_BYTES + 1)
    if len(raw) > MAX_CLOCK_BYTES:
        raise ValueError("calibration byte capacity exceeded")
    clock = json.loads(raw)
    if not isinstance(clock, dict) or not isinstance(clock.get("samples"), list) or len(clock["samples"]) > 10_000:
        raise ValueError("invalid/beyond-capacity calibration samples")
    budget.take(1 + len(clock["samples"]))
    return clock


class RawOffsetIndex:
    """Exact envelopes of l_i-r|H-h3_i| and u_i+r|H-h0_i|, rounded outward."""

    def __init__(self, samples, rate):
        self.n, self.d = rate.numerator, rate.denominator
        self.lower = self.envelope(samples, False)
        self.upper = self.envelope(samples, True)

    def envelope(self, samples, upper):
        n, d = self.n, self.d
        points = []
        for s in samples:
            t = s["host_send_ns"] if upper else s["host_recv_ns"]
            error = s["host_send_error_ns"] if upper else s["host_recv_error_ns"]
            base = (s["guest_recv_ns"] - t + error + s["guest_error_ns"] if upper
                    else s["guest_send_ns"] - t - error - s["guest_error_ns"])
            sign = 1 if upper else -1
            points.append((t, base * d - sign * n * t + sign * n * error,
                           base * d + sign * n * t + sign * n * error))
        points.sort()
        times, left, right = map(list, zip(*points))
        extreme = min if upper else max
        for i in range(1, len(left)):
            left[i] = extreme(left[i-1], left[i])
        for i in range(len(right)-2, -1, -1):
            right[i] = extreme(right[i+1], right[i])
        return times, left, right

    def endpoint(self, h, error, upper):
        times, left, right = self.upper if upper else self.lower
        pos, n, d = bisect_right(times, h), self.n, self.d
        sign = 1 if upper else -1
        candidates = []
        if pos:
            candidates.append(left[pos-1] + sign * n * h)
        if pos < len(times):
            candidates.append(right[pos] - sign * n * h)
        value = (min(candidates) if upper else max(candidates)) + sign * n * error
        return -(-value // d) if upper else value // d

    def bounds(self, h, error=0):
        low, high = self.endpoint(h, error, False), self.endpoint(h, error, True)
        if low > high:
            raise ValueError("raw calibration constraints inconsistent at event time")
        return [low, high]


def calibration(clock, verified):
    if verified is not True:
        raise ValueError("--guest-clock-verified required: explicit same-container/boot/normal-clock proof")
    summary, host, guest = clock.get("summary"), clock.get("host"), clock.get("guest")
    if not all(isinstance(x, dict) for x in (summary, host, guest)):
        raise ValueError("invalid calibration schema/clock metadata")
    if (type(clock.get("schema")) is not int or clock.get("schema") != 1 or clock.get("kind") != "clock_offset_calibration"
            or clock.get("usable") is not True or type(clock.get("errors")) is not list or clock["errors"]
            or host.get("clock") != "host_ca" or guest.get("clock") != "guest_monotonic"
            or clock.get("offset_sign") != "guest_monotonic_ns minus host_ca_ns"
            or summary.get("consistent") is not True or not uint(summary.get("valid_samples"))
            or summary["valid_samples"] < 100):
        raise ValueError("unusable/wrong-clock calibration")
    interval, validity = summary.get("offset_interval_ns"), summary.get("conditional_valid_host_interval_ns")
    if (not isinstance(interval, list) or len(interval) != 2 or not all(type(x) is int for x in interval)
            or interval[0] > interval[1] or not isinstance(validity, list) or len(validity) != 2
            or not all(uint(x) for x in validity) or not uint(summary.get("reference_host_ns"))
            or not validity[0] <= summary["reference_host_ns"] <= validity[1]):
        raise ValueError("invalid calibration interval/reference/validity")
    ppm = summary.get("assumed_max_abs_offset_drift_ppm")
    if type(ppm) not in (int, float) or not math.isfinite(ppm) or not 0 <= ppm < 1_000_000:
        raise ValueError("invalid relative drift budget")
    host_error, guest_error = host.get("timestamp_error_budget_ns"), guest.get("clock_error_ns")
    if (not uint(host_error) or not uint(guest_error) or id_key(guest.get("server_id")) is None
            or type(guest.get("server_id")) is not str):
        raise ValueError("missing timestamp error budgets or stable Guest server identity")
    samples = clock.get("samples")
    if not isinstance(samples, list) or not 100 <= len(samples) <= 10_000:
        raise ValueError("need 100..10000 bounded raw calibration samples")
    valid, previous = [], None
    identities = 0
    for seq, sample in enumerate(samples):
        fields = ("seq", "host_send_ns", "host_recv_ns", "guest_recv_ns", "guest_send_ns",
                  "host_send_error_ns", "host_recv_error_ns", "guest_error_ns")
        if (not isinstance(sample, dict) or not all(uint(sample.get(f)) for f in fields)
                or sample["seq"] != seq or ("schema" in sample and type(sample["schema"]) is not int)
                or sample.get("schema", 1) != 1):
            raise ValueError("invalid raw sample schema/sequence/timestamp/error")
        h0, h3, g1, g2 = (sample[f] for f in fields[1:5])
        if (h3 < h0 or g2 < g1 or (previous and (h0 < previous[0] or g1 < previous[1]))):
            raise ValueError("raw samples reversed/overlapping: clock discontinuity or wrong sample order")
        previous = h3, g2
        if sample["guest_error_ns"] != guest_error:
            raise ValueError("raw sample Guest error budget/identity mismatch")
        if "guest" in sample:
            if sample["guest"] != guest:
                raise ValueError("raw sample Guest identity changed")
            identities += 1
        for f in ("server_id", "boot_id", "time_namespace"):
            if f in sample and (f not in guest or sample[f] != guest[f]):
                raise ValueError("raw sample Guest identity mismatch: " + f)
        raw = [g2-h3, g1-h0]
        if (type(sample.get("rtt_ns")) is not int or sample["rtt_ns"] != h3-h0
                or type(sample.get("guest_processing_ns")) is not int or sample["guest_processing_ns"] != g2-g1
                or sample.get("raw_offset_interval_ns") != raw
                or not all(type(x) is int for x in sample["raw_offset_interval_ns"])):
            raise ValueError("raw sample derived RTT/processing/offset mismatch")
        host_error = max(host_error, sample["host_send_error_ns"], sample["host_recv_error_ns"])
        if raw[0] > raw[1]:
            if sample.get("invalid") != "invalid raw offset interval (guest processing exceeds host RTT)":
                raise ValueError("invalid raw interval not explicitly excluded")
            continue
        if ("invalid" in sample or not uint(sample.get("anchor_host_ns"))
                or sample["anchor_host_ns"] != (h0+h3)//2):
            raise ValueError("raw sample invalid flag/anchor mismatch")
        valid.append(sample)
    invalid_count = summary.get("invalid_samples", len(samples)-len(valid))
    if (len(valid) < 100 or summary["valid_samples"] != len(valid) or not uint(invalid_count)
            or invalid_count != len(samples)-len(valid)):
        raise ValueError("raw sample counts disagree with calibration summary")
    measurement = [valid[0]["host_send_ns"], valid[-1]["host_recv_ns"]]
    if (summary.get("measurement_host_interval_ns", measurement) != measurement
            or validity[0] > measurement[0] or validity[1] < measurement[1]):
        raise ValueError("raw sample measurement/validity window mismatch")
    index = RawOffsetIndex(valid, Fraction(str(ppm)) / 1_000_000)
    # Envelope feasibility can change only at raw timestamp knots. A reference
    # intersection alone can conceal a clock jump near one end of a long run.
    for sample in valid:
        index.bounds(sample["host_send_ns"])
        index.bounds(sample["host_recv_ns"])
    raw_reference = index.bounds(summary["reference_host_ns"])
    return {"reference": summary["reference_host_ns"], "offset": interval, "validity": validity,
            "rate": Fraction(str(ppm)) / 1_000_000, "host_error": host_error, "guest_error": guest_error,
            "raw_index": index, "raw_reference": raw_reference, "raw_valid_samples": len(valid),
            "raw_invalid_samples": len(samples)-len(valid), "raw_identity_records": identities,
            "measurement": measurement}


def ceil_fraction(value):
    return -(-value.numerator // value.denominator)


def receipt_bounds(h, g, cal):
    if not uint(h) or not uint(g):
        raise ValueError("invalid input/receipt timestamp")
    start, end = cal["validity"]
    eh, eg, rate = cal["host_error"], cal["guest_error"], cal["rate"]
    if h - eh < start or h + eh > end:
        raise ValueError("input outside calibration validity")
    lower, upper = cal["raw_index"].bounds(h, eh)
    residual_lower, residual_upper = g - h - upper - eh - eg, g - h - lower + eh + eg
    if residual_upper < 0:
        raise ValueError("receipt precedes input under all permitted offsets")
    # Host-time latency also bounds rate change between input and Guest receipt.
    low = max(0, (Fraction(residual_lower) / (1 + rate)).__floor__())
    high = ceil_fraction(Fraction(residual_upper) / (1 - rate))
    if h + eh + high > end:
        raise ValueError("receipt uncertainty extends beyond calibration validity")
    return [low, high]


def guest_to_host_bounds(g, h, cal):
    """Reverse direction: theta is transported to the Host DESTINATION time."""
    if not uint(g) or not uint(h):
        raise ValueError("invalid pre-commit/Host destination timestamp")
    start, end = cal["validity"]
    eh, eg, rate = cal["host_error"], cal["guest_error"], cal["rate"]
    if h-eh < start or h+eh > end:
        raise ValueError("Host destination outside calibration validity")
    lower, upper = cal["raw_index"].bounds(h, eh)
    residual_lower, residual_upper = h-g+lower-eh-eg, h-g+upper+eh+eg
    if residual_upper < 0:
        raise ValueError("Host destination precedes pre-commit under all permitted offsets")
    low = max(0, (Fraction(residual_lower) / (1+rate)).__floor__())
    high = ceil_fraction(Fraction(residual_upper) / (1-rate))
    if h-eh-high < start:
        raise ValueError("pre-commit uncertainty extends before calibration validity")
    return [low, high]


def marker(payload):
    if not isinstance(payload, bytes) or len(payload) != 120 or payload[:4] != b"CWIP":
        raise ValueError("invalid marker bytes/magic")
    if struct.unpack_from("<HH", payload, 4) != (1, 120):
        raise ValueError("invalid marker version/length")
    result = {}
    for offset, name in [(8,"flags"),(12,"kind"),(32,"time"),(36,"code"),(40,"state"),(44,"serial"),
                         (96,"width"),(100,"height"),(104,"format"),(108,"pid"),(112,"seat"),(116,"reserved")]:
        result[name] = struct.unpack_from("<I", payload, offset)[0]
    for offset, name in [(16,"frame"),(24,"sequence"),(48,"receipt"),(56,"handled"),(64,"pre_commit"),
                         (72,"applied"),(80,"first"),(88,"last")]:
        result[name] = struct.unpack_from("<Q", payload, offset)[0]
    if result["flags"] & ~15 or result["format"] != 0 or result["reserved"] != 0 or not result["frame"]:
        raise ValueError("invalid marker flags/format/reserved/frame")
    return result


MARKER_FIELDS = {
    "frame":"frame_sequence", "kind":"cause_kind", "sequence":"cause_event_sequence",
    "code":"cause_evdev_code", "state":"cause_raw_state", "serial":"cause_protocol_serial",
    "seat":"cause_seat_global", "receipt":"cause_receipt_ns", "handled":"cause_handler_ns",
    "pre_commit":"pre_commit_ns", "applied":"applied_visual_events", "first":"first_applied_event_sequence",
    "last":"last_applied_event_sequence", "width":"width", "height":"height", "pid":"pid",
}


def marker_matches(m, commit):
    flags = (int(commit.get("has_new_input_cause") is True)
             | (int(commit.get("continuous_animation") is True) << 1)
             | (int(commit.get("cause_forwarded_event_time_ms_opaque") is not None) << 2)
             | (int(commit.get("applied_visual_events", 0) > 1) << 3))
    return (m["flags"] == flags and all(m[k] == commit.get(v) for k, v in MARKER_FIELDS.items())
            and m["time"] == (commit.get("cause_forwarded_event_time_ms_opaque") or 0))


def single_cause(commit):
    sequence = commit.get("cause_event_sequence")
    return (commit.get("has_new_input_cause") is True and commit.get("cause_kind") == 1
            and uint(sequence) and sequence > 0 and commit.get("applied_visual_events") == 1
            and commit.get("first_applied_event_sequence") == sequence == commit.get("last_applied_event_sequence"))


def source_pair(row, host=False):
    names = ("source_event_serial", "source_event_time") if host else ("protocol_serial", "forwarded_event_time_ms_opaque")
    pair = tuple(u32_tag(row.get(name)) for name in names)
    return pair if None not in pair else None


def same_key(a, b):
    fields = ("pid", "event_sequence", "seat_global", "protocol_serial", "forwarded_event_time_ms_opaque", "evdev_key", "raw_state")
    return all(a.get(k) == b.get(k) for k in fields)


def validate_key(row):
    if (not uint(row.get("event_sequence")) or not row["event_sequence"]
            or not all(uint(row.get(k),32) for k in ("pid","seat_global","protocol_serial","forwarded_event_time_ms_opaque","evdev_key","raw_state"))
            or (row.get("evdev_code") is not None and row["evdev_code"] != row["evdev_key"])):
        raise ValueError("invalid Guest keyboard identity/source tuple")


def validate_commit(row):
    small = ("pid","cause_kind","cause_evdev_code","cause_raw_state","cause_protocol_serial","cause_seat_global","width","height")
    large = ("frame_sequence","pre_commit_ns","commit_queued_ns","cause_event_sequence","cause_receipt_ns","cause_handler_ns",
             "applied_visual_events","first_applied_event_sequence","last_applied_event_sequence")
    if (not all(uint(row.get(k),32) for k in small) or not all(uint(row.get(k)) for k in large)
            or not row["frame_sequence"] or not row["width"] or not row["height"]
            or type(row.get("has_new_input_cause")) is not bool or type(row.get("continuous_animation")) is not bool
            or (row.get("cause_forwarded_event_time_ms_opaque") is not None and not uint(row["cause_forwarded_event_time_ms_opaque"],32))):
        raise ValueError("invalid Guest commit identity/cause fields")


def cause_matches(commit, receipt, handled):
    pairs = {"cause_event_sequence":"event_sequence", "cause_seat_global":"seat_global",
             "cause_protocol_serial":"protocol_serial", "cause_forwarded_event_time_ms_opaque":"forwarded_event_time_ms_opaque",
             "cause_evdev_code":"evdev_key", "cause_raw_state":"raw_state", "cause_receipt_ns":"guest_monotonic_ns"}
    return (all(commit.get(k) == receipt.get(v) for k, v in pairs.items())
            and commit.get("cause_handler_ns") == handled.get("guest_monotonic_ns"))


def metric(values, bounds=False):
    if not values:
        return {"n": 0, "quantiles_ns": None, "bounded": bounds}
    def quantile(a, p):
        return sorted(a)[math.ceil(len(a) * p) - 1]
    quantiles = {}
    for name, p in (("p50", .50), ("p95", .95), ("p99", .99)):
        quantiles[name] = ([quantile([v[0] for v in values], p), quantile([v[1] for v in values], p)]
                           if bounds else quantile(values, p))
    return {"n": len(values), "quantiles_ns": quantiles, "bounded": bounds}


def analyze(host, guest, clock, verified=False, tag=None, session=None):
    cal = calibration(clock, verified)
    failures = Counter()
    guest_errors = sum(r.get("probe") == PROBE and r.get("record") == "error" for r in guest)
    starts = [r for r in guest if r.get("probe") == PROBE and r.get("record") == "start" and (tag is None or r.get("tag") == tag)]
    if len(starts) != 1:
        raise ValueError("need exactly one Guest start; select --tag for one run")
    start = starts[0]
    if (start.get("probe") != PROBE or start.get("clock") != "CLOCK_MONOTONIC" or start.get("clock_id") != 1
            or start.get("clock_units") != "ns" or start.get("clock_epoch") != "guest_system_monotonic"
            or start.get("receipt_point") != "backend_event_delivery"
            or not uint(start.get("pid"),32) or not start["pid"] or type(start.get("tag")) is not str):
        raise ValueError("wrong Guest probe/receipt clock")
    for field in ("boot_id", "time_namespace"):
        if start.get(field) is not None and clock["guest"].get(field) is not None and start[field] != clock["guest"][field]:
            raise ValueError("Guest clock identity mismatch: " + field)
    run = start.get("tag"), start.get("pid")
    guest = [r for r in guest if (r.get("tag"), r.get("pid")) == run and r.get("probe") == PROBE]
    sessions = {r.get("session_id") for r in host}
    if session is None:
        if len(sessions) != 1:
            raise ValueError("need exactly one Host session; select --session")
        session = next(iter(sessions))
    if session not in sessions:
        raise ValueError("requested Host session absent")
    host = [r for r in host if r.get("session_id") == session]
    events = [r for r in host if r.get("kind") == "event"]
    for r in events:
        if r.get("clock") != "host_ca" or r.get("stage") not in STAGES + METAL_STAGES or not uint(r.get("timestamp_ns")):
            raise ValueError("wrong Host stage/timestamp/clock")
        if r["stage"] in ("MetalPresented", "GPUStarted", "GPUCompleted") and r["timestamp_ns"] == 0:
            raise ValueError("zero is not a reported API timestamp")
    all_receipts = [r for r in guest if r.get("record") == "input_received"]
    inputs = [r for r in all_receipts if r.get("event") == "key"]
    handlers = [r for r in guest if r.get("record") == "input_handled" and r.get("event") == "key"]
    commits = [r for r in guest if r.get("record") == "frame_commit"]
    for r in inputs + handlers + commits:
        if not uint(r.get("guest_monotonic_ns")):
            raise ValueError("invalid Guest monotonic timestamp")
    for r in inputs + handlers:
        validate_key(r)
    for r in commits:
        validate_commit(r)
    by_pair = buckets(inputs, source_pair)
    for r in all_receipts:
        if not uint(r.get("event_sequence")) or not r["event_sequence"]:
            raise ValueError("invalid Guest receipt event sequence")
    by_sequence = buckets(all_receipts, lambda r: r.get("event_sequence"))
    handled = buckets(handlers, lambda r: r.get("event_sequence"))
    guest_frames = buckets(commits, lambda r: r.get("frame_sequence"))
    native = buckets([r for r in events if r["stage"] == "HostNativeInput"], lambda r: id_key(r.get("correlation_id")))
    forwards = [r for r in events if r["stage"] == "Forwarded"]
    forwarded_ids = buckets(forwards, lambda r: id_key(r.get("correlation_id")))
    forwarded_pairs = buckets(forwards, lambda r: source_pair(r, True))
    frames = buckets([r for r in events if r["stage"] in STAGES[2:]], lambda r: id_key(r.get("correlation_id")))
    host_markers, frame_markers = defaultdict(list), defaultdict(list)
    tainted, frame_tainted = set(), set()
    marker_count = 0
    for r in events:
        if r["stage"] != "SurfaceCommit" or r.get("probe_marker") is None:
            continue
        marker_count += 1
        try:
            m = marker(r["probe_marker"])
        except ValueError:
            failures["invalid_marker"] += 1
            continue
        if m["pid"] != run[1]:
            failures["marker_other_pid"] += 1
            continue
        commit = one(guest_frames, m["frame"])
        if commit is None or not marker_matches(m, commit):
            failures["marker_commit_missing_duplicate_or_mismatch"] += 1
            tainted.add(m["frame"])
            frame_tainted.add(m["frame"])
            continue
        frame_markers[m["frame"]].append(r)
        if m["kind"] == 1 and (m["flags"] & 8 or m["applied"] != 1 or m["first"] != m["last"]):
            failures["coalesced_key_marker"] += 1
            tainted.add(m["frame"])
            continue
        host_markers[m["frame"]].append(r)

    durations, frame_bounds = defaultdict(list), defaultdict(list)
    frame_names = {
        "SurfaceCommit": "guest_pre_commit_to_host_surface_observed_frame",
        "ComposeStarted": "guest_pre_commit_to_host_compose_started_frame",
        "CPUSubmitted": "guest_pre_commit_to_host_cpu_submitted_frame",
    }
    matched_frames = 0
    for frame, rows in guest_frames.items():
        surface = one(frame_markers, frame)
        if len(rows) != 1 or surface is None or frame in frame_tainted:
            failures["frame_marker_missing_duplicate_or_tainted"] += len(rows)
            continue
        commit = rows[0]
        if (commit["guest_monotonic_ns"] != commit["pre_commit_ns"]
                or commit["pre_commit_ns"] > commit["commit_queued_ns"]):
            failures["frame_pre_commit_timestamps_invalid"] += 1
            continue
        matched_frames += 1
        identity = id_key(surface.get("correlation_id"))
        chain = buckets(frames.get(identity, []), lambda r: r["stage"])
        prefix = []
        for stage, name in frame_names.items():
            destination = surface if stage == "SurfaceCommit" else one(chain, stage)
            if stage != "SurfaceCommit" and (identity is None or one(chain, "SurfaceCommit") is not surface
                                             or destination is None):
                failures["frame_stage_missing_duplicate_or_coalesced: " + stage] += 1
                break
            prefix.append(destination)
            times = [r["timestamp_ns"] for r in prefix]
            windows = {id_key(r["window_id"]) for r in prefix if r.get("window_id") is not None}
            if len(windows) > 1 or any(b < a for a,b in zip(times,times[1:])):
                failures["frame_stage_window_mismatch_or_reversed: " + stage] += 1
                break
            try:
                frame_bounds[name].append(guest_to_host_bounds(commit["pre_commit_ns"], destination["timestamp_ns"], cal))
            except ValueError as error:
                failures["frame_calibration_excluded: " + stage + ": " + str(error)] += 1
    good_handlers = {}
    for pair, rows in by_pair.items():
        if len(rows) != 1:
            failures["duplicate_guest_key_tuple"] += len(rows)
            continue
        receipt = rows[0]
        if len(by_sequence[receipt["event_sequence"]]) != 1:
            failures["duplicate_guest_event_sequence"] += 1
            continue
        handler = one(handled, receipt.get("event_sequence"))
        if (handler is None or not same_key(receipt, handler) or handler.get("receipt_ns") != receipt["guest_monotonic_ns"]
                or not uint(handler.get("handler_start_ns"))
                or not receipt["guest_monotonic_ns"] <= handler["handler_start_ns"] <= handler["guest_monotonic_ns"]):
            failures["handler_missing_duplicate_or_mismatch"] += 1
            continue
        good_handlers[receipt["event_sequence"]] = handler
        durations["guest_receipt_to_handled"].append(handler["guest_monotonic_ns"] - receipt["guest_monotonic_ns"])
        durations["guest_handler_execution"].append(handler["guest_monotonic_ns"] - handler["handler_start_ns"])

    joined = {}
    latency_bounds = []
    for pair, rows in forwarded_pairs.items():
        receipt = one(by_pair, pair)
        if len(rows) != 1 or receipt is None:
            failures["forward_receipt_tuple_missing_or_duplicate"] += len(rows)
            continue
        forward = rows[0]
        if len(by_sequence[receipt["event_sequence"]]) != 1:
            failures["join_duplicate_guest_event_sequence"] += 1
            continue
        identity = id_key(forward.get("correlation_id"))
        observation = one(native, identity)
        if observation is None or one(forwarded_ids, identity) is None or observation["timestamp_ns"] > forward["timestamp_ns"]:
            failures["native_forward_id_missing_duplicate_or_reversed"] += 1
            continue
        joined[receipt["event_sequence"]] = observation, forward, receipt
        durations["host_observation_to_forwarded"].append(forward["timestamp_ns"] - observation["timestamp_ns"])
        try:
            latency_bounds.append(receipt_bounds(observation["timestamp_ns"], receipt["guest_monotonic_ns"], cal))
        except ValueError as error:
            failures["calibration_excluded: " + str(error)] += 1

    cause_commits = defaultdict(list)
    coalesced = 0
    for frame, rows in guest_frames.items():
        if len(rows) != 1:
            failures["duplicate_guest_frame"] += len(rows)
            continue
        commit = rows[0]
        if commit.get("cause_kind") != 1 or commit.get("has_new_input_cause") is not True:
            continue
        if not single_cause(commit):
            coalesced += 1
            continue
        if frame in tainted:
            failures["single_cause_frame_tainted_by_marker"] += 1
            continue
        receipt = one(by_sequence, commit.get("cause_event_sequence"))
        handler = good_handlers.get(commit.get("cause_event_sequence"))
        if receipt is None or handler is None or not cause_matches(commit, receipt, handler):
            failures["commit_cause_receipt_handler_mismatch"] += 1
            continue
        if (commit.get("guest_monotonic_ns") != commit.get("pre_commit_ns") or not uint(commit.get("commit_queued_ns"))
                or not handler["guest_monotonic_ns"] <= commit["pre_commit_ns"] <= commit["commit_queued_ns"]):
            failures["guest_commit_timestamps_reversed_or_invalid"] += 1
            continue
        cause_commits[receipt["event_sequence"]].append(commit)

    single_cause_keys = 0
    for sequence, rows in cause_commits.items():
        if len(rows) != 1:
            failures["duplicate_single_cause_commit"] += len(rows)
            continue
        commit = rows[0]
        single_cause_keys += 1
        handler = good_handlers[sequence]
        durations["guest_handled_to_pre_commit_single_cause"].append(commit["pre_commit_ns"] - handler["guest_monotonic_ns"])
        durations["guest_handled_to_commit_queued_single_cause"].append(commit["commit_queued_ns"] - handler["guest_monotonic_ns"])
        surface = one(host_markers, commit["frame_sequence"])
        if surface is None or sequence not in joined:
            failures["presentation_missing_duplicate_marker_or_input_join"] += 1
            continue
        chain = buckets(frames.get(id_key(surface.get("correlation_id")), []), lambda r: r["stage"])
        ordered = [one(chain, stage) for stage in STAGES[2:]]
        if any(r is None for r in ordered) or ordered[0] is not surface:
            failures["presentation_frame_stages_missing_or_duplicate"] += 1
            continue
        observation, forward, _ = joined[sequence]
        times = [observation["timestamp_ns"], forward["timestamp_ns"]] + [r["timestamp_ns"] for r in ordered]
        windows = {id_key(r.get("window_id")) for r in [observation, forward] + ordered if r.get("window_id") is not None}
        if len(windows) > 1 or any(b < a for a, b in zip(times, times[1:])):
            failures["presentation_window_mismatch_or_reversed"] += 1
            continue
        durations["host_observation_to_actual_metal_presented_single_cause"].append(times[-1] - times[0])

    stats = [r for r in host if r.get("kind") == "stats"]
    for r in stats:
        if not uint(r.get("dropped")) or not uint(r.get("rejected")) or type(r.get("final")) is not bool:
            raise ValueError("invalid Host loss stats")
    stops = [r for r in guest if r.get("record") == "stop"]
    complete_guest = len(stops) == 1 and stops[0].get("key_events") == len(inputs)
    stage_counts = Counter(r["stage"] for r in events)
    metrics = {name: metric(durations[name]) for name in (
        "host_observation_to_forwarded", "guest_receipt_to_handled", "guest_handler_execution",
        "guest_handled_to_pre_commit_single_cause", "guest_handled_to_commit_queued_single_cause",
        "host_observation_to_actual_metal_presented_single_cause")}
    metrics["host_observation_to_guest_receipt"] = metric(latency_bounds, True)
    for name in frame_names.values():
        metrics[name] = metric(frame_bounds[name], True)
    return {
        "schema": 1, "kind": "input_probe_analysis",
        "metadata": {"guest_clock_verified": verified, "verification": "explicit caller assertion, not inferred from PID/hostname",
                     "transport": "unknown", "transport_evidence": "exact Guest relay log, never inferred from filename or configuration",
                     "calibration_id": clock.get("calibration_id"), "calibration_guest": clock["guest"],
                     "calibration_reference_host_ns": cal["reference"], "calibration_offset_ns": cal["offset"],
                     "offset_method": "all validated raw intervals transported to each event, exact rational O(log n) envelope",
                     "raw_offset_at_reference_ns": cal["raw_reference"],
                     "raw_reference_matches_saved_summary": cal["raw_reference"] == cal["offset"],
                     "raw_valid_samples": cal["raw_valid_samples"], "raw_invalid_samples": cal["raw_invalid_samples"],
                     "raw_sample_guest_identity_records": cal["raw_identity_records"],
                     "raw_identity_proof": "sidecar enforces stable peer identity live; old samples omit per-sample identity; caller verifies probe epoch",
                     "calibration_measurement_host_interval_ns": cal["measurement"],
                     "calibration_valid_host_interval_ns": cal["validity"],
                     "assumed_timestamp_error_ns": {"host":cal["host_error"],"guest":cal["guest_error"]},
                     "max_input_reference_age_ns": max((abs(h["timestamp_ns"]-cal["reference"]) for h,_,_ in joined.values()),default=None),
                     "rate_correction_during_unknown_receipt_delay": True,
                     "rate_correction_during_unknown_frame_delay": True,
                     "assumed_drift_ppm": clock["summary"]["assumed_max_abs_offset_drift_ppm"],
                     "host_session": session, "guest_tag": run[0], "guest_pid": run[1],
                     "host_boundary": "Winit observation, NOT device/hardware arrival",
                     "guest_boundary": "backend event delivery, NOT kernel/hardware arrival",
                     "presentation_boundary": "actual CoreAnimation/Metal presentedTime, NOT photons",
                     "frame_bounds_boundary": "Guest pre-commit marker timestamp to Host observed/compose/CPU; includes marker writing/queueing; NOT actual presentation or per-input causality",
                     "instrumentation": "Guest synchronous stderr formatting/writes may block and perturb timing; no overhead correction"},
        "counts": {"host_stages": {stage: stage_counts[stage] for stage in STAGES}, "guest_input_receipts":len(all_receipts),
                   "guest_key_receipts": len(inputs), "guest_key_handlers": len(handlers),
                   "tagged_host_forwards": sum(len(v) for v in forwarded_pairs.values()),
                   "host_forwards_without_serial_time": len(forwards) - sum(len(v) for v in forwarded_pairs.values()),
                   "host_native_without_keyboard_tuple_join": stage_counts["HostNativeInput"] - len(joined),
                   "unique_joined_keys": len(joined), "receipt_bounds_samples": len(latency_bounds),
                   "unmatched_tagged_host_forwards": sum(len(v) for v in forwarded_pairs.values()) - len(joined),
                   "unmatched_guest_key_receipts": len(inputs) - len(joined), "guest_coalesced_key_frames": coalesced,
                   "host_marker_records": marker_count, "single_cause_keys": single_cause_keys,
                   "unique_marker_matched_guest_frames": matched_frames,
                   "guest_frames_without_unique_verified_host_marker": len(commits)-matched_frames,
                   "guest_frames_without_stage_bounds": {stage: len(commits)-len(frame_bounds[name])
                                                         for stage,name in frame_names.items()},
                   "keys_without_verified_single_cause_commit": len(inputs) - single_cause_keys,
                   "keys_without_actual_presentation_metric": len(inputs) - metrics["host_observation_to_actual_metal_presented_single_cause"]["n"],
                   "host_frames_without_metal_presented": sum(1 for rows in frames.values() if not any(r["stage"] == "MetalPresented" for r in rows))},
        "loss": {"host_final_stats_complete": bool(stats) and stats[-1]["final"],
                 "observed_host_dropped": max((r["dropped"] for r in stats), default=None),
                 "observed_host_rejected": max((r["rejected"] for r in stats), default=None),
                 "host_totals_unknown_without_final": not stats or not stats[-1]["final"],
                 "guest_stop_records": len(stops), "guest_stop_key_count_consistent": complete_guest,
                 "guest_stop_frame_count_consistent": len(stops) == 1 and stops[0].get("frames_committed") == len(commits),
                 "guest_stop_total_input_count_consistent": len(stops) == 1 and stops[0].get("observed_input_events") == len(all_receipts),
                 "guest_uncommitted_visual_events_at_stop": stops[0].get("uncommitted_visual_events") if len(stops)==1 else None,
                 "guest_probe_error_records_in_file": guest_errors, "guest_drop_accounting": "not provided by probe"},
        "excluded": dict(failures), "metrics": metrics,
    }


def report(result):
    m, c, loss = result["metadata"], result["counts"], result["loss"]
    print(f"Guest clock VERIFIED BY CALLER={m['guest_clock_verified']}; tag={m['guest_tag']} pid={m['guest_pid']}")
    print(f"TRANSPORT={m['transport']} (Guest relay log evidence, not configuration)")
    print(f"Calibration Guest identity incomplete={m['calibration_guest'].get('identity_incomplete', False)}; drift bound ASSUMED={m['assumed_drift_ppm']} ppm")
    print("Winit observation, not device hardware; Guest backend delivery; actual Metal presentation, not photons.")
    print("All raw calibration constraints intersected per event (not summary extrapolation); declared drift/error assumptions unchanged.")
    print("Frame bounds: Guest pre-commit marker timestamp to Host arrival/compose/CPU, NOT actual presentation or per-input causality.")
    print("Guest stderr instrumentation may BLOCK/perturb timing; no overhead correction or target-achievement claim.")
    print("COUNTS " + json.dumps(c, sort_keys=True))
    print("LOSS " + json.dumps(loss, sort_keys=True))
    if loss["host_totals_unknown_without_final"]:
        print("Host final stats incomplete: observed drops/rejections are lower bounds; total loss UNKNOWN.")
    for name, metric_row in result["metrics"].items():
        quantiles = metric_row["quantiles_ns"]
        if quantiles is None:
            print(f"{name}: n=0; NO VERIFIED SAMPLES")
            continue
        formatted = []
        for q, value in quantiles.items():
            text = f"[{value[0]/1e6:.6f}, {value[1]/1e6:.6f}]" if isinstance(value, list) else f"{value/1e6:.6f}"
            formatted.append(f"{q.upper()}={text} ms")
        label = "BOUNDS (not midpoint estimates)" if metric_row.get("bounded") else "same-clock recorded duration"
        print(f"{name}: n={metric_row['n']} {label}; " + "; ".join(formatted))
    print("EXCLUDED " + json.dumps(result["excluded"], sort_keys=True))
    print("Missing/duplicate/coalesced samples and trace/instrumentation loss can bias these distributions.")


class StaticTests(unittest.TestCase):
    @staticmethod
    def fixture():
        clock = {"schema":1, "kind":"clock_offset_calibration", "usable":True, "errors":[],
                 "offset_sign":"guest_monotonic_ns minus host_ca_ns", "calibration_id":"test",
                 "host":{"clock":"host_ca","timestamp_error_budget_ns":0},
                 "guest":{"clock":"guest_monotonic","clock_error_ns":0,"pid":6,"boot_id":"boot","server_id":"test-server"},
                 "summary":{"consistent":True,"valid_samples":100,"reference_host_ns":1000,
                            "offset_interval_ns":[990,1010],"conditional_valid_host_interval_ns":[500,6000],
                            "assumed_max_abs_offset_drift_ppm":0}, "samples":[]}
        for seq in range(100):
            h0 = 500+seq*40
            clock["samples"].append({"seq":seq,"host_send_ns":h0,"host_recv_ns":h0+30,
                "guest_recv_ns":h0+1010,"guest_send_ns":h0+1020,"host_send_error_ns":0,
                "host_recv_error_ns":0,"guest_error_ns":0,"rtt_ns":30,"guest_processing_ns":10,
                "raw_offset_interval_ns":[990,1010],"anchor_host_ns":h0+15})
        def h(stage, ns, identity, **extra):
            return {"kind":"event","session_id":"s","stage":stage,"clock":"host_ca","timestamp_ns":ns,
                    "correlation_id":identity, **extra}
        def g(record, ns, **extra):
            return {"probe":PROBE,"record":record,"tag":"test","pid":3,"guest_monotonic_ns":ns, **extra}
        key = {"event":"key","event_sequence":1,"seat_global":5,"protocol_serial":7,
               "forwarded_event_time_ms_opaque":20,"evdev_key":30,"raw_state":1}
        commit = g("frame_commit",2200,frame_sequence=1,pre_commit_ns=2200,commit_queued_ns=2210,
                   has_new_input_cause=True,cause_kind=1,cause_event_sequence=1,cause_forwarded_event_time_ms_opaque=20,
                   cause_evdev_code=30,cause_raw_state=1,cause_protocol_serial=7,cause_seat_global=5,
                   cause_receipt_ns=2110,cause_handler_ns=2120,applied_visual_events=1,
                   first_applied_event_sequence=1,last_applied_event_sequence=1,width=64,height=64,continuous_animation=False)
        payload = bytearray(120); payload[:4] = b"CWIP"; struct.pack_into("<HH",payload,4,1,120)
        for offset,value in [(8,5),(12,1),(32,20),(36,30),(40,1),(44,7),(96,64),(100,64),(108,3),(112,5)]:
            struct.pack_into("<I",payload,offset,value)
        for offset,value in [(16,1),(24,1),(48,2110),(56,2120),(64,2200),(72,1),(80,1),(88,1)]:
            struct.pack_into("<Q",payload,offset,value)
        host = [h("HostNativeInput",1000,1),h("Forwarded",1010,1,source_event_serial=7,source_event_time=20),
                h("SurfaceCommit",1250,2,probe_marker=bytes(payload)),h("ComposeStarted",1260,2),
                h("CPUSubmitted",1270,2),h("MetalPresented",1300,2)]
        guest = [g("start",2000,clock="CLOCK_MONOTONIC",clock_id=1,clock_units="ns",clock_epoch="guest_system_monotonic",receipt_point="backend_event_delivery"),
                 g("input_received",2110,**key),g("input_handled",2120,**key,receipt_ns=2110,handler_start_ns=2115),commit]
        return host, guest, clock

    def test_known_offset_and_exact_single_cause_presentation(self):
        h,g,c = self.fixture(); r = analyze(h,g,c,True)
        self.assertEqual(r["metrics"]["host_observation_to_guest_receipt"]["quantiles_ns"]["p50"],[100,120])
        self.assertEqual(r["metrics"]["host_observation_to_actual_metal_presented_single_cause"]["quantiles_ns"]["p99"],300)
        self.assertEqual(r["metrics"]["guest_handled_to_pre_commit_single_cause"]["quantiles_ns"]["p50"],80)
        self.assertEqual(r["metrics"]["guest_pre_commit_to_host_surface_observed_frame"]["quantiles_ns"]["p50"],[40,60])
        self.assertEqual(r["metrics"]["guest_pre_commit_to_host_compose_started_frame"]["quantiles_ns"]["p50"],[50,70])
        self.assertEqual(r["metrics"]["guest_pre_commit_to_host_cpu_submitted_frame"]["quantiles_ns"]["p50"],[60,80])
        self.assertFalse(r["loss"]["host_final_stats_complete"])

    def test_auxiliary_metal_events_do_not_extend_the_input_causal_chain(self):
        h,g,c = self.fixture()
        h.extend({"kind":"event", "session_id":"s", "stage":stage, "clock":"host_ca",
                  "timestamp_ns":ns, "correlation_id":2}
                 for stage, ns in [("DrawableAcquireStarted",1261),
                                   ("DrawableAcquireFinished",1262),
                                   ("GPUCompletionCallback",1290),
                                   ("MetalPresentedCallback",1310)])
        r = analyze(h,g,c,True)
        self.assertEqual(r["metrics"]["host_observation_to_actual_metal_presented_single_cause"]["quantiles_ns"]["p99"],300)
        h[:] = [row for row in h if row.get("stage") != "MetalPresented"]
        r = analyze(h,g,c,True)
        self.assertEqual(r["metrics"]["host_observation_to_actual_metal_presented_single_cause"]["n"],0)
        self.assertIsNone(r["metrics"]["host_observation_to_actual_metal_presented_single_cause"]["quantiles_ns"])

    def test_explicit_clock_gate_and_boot_mismatch(self):
        h,g,c = self.fixture()
        with self.assertRaises(ValueError): analyze(h,g,c)
        g[0]["boot_id"] = "other"
        with self.assertRaises(ValueError): analyze(h,g,c,True)

    def test_signed_offset_and_numeric_source_tags(self):
        h,g,c = self.fixture(); c["summary"]["offset_interval_ns"] = [-1010,-990]
        c["summary"].update(reference_host_ns=3000,conditional_valid_host_interval_ns=[2500,8000])
        for s in c["samples"]:
            for field in ("host_send_ns","host_recv_ns","anchor_host_ns"):
                s[field] += 2000
            s["raw_offset_interval_ns"] = [-1010,-990]
        cal = calibration(c,True)
        self.assertEqual(receipt_bounds(3000,2110,cal),[100,120])
        self.assertEqual(guest_to_host_bounds(3200,4250,cal),[40,60])
        h,g,c = self.fixture(); h[1].update(source_event_serial="7",source_event_time="20")
        self.assertEqual(analyze(h,g,c,True)["counts"]["unique_joined_keys"],1)
        h[0]["correlation_id"] = "1"
        self.assertEqual(analyze(h,g,c,True)["counts"]["unique_joined_keys"],0)

    def test_duplicate_sequence_and_native_id_not_selected(self):
        h,g,c = self.fixture(); duplicate=dict(g[1]); duplicate["protocol_serial"]=8; g.append(duplicate)
        self.assertEqual(analyze(h,g,c,True)["counts"]["unique_joined_keys"],0)
        h,g,c = self.fixture(); h.append(dict(h[0]))
        self.assertEqual(analyze(h,g,c,True)["counts"]["unique_joined_keys"],0)
        h,g,c = self.fixture(); g[1]["raw_state"] = True
        with self.assertRaises(ValueError): analyze(h,g,c,True)

    def test_tuple_mismatch_and_duplicate_receipts_are_not_guessed(self):
        h,g,c = self.fixture(); g[1]["protocol_serial"] = 8
        self.assertEqual(analyze(h,g,c,True)["counts"]["unique_joined_keys"],0)
        h,g,c = self.fixture(); g.append(dict(g[1]))
        self.assertEqual(analyze(h,g,c,True)["counts"]["unique_joined_keys"],0)
        h,g,c = self.fixture(); h.append(dict(h[1]))
        self.assertEqual(analyze(h,g,c,True)["counts"]["unique_joined_keys"],0)

    def test_marker_mismatch_coalescing_and_missing_metal(self):
        for offset,value,fmt in [(44,8,"I"),(8,13,"I"),(80,0,"Q")]:
            h,g,c = self.fixture(); b=bytearray(h[2]["probe_marker"]); struct.pack_into("<"+fmt,b,offset,value)
            h[2]["probe_marker"] = bytes(b)
            self.assertEqual(analyze(h,g,c,True)["metrics"]["host_observation_to_actual_metal_presented_single_cause"]["n"],0)
        h,g,c = self.fixture(); h.pop()
        self.assertEqual(analyze(h,g,c,True)["metrics"]["host_observation_to_actual_metal_presented_single_cause"]["n"],0)
        h,g,c = self.fixture(); h.append(dict(h[-1]))
        self.assertEqual(analyze(h,g,c,True)["metrics"]["host_observation_to_actual_metal_presented_single_cause"]["n"],0)

    def test_drift_transported_at_input_and_over_unknown_delay(self):
        _,_,c = self.fixture(); c["summary"]["assumed_max_abs_offset_drift_ppm"] = 1000
        cal=calibration(c,True)
        self.assertEqual(receipt_bounds(1500,2610,cal),[99,122])
        with self.assertRaises(ValueError): receipt_bounds(499,1600,cal)
        with self.assertRaises(ValueError): receipt_bounds(5990,7100,cal)
        with self.assertRaises(ValueError): guest_to_host_bounds(1400,500,cal)
        with self.assertRaises(ValueError): guest_to_host_bounds(9000,1000,cal)

    def test_raw_envelope_matches_all_constraints_not_summary(self):
        _,_,c = self.fixture(); rate = Fraction(1000,1_000_000)
        for i,s in enumerate(c["samples"]):
            s["host_send_error_ns"], s["host_recv_error_ns"] = i%7, i%11
        index = RawOffsetIndex(c["samples"],rate)
        for h in (0,499,500,515,530,531,1000,1500,4490,54_000_000_000):
            for error in (0,5):
                lower, upper = [], []
                for s in c["samples"]:
                    es, er, eg = s["host_send_error_ns"], s["host_recv_error_ns"], s["guest_error_ns"]
                    lower.append(s["guest_send_ns"]-s["host_recv_ns"]-er-eg
                                 -ceil_fraction(rate*(abs(h-s["host_recv_ns"])+er+error)))
                    upper.append(s["guest_recv_ns"]-s["host_send_ns"]+es+eg
                                 +ceil_fraction(rate*(abs(h-s["host_send_ns"])+es+error)))
                self.assertEqual(index.bounds(h,error),[max(lower),min(upper)])
        _,_,c = self.fixture(); c["summary"]["offset_interval_ns"] = [-999_999,999_999]
        self.assertEqual(receipt_bounds(1000,2110,calibration(c,True)),[100,120])
        distant = index.bounds(54_000_000_000)
        self.assertGreater(distant[1]-distant[0],100_000_000)
        # Widely separated samples: a contemporaneous sample must retain its
        # tight bound rather than acquire drift from an aggregate reference.
        samples = [dict(s) for s in c["samples"]]
        for i,s in enumerate(samples):
            delta = i*1_000_000_000
            for f in ("host_send_ns","host_recv_ns","guest_recv_ns","guest_send_ns"):
                s[f] += delta
        self.assertEqual(RawOffsetIndex(samples,rate).bounds(samples[0]["host_send_ns"]),[989,1010])

    def test_raw_sample_schema_monotonicity_and_identity(self):
        for field,value in (("seq",0),("host_send_ns",499),("guest_recv_ns",0),
                            ("rtt_ns",99),("raw_offset_interval_ns",[991,1010]),
                            ("anchor_host_ns",0),("guest_error_ns",1),("server_id","other"),
                            ("guest",{"clock":"guest_monotonic","boot_id":"other"}),("schema",True)):
            _,_,c = self.fixture(); c["samples"][1][field] = value
            with self.subTest(field=field), self.assertRaises(ValueError): calibration(c,True)
        _,_,c = self.fixture(); c["summary"]["valid_samples"] = 101
        with self.assertRaises(ValueError): calibration(c,True)
        _,_,c = self.fixture(); c["samples"] = []
        with self.assertRaises(ValueError): calibration(c,True)

    def test_clock_jump_is_not_hidden_by_consistent_reference_summary(self):
        _,_,c = self.fixture()
        c["summary"].update(assumed_max_abs_offset_drift_ppm=1000,offset_interval_ns=[-999999,999999])
        for s in c["samples"][50:]:
            s["guest_recv_ns"] += 4000; s["guest_send_ns"] += 4000
            s["raw_offset_interval_ns"] = [4990,5010]
        with self.assertRaises(ValueError): calibration(c,True)

    def test_frame_bounds_survive_missing_present_and_input_coalescing(self):
        h,g,c = self.fixture(); h.pop()
        r = analyze(h,g,c,True)
        self.assertEqual(r["metrics"]["guest_pre_commit_to_host_cpu_submitted_frame"]["n"],1)
        self.assertEqual(r["metrics"]["host_observation_to_actual_metal_presented_single_cause"]["n"],0)
        g[-1].update(applied_visual_events=2,first_applied_event_sequence=0)
        b = bytearray(h[2]["probe_marker"])
        struct.pack_into("<I",b,8,13); struct.pack_into("<Q",b,72,2); struct.pack_into("<Q",b,80,0)
        h[2]["probe_marker"] = bytes(b)
        r = analyze(h,g,c,True)
        self.assertEqual(r["counts"]["single_cause_keys"],0)
        self.assertEqual(r["metrics"]["guest_pre_commit_to_host_cpu_submitted_frame"]["n"],1)
        duplicate = dict(h[2]); duplicate.pop("probe_marker"); h.append(duplicate)
        r = analyze(h,g,c,True)
        self.assertEqual(r["metrics"]["guest_pre_commit_to_host_surface_observed_frame"]["n"],1)
        self.assertEqual(r["metrics"]["guest_pre_commit_to_host_compose_started_frame"]["n"],0)
        self.assertEqual(r["metrics"]["guest_pre_commit_to_host_cpu_submitted_frame"]["n"],0)
        h.append(dict(h[2])); r = analyze(h,g,c,True)
        self.assertEqual(r["metrics"]["guest_pre_commit_to_host_surface_observed_frame"]["n"],0)

    def test_multiple_distinct_commits_in_one_compose_id_are_not_chosen(self):
        h,g,c = self.fixture()
        commit = dict(g[-1]); commit["frame_sequence"] = 2; g.append(commit)
        surface = dict(h[2]); surface["timestamp_ns"] += 5
        b = bytearray(surface["probe_marker"]); struct.pack_into("<Q",b,16,2)
        surface["probe_marker"] = bytes(b); h.append(surface)
        r = analyze(h,g,c,True)
        self.assertEqual(r["metrics"]["guest_pre_commit_to_host_surface_observed_frame"]["n"],2)
        self.assertEqual(r["metrics"]["guest_pre_commit_to_host_compose_started_frame"]["n"],0)
        self.assertEqual(r["metrics"]["guest_pre_commit_to_host_cpu_submitted_frame"]["n"],0)
        self.assertEqual(r["metrics"]["host_observation_to_actual_metal_presented_single_cause"]["n"],0)

    def test_frame_prefix_reversal_window_and_stage_duplicates(self):
        for mutation in ("reversed", "window", "duplicate"):
            h,g,c = self.fixture()
            if mutation == "reversed": h[4]["timestamp_ns"] = 1240
            elif mutation == "window":
                h[3]["window_id"] = 1; h[4]["window_id"] = 2
            else: h.append(dict(h[4]))
            r = analyze(h,g,c,True)
            self.assertEqual(r["metrics"]["guest_pre_commit_to_host_compose_started_frame"]["n"],1)
            self.assertEqual(r["metrics"]["guest_pre_commit_to_host_cpu_submitted_frame"]["n"],0)

    def test_bounded_reader_and_quantile_endpoint_bounds(self):
        line=b'{"schema":1,"kind":"stats"}\n'
        with self.assertRaises(ValueError): read_rows(io.BytesIO(line*3),HOST_FIELDS,Budget(2))
        rows,diag=read_rows(io.BytesIO(b"relay startup\n"),GUEST_FIELDS,Budget(2),True)
        self.assertEqual(rows,[]); self.assertEqual(diag["non_json_stderr_lines"],1)
        self.assertEqual(metric([[0,20],[5,30],[10,40]],True)["quantiles_ns"]["p50"],[5,30])

    def test_transport_is_logged_not_inferred_from_v2_failure(self):
        fallback = (b"cocoa-way transport v2 startup failed: permission denied; falling back to stdio relay\n"
                    b"cocoa-way container relay: using stdio compatibility transport\n")
        published = b"cocoa-way container relay: using publish-socket transport v2\n"
        for raw, expected in ((fallback,"stdio"),(published,"publish_socket_v2"),
                              (fallback+published,"mixed_or_ambiguous"),(b"v2 requested\n","unknown")):
            _,diag = read_rows(io.BytesIO(raw),GUEST_FIELDS,Budget(10),True)
            self.assertEqual(transport_evidence(diag),expected)


def main():
    parser=argparse.ArgumentParser(description=__doc__,formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--host"); parser.add_argument("--guest"); parser.add_argument("--clock")
    parser.add_argument("--guest-clock-verified",action="store_true")
    parser.add_argument("--tag"); parser.add_argument("--session")
    parser.add_argument("--json",action="store_true"); parser.add_argument("--self-test",action="store_true")
    parser.add_argument("--max-records",type=int,default=MAX_RECORDS)
    args=parser.parse_args()
    if args.self_test:
        unittest.main(argv=[sys.argv[0]])
        return 0
    if not all((args.host,args.guest,args.clock)):
        parser.error("--host, --guest and --clock are required")
    try:
        budget=Budget(args.max_records); clock=load_clock(args.clock,budget)
        with open(args.host,"rb") as stream: host,_=read_rows(stream,HOST_FIELDS,budget)
        with open(args.guest,"rb") as stream: guest,diag=read_rows(stream,GUEST_FIELDS,budget,True)
        result=analyze(host,guest,clock,args.guest_clock_verified,args.tag,args.session)
        result["metadata"].update({"record_limit":budget.maximum,"records_read":budget.seen,
                                  "transport":transport_evidence(diag),
                                  "input_paths":{"host":args.host,"guest":args.guest,"clock":args.clock}})
        result["counts"].update(diag)
        if args.json: print(json.dumps(result,indent=2,allow_nan=False))
        else: report(result)
        return 0
    except (OSError,ValueError,TypeError,KeyError,RecursionError) as error:
        print(f"input-probe analysis refused: {error}",file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
