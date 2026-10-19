#!/usr/bin/env python3
"""Explicit input -> probe marker -> HOST stage joins, without clock calibration.

All reported spans use two host_ca timestamps. Probe receipt/commit data validates
causality only; its CLOCK_MONOTONIC timestamp is never subtracted from host time.
Works for native or proxied probes, not evidence of Linux receipt or photons.
"""
import argparse
from collections import Counter, defaultdict
import importlib.util
import json
from pathlib import Path
import sys
import unittest

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("input_probe", Path(__file__).with_name("analyze-input-probe.py"))
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)


def summarize(host, guest):
    sessions = {row.get("session_id") for row in host}
    starts = [row for row in guest if row.get("record") == "start" and row.get("probe") == probe.PROBE]
    if len(sessions) != 1 or len(starts) != 1:
        raise ValueError("exactly one host session and one probe run required")
    run = starts[0]["pid"], starts[0]["tag"]
    guest = [r for r in guest if (r.get("pid"), r.get("tag")) == run and r.get("probe") == probe.PROBE]
    events = [row for row in host if row.get("kind") == "event"]
    if any(row.get("clock") != "host_ca" or not probe.uint(row.get("timestamp_ns")) for row in events):
        raise ValueError("all host observations must have host_ca timestamps")
    groups = probe.buckets(events, lambda row: probe.id_key(row.get("correlation_id")))
    forwards = probe.buckets([r for r in events if r.get("stage") == "Forwarded"], lambda r: probe.source_pair(r, True))
    keys = [r for r in guest if r.get("record") == "input_received" and r.get("event") == "key"]
    handlers = [r for r in guest if r.get("record") == "input_handled" and r.get("event") == "key"]
    commits = [r for r in guest if r.get("record") == "frame_commit"]
    for row in keys + handlers:
        probe.validate_key(row)
    for row in commits:
        probe.validate_commit(row)
    receipts = probe.buckets(keys, probe.source_pair)
    sequences = probe.buckets(keys, lambda r: r["event_sequence"])
    handled = probe.buckets(handlers, lambda r: r["event_sequence"])
    frames = probe.buckets(commits, lambda r: r["frame_sequence"])
    markers = defaultdict(list)
    excluded = Counter()
    for row in events:
        if row.get("stage") != "SurfaceCommit" or row.get("probe_marker") is None:
            continue
        marker = probe.marker(row["probe_marker"])
        if marker["pid"] == run[0]:
            markers[marker["frame"]].append((row, marker))
    values = defaultdict(list)
    endpoints = ("SurfaceCommit", "ComposeStarted", "CPUSubmitted", "MetalPresentedCallback", "MetalPresented")
    for frame, rows in frames.items():
        if len(rows) != 1 or len(markers[frame]) != 1:
            excluded["missing_or_duplicate_frame_marker"] += 1
            continue
        commit, (surface, marker) = rows[0], markers[frame][0]
        if not probe.marker_matches(marker, commit):
            excluded["marker_mismatch"] += 1
            continue
        if not probe.single_cause(commit):
            excluded["not_a_single_keyboard_cause"] += 1
            continue
        pair = commit.get("cause_protocol_serial"), commit.get("cause_forwarded_event_time_ms_opaque")
        receipt, forward = probe.one(receipts, pair), probe.one(forwards, pair)
        handler = probe.one(handled, commit.get("cause_event_sequence"))
        if (receipt is None or forward is None or handler is None
                or len(sequences[receipt["event_sequence"]]) != 1
                or not probe.same_key(receipt, handler)
                or handler.get("receipt_ns") != receipt.get("guest_monotonic_ns")
                or not receipt["guest_monotonic_ns"] <= handler["guest_monotonic_ns"] <= commit["pre_commit_ns"] <= commit["commit_queued_ns"]
                or not probe.cause_matches(commit, receipt, handler)):
            excluded["receipt_forward_or_handler_not_unique_or_mismatched"] += 1
            continue
        input_group = probe.buckets(groups.get(probe.id_key(forward.get("correlation_id")), []), lambda r: r.get("stage"))
        native = probe.one(input_group, "HostNativeInput")
        if native is None or probe.one(input_group, "Forwarded") is not forward:
            excluded["native_forward_not_unique"] += 1
            continue
        chain = probe.buckets(groups.get(probe.id_key(surface.get("correlation_id")), []), lambda r: r.get("stage"))
        if probe.one(chain, "SurfaceCommit") is not surface:
            excluded["duplicate_surface_commit"] += 1
            continue
        for stage in endpoints:
            end = probe.one(chain, stage)
            if end is None:
                excluded["missing_or_duplicate_" + stage] += 1
                continue
            timestamps = [native["timestamp_ns"], forward["timestamp_ns"], surface["timestamp_ns"], end["timestamp_ns"]]
            windows = {probe.id_key(row["window_id"]) for row in (native, forward, surface, end) if row.get("window_id") is not None}
            if len(windows) > 1 or any(b < a for a, b in zip(timestamps, timestamps[1:])):
                excluded["reversed_" + stage] += 1
                continue
            values[stage].append(end["timestamp_ns"] - native["timestamp_ns"])
    metrics = []
    for stage in endpoints:
        result = probe.metric(values[stage])
        metrics.append({"from": "HostNativeInput", "to": stage, "samples": result["n"],
                        "milliseconds": None if result["quantiles_ns"] is None else
                        {k: v / 1e6 for k, v in result["quantiles_ns"].items()},
                        "endpoint": "actual_API_presentedTime_not_photons" if stage == "MetalPresented" else
                        "callback_arrival_NOT_presentation" if stage == "MetalPresentedCallback" else
                        "CPU_observation_NOT_presentation"})
    return {"schema": 1, "clock": "host_ca", "cross_clock_subtraction": False,
            "linux_receipt_or_input_to_photon": None, "host_keyboard_events": sum(r.get("stage") == "Forwarded" for r in events),
            "probe_keyboard_receipts": len(keys), "probe_presses": sum(r.get("raw_state") == 1 for r in keys),
            "probe_releases": sum(r.get("raw_state") == 0 for r in keys),
            "metrics": metrics, "excluded": dict(excluded),
            "limitations": ["Only uniquely joined, single-keyboard-cause frames; coalescing can bias quantiles.",
                            "HostNativeInput is a Winit observation, not device/hardware arrival.",
                            "Read companion stage report for trace loss and final-stats completeness.",
                            "No Linux receipt, application Hover latency, or photon latency is measured."]}


class Tests(unittest.TestCase):
    def test_explicit_join_and_distinct_actual_present_endpoint(self):
        host, guest, _ = probe.StaticTests.fixture()
        result = summarize(host, guest)
        metrics = {row["to"]: row for row in result["metrics"]}
        self.assertEqual(metrics["MetalPresented"]["milliseconds"]["p99"], .0003)
        self.assertEqual(metrics["MetalPresentedCallback"]["samples"], 0)
        self.assertFalse(result["cross_clock_subtraction"])

    def test_duplicate_or_coalesced_markers_are_not_latency_samples(self):
        host, guest, _ = probe.StaticTests.fixture()
        host.append(dict(host[2]))
        self.assertTrue(all(row["samples"] == 0 for row in summarize(host, guest)["metrics"]))
        host, guest, _ = probe.StaticTests.fixture()
        guest[-1]["applied_visual_events"] = 2
        self.assertTrue(all(row["samples"] == 0 for row in summarize(host, guest)["metrics"]))

    def test_duplicate_forward_or_reversed_endpoint_is_rejected(self):
        host, guest, _ = probe.StaticTests.fixture()
        host.append(dict(host[1]))
        self.assertTrue(all(row["samples"] == 0 for row in summarize(host, guest)["metrics"]))
        host, guest, _ = probe.StaticTests.fixture()
        host[-1]["timestamp_ns"] = 1
        self.assertEqual(summarize(host, guest)["metrics"][-1]["samples"], 0)

    def test_window_mismatch_is_rejected(self):
        host, guest, _ = probe.StaticTests.fixture()
        host[2]["window_id"] = 1
        host[-1]["window_id"] = 2
        self.assertEqual(summarize(host, guest)["metrics"][-1]["samples"], 0)

    def test_empty_is_not_zero(self):
        result = summarize([{"session_id": "one"}], [{"probe": probe.PROBE, "record": "start", "pid": 1, "tag": "one"}])
        self.assertTrue(all(row["samples"] == 0 and row["milliseconds"] is None for row in result["metrics"]))

    def test_multiple_runs_rejected(self):
        with self.assertRaises(ValueError):
            summarize([{"session_id": "one"}, {"session_id": "two"}],
                      [{"probe": probe.PROBE, "record": "start", "pid": 1, "tag": "one"}])

    def test_foreign_host_clock_rejected(self):
        with self.assertRaisesRegex(ValueError, "host_ca"):
            summarize([{"session_id": "one", "kind": "event", "clock": "guest_monotonic", "timestamp_ns": 1}],
                      [{"probe": probe.PROBE, "record": "start", "pid": 1, "tag": "one"}])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", type=Path)
    parser.add_argument("--probe", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return int(not unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromTestCase(Tests)).wasSuccessful())
    if not args.host or not args.probe:
        parser.error("--host HOST.jsonl --probe PROBE.jsonl required")
    budget = probe.Budget()
    with args.host.open("rb") as stream:
        host, _ = probe.read_rows(stream, probe.HOST_FIELDS, budget)
    with args.probe.open("rb") as stream:
        guest, _ = probe.read_rows(stream, probe.GUEST_FIELDS, budget, guest=True)
    text = json.dumps(summarize(host, guest), indent=2, allow_nan=False) + "\n"
    if args.output:
        with args.output.open("x", encoding="utf-8") as stream:
            stream.write(text)
    else:
        print(text, end="")


if __name__ == "__main__":
    sys.exit(main())
