#!/usr/bin/env python3
"""Summarize REAL trace host_ca spans; never synthesize events or subtract clocks.

Reuses analyze-latency.py's strict explicit-correlation, uniqueness and clock rules.
Input is capped at 250,000 records/64 MiB and 50,000 retained groups. Non-host clocks
are omitted from output metrics. Callback arrival is never actual presentation.
Forwarded is a host observation label, not proof of guest delivery: report whether
its protocol serial/time tags exist and still require actual guest-side evidence.
Capture-only final counts and late-call snapshots never establish lifetime loss
totals, callback completion, actual presentation or an end-to-end measurement.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import stat
import sys
import unittest

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("audit_latency", ROOT / "scripts/analyze-latency.py")
latency = importlib.util.module_from_spec(spec)
spec.loader.exec_module(latency)


def limited(paths):
    total = count = 0
    for path in paths:
        descriptor = os.open(path, os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW)
        with os.fdopen(descriptor, "rb") as stream:
            if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
                raise ValueError("real trace input must be a regular file")
            while True:
                row = stream.readline(latency.MAX_LINE_BYTES + 1)
                if not row:
                    break
                count += 1
                total += len(row)
                if len(row) > latency.MAX_LINE_BYTES or count > 250_000 or total > 64 * 2**20:
                    raise ValueError("trace record/byte capacity exceeded; all metrics aborted")
                yield row


def summarize(result):
    metrics = []
    for (clock, start, end), values in sorted(result["metrics"].items()):
        if clock != "host_ca":
            continue
        metrics.append({"clock": clock, "start": start, "end": end, "samples": len(values),
            "endpoint_semantics": "actual_CoreAnimation_presentedTime_not_photons" if end == "MetalPresented"
                else "GPU_command_timestamp" if end in ("GPUStarted", "GPUCompleted")
                else "CPU_observation_or_callback_arrival_not_actual_presentation",
            "milliseconds": {name: latency.percentile(values, q) for name, q in (("p50", .5), ("p95", .95), ("p99", .99))}})
    incomplete = result["sessions"] - {s for s, row in result["stats"].items() if row["final"]}
    return {"schema": 1, "kind": "real_host_stage_latency", "percentile_method": "nearest_rank",
        "host_guest_subtraction_performed": False, "input_to_photon": None,
        "verified_host_ca_spans": metrics, "sessions": len(result["sessions"]),
        "sessions_without_final_stats": len(incomplete),
        "final_loss_totals_known": bool(result["sessions"]) and not incomplete and not result["untracked_records"],
        **latency.capture_status(result),
        "observed_cumulative_dropped": sum(row["dropped"] for row in result["stats"].values()),
        "observed_cumulative_rejected": sum(row["rejected"] for row in result["stats"].values()),
        "invalid_records": result["invalid"], "oversized_records": result["oversized"],
        "capacity_untracked_records": result["untracked_records"],
        "capacity_unanalyzed_events": sum(result["unanalyzed"].values()),
        "retained_groups": result["groups"], "stages_observed": dict(result["stages"]),
        "excluded_spans": {start + "->" + end: dict(reasons) for (start, end), reasons in result["excluded"].items()},
        "limitations": ["No temporal-nearest input/commit matching and no native-vs-Linux clock substitution.",
            "Missing/ambiguous/coalesced/dropped records can bias percentiles; these are retained verified spans only.",
            "CPU submission, GPU completion and presented callback arrival are not actual screen presentation.",
            "Capture closure/drain is a recording-call boundary, not callback completion or end-to-end latency evidence.",
            "Late recording calls are outside capture dropped/rejected counts; snapshots are not pending-callback totals."]}


def annotate_forwarding(report, forwarding):
    verified, legacy = [], []
    fully_tagged = forwarding["events"] > 0 and forwarding["events"] == forwarding["with_protocol_serial_and_time"]
    for span in report["verified_host_ca_spans"]:
        if span["end"] == "Forwarded":
            if not fully_tagged:
                span["endpoint_semantics"] = "legacy_or_unverified_native_event_handler_completion_NOT_guest_forwarding"
                span["semantic_end"] = "LegacyHostInputHandled"
                legacy.append(span)
                continue
            span["endpoint_semantics"] = "host_keyboard_dispatch_observation_NOT_guest_receipt"
        verified.append(span)
    report["verified_host_ca_spans"] = verified
    report["legacy_or_unverified_handler_spans"] = legacy
    report["forwarding_metadata"] = forwarding
    report["five_ms_guest_receipt_goal_assessed"] = False
    return report


class Tests(unittest.TestCase):
    def test_empty_is_unknown_not_zero(self):
        report = summarize(latency.analyze([]))
        self.assertEqual(report["verified_host_ca_spans"], [])
        self.assertIsNone(report["input_to_photon"])
    def test_cpu_and_present_endpoints_remain_distinct(self):
        events = [latency.AnalysisTests.event(stage, (i + 1) * 1000) for i, stage in enumerate(latency.STAGES)]
        report = summarize(latency.analyze(events))
        present = next(row for row in report["verified_host_ca_spans"] if row["end"] == "MetalPresented")
        self.assertIn("actual_CoreAnimation", present["endpoint_semantics"])
        self.assertFalse(report["final_loss_totals_known"])
    def test_other_clock_not_exported(self):
        rows = [latency.AnalysisTests.event(stage, i + 1, clock="guest_monotonic") for i, stage in enumerate(latency.STAGES)]
        self.assertEqual(summarize(latency.analyze(rows))["verified_host_ca_spans"], [])
    def test_legacy_forwarded_not_exported_as_dispatch(self):
        rows = [latency.AnalysisTests.event(stage, i + 1) for i, stage in enumerate(latency.STAGES)]
        result = annotate_forwarding(summarize(latency.analyze(rows)), {"events": 1, "with_protocol_serial_and_time": 0})
        self.assertFalse(any(row["end"] == "Forwarded" for row in result["verified_host_ca_spans"]))
        self.assertEqual(result["legacy_or_unverified_handler_spans"][0]["semantic_end"], "LegacyHostInputHandled")
        self.assertFalse(result["five_ms_guest_receipt_goal_assessed"])
    def test_capture_complete_with_held_producers_keeps_lifetime_unknown(self):
        header = json.dumps({"schema": 1, "kind": "session", "session_id": "s",
                             "capture_endpoint": "recording_api_admission_not_event_timestamp",
                             "late_callbacks": "post_close_recording_calls_excluded_from_capture"})
        rows = [header, latency.AnalysisTests.event("CPUSubmitted", 10),
                latency.AnalysisTests.event("MetalPresentedCallback", 20),
                latency.AnalysisTests.stats(dropped=2, rejected=1, capture_counts_final=True,
                    capture_drained=True, lifetime_loss_totals_known=False, late_record_calls_observed=4)]
        report = summarize(latency.analyze(rows))
        self.assertEqual(report["sessions_without_final_stats"], 1)
        self.assertFalse(report["final_loss_totals_known"])
        self.assertTrue(report["capture_loss_totals_known"])
        self.assertEqual(report["sessions_with_final_capture_counts"], 1)
        self.assertEqual(report["observed_cumulative_dropped"], 2)
        self.assertEqual(report["observed_cumulative_rejected"], 1)
        self.assertEqual(report["observed_cumulative_late_record_calls"], 4)
        self.assertFalse(report["late_record_call_totals_known"])
        self.assertFalse(report["session_capture_metadata"]["s"]["lifetime_loss_totals_known"])
        self.assertEqual(report["session_capture_metadata"]["s"]["capture_endpoint"],
                         "recording_api_admission_not_event_timestamp")
        self.assertIsNone(report["input_to_photon"])
        self.assertFalse(any(span["end"] == "MetalPresented" for span in report["verified_host_ca_spans"]))
        self.assertIn("callback_arrival_not_actual_presentation", report["verified_host_ca_spans"][0]["endpoint_semantics"])
    def test_legacy_final_stats_leave_unreported_late_counts_unknown(self):
        report = summarize(latency.analyze([latency.AnalysisTests.stats(final=True)]))
        self.assertTrue(report["final_loss_totals_known"])
        self.assertTrue(report["capture_loss_totals_known"])
        self.assertEqual(report["sessions_with_late_call_counts"], 0)
        self.assertFalse(report["late_record_call_totals_known"])
        self.assertIsNone(report["session_capture_metadata"]["s"]["late_record_calls_observed"])
    def test_mixed_sessions_and_snapshots_do_not_sum_late_counts_or_upgrade_totals(self):
        rows = [latency.AnalysisTests.stats(capture_counts_final=True, late_record_calls_observed=2),
                latency.AnalysisTests.stats(capture_counts_final=True, late_record_calls_observed=5),
                latency.AnalysisTests.event("Forwarded", 1, session="unfinished")]
        report = summarize(latency.analyze(rows))
        self.assertEqual(report["observed_cumulative_late_record_calls"], 5)
        self.assertEqual(report["sessions_with_final_capture_counts"], 1)
        self.assertFalse(report["capture_loss_totals_known"])
        self.assertFalse(report["final_loss_totals_known"])
        self.assertFalse(report["late_record_call_totals_known"])
    def test_lifetime_final_does_not_finalize_an_earlier_late_snapshot(self):
        rows = [latency.AnalysisTests.stats(late_record_calls_observed=3),
                latency.AnalysisTests.stats(final=True)]
        report = summarize(latency.analyze(rows))
        self.assertTrue(report["final_loss_totals_known"])
        self.assertTrue(report["capture_loss_totals_known"])
        self.assertEqual(report["observed_cumulative_late_record_calls"], 3)
        self.assertFalse(report["late_record_call_totals_known"])
    def test_conflicting_lifetime_metadata_is_not_reported_as_final(self):
        rows = [latency.AnalysisTests.stats(final=True, lifetime_loss_totals_known=False)]
        report = summarize(latency.analyze(rows))
        self.assertEqual(report["invalid_records"], 1)
        self.assertFalse(report["final_loss_totals_known"])
        self.assertFalse(report["capture_loss_totals_known"])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("traces", nargs="*", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return int(not unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromTestCase(Tests)).wasSuccessful())
    if not args.traces or len(args.traces) > 16:
        parser.error("provide 1..16 real trace paths")
    forwarding = {"events": 0, "with_protocol_serial_and_time": 0,
                  "guest_receipt_proven": False,
                  "meaning": "Host event-completion span alone does not establish protocol delivery or guest receipt"}
    def records():
        for line in limited(args.traces):
            try:
                row = json.loads(line)
                if isinstance(row, dict) and row.get("kind") == "event" and row.get("stage") == "Forwarded":
                    forwarding["events"] += 1
                    if row.get("source_event_serial") is not None and row.get("source_event_time") is not None:
                        forwarding["with_protocol_serial_and_time"] += 1
            except (ValueError, TypeError, UnicodeError, RecursionError):
                pass  # The underlying analyzer reports malformed rows exactly once.
            yield line
    report = summarize(latency.analyze(records()))
    annotate_forwarding(report, forwarding)
    text = json.dumps(report, indent=2, allow_nan=False) + "\n"
    if args.output:
        with args.output.open("x", encoding="utf-8") as stream:
            stream.write(text)
    else:
        sys.stdout.write(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
