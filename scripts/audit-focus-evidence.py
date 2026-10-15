#!/usr/bin/env python3
"""Read-only native-macOS focus evidence; no calibrated Host/Guest latency.

Usage: python3 scripts/audit-focus-evidence.py --host HOST.jsonl
       --native-probe PROBE.jsonl --output NEW.json
       python3 scripts/audit-focus-evidence.py --self-test

Requires an actual native macOS probe (CLOCK_MONOTONIC id 6), never a Linux log.
Unique keyboard protocol serial/opaque-time tuples establish delivery only.
Durations stay strictly inside host_ca or the one native probe's monotonic clock.
Bounds: 64 MiB/250,000 records per input, 16 KiB/line, 50,000 input groups.
Malformed input/capacity overflow aborts all metrics instead of selecting a prefix.
"""
import argparse
from collections import Counter, defaultdict
import importlib.util
import json
from pathlib import Path
import sys
import unittest

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("audit_stage", Path(__file__).with_name("audit-stage-latency.py"))
stage = importlib.util.module_from_spec(spec)
spec.loader.exec_module(stage)


def load(path):
    for line in stage.limited([path]):
        if line.strip():
            row = json.loads(line)
            if not isinstance(row, dict) or type(row.get("schema")) is not int or row["schema"] != 1:
                raise ValueError("invalid evidence schema")
            yield row


def tuple_key(row, host=False):
    fields = ("source_event_serial", "source_event_time") if host else ("protocol_serial", "forwarded_event_time_ms_opaque")
    pair = tuple(row.get(field) for field in fields)
    return pair if all(type(value) is int and 0 < value < 2**32 for value in pair) else None


def metric(values, clock, meaning):
    return {"n": len(values), "clock": clock, "meaning": meaning,
        "milliseconds": {name: stage.latency.percentile(values, q) for name, q in
            (("p50", .5), ("p95", .95), ("p99", .99))} if values else None}


def summarize(host, probe):
    host_groups, native_groups = {}, {}
    sessions, starts, stops = set(), [], []
    excluded = Counter()
    def append(groups, key, label, row):
        if key not in groups:
            if len(groups) >= 50_000:
                raise ValueError("input group capacity exceeded; no metrics emitted")
            groups[key] = defaultdict(list)
        if len(groups[key][label]) < 2:
            groups[key][label].append(row)
    for row in host:
        if row.get("kind") != "event" or row.get("stage") not in ("HostNativeInput", "Forwarded"):
            continue
        session, correlation = row.get("session_id"), row.get("correlation_id")
        if not stage.latency.is_tag(session) or not stage.latency.is_tag(correlation):
            excluded["host_missing_explicit_identity"] += 1
            continue
        sessions.add(session)
        append(host_groups, (session, stage.latency.tag_key(correlation)), row["stage"], row)
    if len(sessions) != 1:
        raise ValueError("exactly one host input session required")
    for row in probe:
        if row.get("record") == "start":
            starts.append(row)
        elif row.get("record") == "stop":
            stops.append(row)
        elif row.get("event") == "key" and row.get("record") in ("input_received", "input_handled"):
            sequence = row.get("event_sequence")
            if not stage.latency.is_u64(sequence):
                raise ValueError("invalid native event sequence")
            append(native_groups, sequence, row["record"], row)
    if len(starts) != 1 or len(stops) != 1 or starts[0].get("clock") != "CLOCK_MONOTONIC" or starts[0].get("clock_id") != 6:
        raise ValueError("one complete actual native-macOS CLOCK_MONOTONIC id6 run required")
    identity = tuple(starts[0].get(field) for field in ("pid", "tag", "probe"))
    if stops[0].get("pid") != identity[0]:
        raise ValueError("native probe identity changed")
    native_by_tuple, host_by_tuple = defaultdict(list), defaultdict(list)
    for group in native_groups.values():
        if any(len(group[label]) != 1 for label in ("input_received", "input_handled")):
            excluded["native_missing_or_duplicate_stage"] += 1
            continue
        receipt, handled = group["input_received"][0], group["input_handled"][0]
        pair = tuple_key(receipt)
        if pair is None or tuple_key(handled) != pair:
            excluded["native_missing_or_conflicting_tuple"] += 1
            continue
        if any(tuple(row.get(field) for field in ("pid", "tag", "probe")) != identity for row in (receipt, handled)):
            raise ValueError("native probe identity changed")
        fields = ("seat_global", "evdev_code", "raw_state")
        if any(receipt.get(field) != handled.get(field) for field in fields):
            raise ValueError("native handler metadata conflicts with receipt")
        native_by_tuple[pair].append((receipt, handled))
    for group in host_groups.values():
        if any(len(group[label]) != 1 for label in ("HostNativeInput", "Forwarded")):
            excluded["host_missing_or_duplicate_stage"] += 1
            continue
        observed, forwarded = group["HostNativeInput"][0], group["Forwarded"][0]
        pair = tuple_key(forwarded, host=True)
        if pair is None:
            excluded["legacy_or_nonkeyboard_untagged_host_completion"] += 1
            continue
        host_by_tuple[pair].append((observed, forwarded))
    dispatch, handler = [], []
    joined = 0
    for pair in host_by_tuple.keys() | native_by_tuple.keys():
        if len(host_by_tuple[pair]) != 1 or len(native_by_tuple[pair]) != 1:
            excluded["unmatched_or_duplicate_protocol_tuple"] += 1
            continue
        observed, forwarded = host_by_tuple[pair][0]
        receipt, handled = native_by_tuple[pair][0]
        clocks = (observed.get("clock"), forwarded.get("clock"))
        windows = {stage.latency.tag_key(row["window_id"]) for row in (observed, forwarded) if row.get("window_id") is not None}
        host_times = [row.get("timestamp_ns") for row in (observed, forwarded)]
        native_times = [receipt.get("guest_monotonic_ns"), handled.get("handler_start_ns"), handled.get("guest_monotonic_ns")]
        if clocks != ("host_ca", "host_ca") or len(windows) > 1:
            excluded["host_clock_or_window_conflict"] += 1
            continue
        if not all(stage.latency.is_u64(value) for value in host_times + native_times) or host_times != sorted(host_times) or native_times != sorted(native_times):
            excluded["invalid_or_reversed_within_clock_time"] += 1
            continue
        if handled.get("receipt_ns") != native_times[0]:
            excluded["native_receipt_reference_conflict"] += 1
            continue
        dispatch.append(host_times[1] - host_times[0])
        handler.append(native_times[-1] - native_times[0])
        joined += 1
    return {"schema": 1, "kind": "actual_native_macos_keyboard_delivery", "joined_key_events": joined,
        "native_final_record": stops[0], "excluded": dict(excluded),
        "host_observation_to_tagged_keyboard_dispatch": metric(dispatch, "host_ca", "CPU keyboard-dispatch observation, NOT receipt or presentation"),
        "native_probe_receipt_to_handler_completion": metric(handler, "native_macos_CLOCK_MONOTONIC_id6", "Backend delivery to native probe handler completion, NOT Linux Guest"),
        "cross_clock_subtraction_performed": False, "host_to_probe_receipt_latency": None,
        "actual_presentation_latency": None, "five_ms_guest_goal_assessed": False,
        "limitations": ["Only unique explicit correlation/keyboard tuples; no proximity joins.",
            "No clock calibration or Host/Guest subtraction; native record field names do not make this Linux.",
            "Small retained sample, synchronous trace overhead, incomplete host final stats can bias tails."]}


class Tests(unittest.TestCase):
    def fixture(self):
        host = [json.loads(stage.latency.AnalysisTests.event(name, stamp, source_event_serial=7, source_event_time=9))
            for name, stamp in (("HostNativeInput", 100), ("Forwarded", 120))]
        common = {"schema": 1, "pid": 5, "tag": "test", "probe": "native"}
        event = dict(common, event="key", event_sequence=1, protocol_serial=7, forwarded_event_time_ms_opaque=9,
            seat_global=2, evdev_code=30, raw_state=1)
        probe = [dict(common, record="start", clock="CLOCK_MONOTONIC", clock_id=6),
            dict(event, record="input_received", guest_monotonic_ns=9000),
            dict(event, record="input_handled", guest_monotonic_ns=9050, handler_start_ns=9010, receipt_ns=9000),
            dict(common, record="stop")]
        return host, probe
    def test_clocks_remain_separate(self):
        result = summarize(*self.fixture())
        self.assertEqual(result["joined_key_events"], 1)
        self.assertEqual(result["host_observation_to_tagged_keyboard_dispatch"]["milliseconds"]["p50"], .00002)
        self.assertEqual(result["native_probe_receipt_to_handler_completion"]["milliseconds"]["p50"], .00005)
        self.assertIsNone(result["host_to_probe_receipt_latency"])
    def test_legacy_or_duplicate_host_not_joined(self):
        host, probe = self.fixture()
        self.assertEqual(summarize(host + [host[1]], probe)["joined_key_events"], 0)
        host[1]["source_event_serial"] = None
        self.assertEqual(summarize(host, probe)["joined_key_events"], 0)
    def test_duplicate_native_or_cross_clock_not_joined(self):
        host, probe = self.fixture()
        self.assertEqual(summarize(host, probe + [probe[1]])["joined_key_events"], 0)
        host[1]["clock"] = "guest_monotonic"
        self.assertEqual(summarize(host, probe)["joined_key_events"], 0)
    def test_linux_probe_rejected(self):
        host, probe = self.fixture()
        probe[0]["clock_id"] = 1
        with self.assertRaises(ValueError):
            summarize(host, probe)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--host", type=Path)
    parser.add_argument("--native-probe", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return int(not unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromTestCase(Tests)).wasSuccessful())
    if not args.host or not args.native_probe or not args.output:
        parser.error("--host HOST.jsonl --native-probe PROBE.jsonl --output NEW.json required")
    report = summarize(load(args.host), load(args.native_probe))
    with args.output.open("x") as stream:
        json.dump(report, stream, indent=2, allow_nan=False)
        stream.write("\n")
    print(json.dumps({"joined_key_events": report["joined_key_events"], "report": str(args.output)}))
    return 0


if __name__ == "__main__":
    sys.exit(main())
