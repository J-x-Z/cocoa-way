#!/usr/bin/env python3
"""Analyze v1 latency JSONL without guessing input/commit causality.

Usage: python3 scripts/analyze-latency.py TRACE.jsonl [MORE.jsonl ...]
       python3 scripts/analyze-latency.py --self-test

Only explicit, type-preserving (session_id, correlation_id) groups are paired.
Serials, source-event times, window IDs, and temporal proximity are NOT links.
Each metric requires one event at every stage in its span, one clock domain,
compatible window tags, and nondecreasing times. Percentiles use nearest rank.
Cross-clock spans are excluded even if calibration records exist: calibration
bounds and a guest harness are not implemented here. host_ca is CACurrentMediaTime
in ns, including actual drawable.presentedTime, not callback/completion time.
Stats are cumulative per session: take maxima, never sum repeated snapshots.
Missing final stats means drop/loss totals are unknown, not zero.

Memory is bounded: retain the first --max-groups correlation groups and at most
--max-sessions sessions; no eviction/reopening that could hide duplicates. Keep
one compact event and a count per stage, including ALL later duplicates of kept
groups. Exact percentile storage is bounded by 8 * max-groups samples. Capacity
omissions are declared; this retained subset is NOT a random/all-frame sample.
Repeated/coalesced SurfaceCommits are ALL unmatched to compose, never first/latest
selected; compose/submit/present spans can still be verified independently.
"""

import argparse
from collections import Counter, defaultdict
from contextlib import redirect_stdout
import io
import json
import math
import sys
import unittest


STAGES = (
    "HostNativeInput", "Forwarded", "SurfaceCommit", "ComposeStarted",
    "CPUSubmitted", "MetalPresented",
)
PAIRS = tuple(zip(STAGES, STAGES[1:])) + (
    ("SurfaceCommit", "MetalPresented"),
    ("ComposeStarted", "MetalPresented"),
    ("HostNativeInput", "MetalPresented"),
)
U64_MAX = (1 << 64) - 1
MAX_LINE_BYTES = 16_384
DEFAULT_MAX_GROUPS = 50_000
DEFAULT_MAX_SESSIONS = 128


def is_u64(value):
    return type(value) is int and 0 <= value <= U64_MAX


def is_tag(value):
    return is_u64(value) or (type(value) is str and 0 < len(value.encode("utf-8")) <= 256)


def tag_key(value):
    return type(value).__name__, value


def percentile(values, fraction):
    ordered = sorted(values)
    return ordered[max(0, math.ceil(len(ordered) * fraction) - 1)] / 1_000_000


def analyze(lines, max_groups=DEFAULT_MAX_GROUPS, max_sessions=DEFAULT_MAX_SESSIONS):
    if max_groups < 1 or max_sessions < 1:
        raise ValueError("analyzer capacities must be positive")
    groups = {}
    sessions = set()
    headers = set()
    stats = {}
    stages = Counter()
    unmatched = Counter()
    unanalyzed = Counter()
    invalid = 0
    ignored_calibrations = 0
    oversized = 0
    untracked_records = 0
    for line in lines:
        if line is None or len(line) > MAX_LINE_BYTES:
            oversized += 1
            continue
        if not line.strip():
            continue
        try:
            row = json.loads(line)
            if not isinstance(row, dict) or type(row.get("schema")) is not int or row["schema"] != 1:
                raise ValueError("unsupported schema")
            session = row.get("session_id")
            if type(session) is not str or not is_tag(session):
                raise ValueError("missing session")
            kind = row.get("kind")
            if kind == "stats":
                if not all(is_u64(row.get(k)) for k in ("dropped", "rejected")) or type(row.get("final")) is not bool:
                    raise ValueError("invalid stats")
            elif kind == "event":
                stage = row.get("stage")
                clock = row.get("clock")
                if stage not in STAGES or not is_u64(row.get("timestamp_ns")):
                    raise ValueError("invalid stage/time")
                if type(clock) is not str or not is_tag(clock):
                    raise ValueError("missing clock domain")
                if stage == "MetalPresented" and row["timestamp_ns"] == 0:
                    raise ValueError("not actually presented")
                for field in ("correlation_id", "window_id", "source_event_serial", "source_event_time"):
                    if row.get(field) is not None and not is_tag(row[field]):
                        raise ValueError("invalid tag")
                stages[stage] += 1
            elif kind not in ("session", "calibration"):
                raise ValueError("unknown record kind")

            if session not in sessions and len(sessions) >= max_sessions:
                untracked_records += 1
                if kind == "event":
                    unanalyzed[stage] += 1
                continue
            sessions.add(session)
            if kind == "session":
                headers.add(session)
            elif kind == "stats":
                previous = stats.get(session, {"dropped": 0, "rejected": 0, "final": False})
                stats[session] = {
                    "dropped": max(previous["dropped"], row["dropped"]),
                    "rejected": max(previous["rejected"], row["rejected"]),
                    "final": previous["final"] or row["final"],
                }
            elif kind == "calibration":
                ignored_calibrations += 1
            elif kind == "event":
                correlation = row.get("correlation_id")
                if correlation is None:
                    unmatched[stage] += 1
                else:
                    key = session, tag_key(correlation)
                    if key not in groups:
                        if len(groups) >= max_groups:
                            unanalyzed[stage] += 1
                            continue
                        groups[key] = {}
                    group = groups[key]
                    if stage not in group:
                        group[stage] = [0, {k: row.get(k) for k in ("clock", "timestamp_ns", "window_id")}]
                    group[stage][0] += 1
        except (ValueError, TypeError, UnicodeError, RecursionError):
            invalid += 1

    missing = Counter()
    ambiguous = Counter()
    metrics = defaultdict(list)
    excluded = {pair: Counter() for pair in PAIRS}
    matched_commits = 0
    multiple_commit_groups = 0
    for group in groups.values():
        if group.get("SurfaceCommit", [0])[0] > 1:
            multiple_commit_groups += 1
        for stage in STAGES:
            if not group.get(stage):
                missing[stage] += 1
            elif group[stage][0] > 1:
                ambiguous[stage] += 1
        for pair in PAIRS:
            start, end = pair
            span = STAGES[STAGES.index(start):STAGES.index(end) + 1]
            if any(not group.get(stage) for stage in span):
                excluded[pair]["missing"] += 1
                continue
            if any(group[stage][0] != 1 for stage in span):
                excluded[pair]["ambiguous"] += 1
                continue
            chain = [group[stage][1] for stage in span]
            clocks = {row["clock"] for row in chain}
            windows = {tag_key(row["window_id"]) for row in chain if row.get("window_id") is not None}
            times = [row["timestamp_ns"] for row in chain]
            if len(clocks) != 1:
                excluded[pair]["cross_clock"] += 1
            elif len(windows) > 1:
                excluded[pair]["window_conflict"] += 1
            elif any(b < a for a, b in zip(times, times[1:])):
                excluded[pair]["reversed_time"] += 1
            else:
                metrics[(chain[0]["clock"], start, end)].append(times[-1] - times[0])
                if pair == ("SurfaceCommit", "ComposeStarted"):
                    matched_commits += 1
    return {
        "sessions": sessions, "headers": headers, "stats": stats,
        "groups": len(groups), "stages": stages, "unmatched": unmatched,
        "missing": missing, "ambiguous": ambiguous, "metrics": metrics,
        "excluded": excluded, "invalid": invalid,
        "ignored_calibrations": ignored_calibrations,
        "limits": (max_groups, max_sessions), "oversized": oversized,
        "unanalyzed": unanalyzed, "untracked_records": untracked_records,
        "multiple_commit_groups": multiple_commit_groups,
        "unmatched_commits": stages["SurfaceCommit"] - matched_commits,
    }


def report(result):
    max_groups, max_sessions = result["limits"]
    print(f"Retained sessions: {len(result['sessions'])}; retained explicit correlation groups: {result['groups']}")
    print(f"Analyzer limits: groups={max_groups} sessions={max_sessions} line_bytes={MAX_LINE_BYTES}")
    print(f"Capacity-untracked records: {result['untracked_records']}; oversized records: {result['oversized']}")
    print(f"Invalid/unsupported JSONL records: {result['invalid']}; ignored calibrations: {result['ignored_calibrations']}")
    incomplete = result["sessions"] - {s for s, row in result["stats"].items() if row["final"]}
    dropped = sum(row["dropped"] for row in result["stats"].values())
    rejected = sum(row["rejected"] for row in result["stats"].values())
    print(f"Observed cumulative drops: {dropped}; rejected times/tags: {rejected}")
    print(f"Sessions without final stats: {len(incomplete)} (their loss totals are UNKNOWN)")
    print(f"Sessions without headers: {len(result['sessions'] - result['headers'])}")
    if result["untracked_records"]:
        print("Untracked sessions' drop totals and distinct session count are UNKNOWN.")
    print("Missing/ambiguous counts concern retained groups only; no_id means uncorrelated events.")
    for stage in STAGES:
        print(f"  {stage}: events={result['stages'][stage]} no_id={result['unmatched'][stage]} "
              f"capacity_unanalyzed={result['unanalyzed'][stage]} "
              f"missing={result['missing'][stage]} ambiguous={result['ambiguous'][stage]}")
    print(f"Multiple-commit groups (coalesced/repeated, never selected): {result['multiple_commit_groups']}")
    print(f"SurfaceCommit samples without a verified ComposeStarted match: {result['unmatched_commits']}")
    print("Verified retained spans (explicit caller-asserted correlation, same clock; nearest-rank ms):")
    if not result["metrics"]:
        print("  No verified samples.")
    for (clock, start, end), values in sorted(result["metrics"].items()):
        print(f"  [{clock}] {start} -> {end}: n={len(values)} "
              f"P50={percentile(values, .50):.3f} P95={percentile(values, .95):.3f} "
              f"P99={percentile(values, .99):.3f}")
    print("Excluded spans (no inferred input/commit associations or uncalibrated host/guest subtraction):")
    for (start, end), reasons in result["excluded"].items():
        print(f"  {start} -> {end}: " + (", ".join(f"{k}={v}" for k, v in sorted(reasons.items())) or "none"))
    print("Trace loss, missing stages and capacity-prefix selection can bias percentiles; not all-frame latency.")


def bounded_lines(stream):
    """Read binary lines with bounded allocation, discarding an oversized row whole."""
    while True:
        chunk = stream.readline(MAX_LINE_BYTES + 1)
        if not chunk:
            return
        if len(chunk) > MAX_LINE_BYTES:
            while chunk and not chunk.endswith(b"\n"):
                chunk = stream.readline(MAX_LINE_BYTES + 1)
            yield None
        else:
            yield chunk


class AnalysisTests(unittest.TestCase):
    @staticmethod
    def event(stage, ns, correlation=1, clock="host_ca", session="s", **tags):
        return json.dumps({"schema": 1, "kind": "event", "session_id": session,
                           "stage": stage, "timestamp_ns": ns, "clock": clock,
                           "correlation_id": correlation, **tags})

    def test_complete_explicit_chain_and_percentiles(self):
        rows = [self.event(stage, (i + 1) * 1_000_000) for i, stage in enumerate(STAGES)]
        result = analyze(rows)
        self.assertEqual(result["metrics"][("host_ca", "HostNativeInput", "MetalPresented")], [5_000_000])
        self.assertEqual(percentile([1_000_000, 2_000_000, 3_000_000], .50), 2.0)
        self.assertEqual(percentile([1_000_000, 2_000_000, 3_000_000], .99), 3.0)

    def test_no_serial_window_or_proximity_inference(self):
        rows = [self.event(stage, i + 1, None, source_event_serial=5, window_id=8)
                for i, stage in enumerate(STAGES)]
        result = analyze(rows)
        self.assertFalse(result["metrics"])
        self.assertEqual(sum(result["unmatched"].values()), 6)
        rows = [self.event("HostNativeInput", 1, 5), self.event("MetalPresented", 2, "5")]
        self.assertEqual(analyze(rows)["groups"], 2)

    def test_cross_clock_never_subtracted(self):
        result = analyze([self.event("HostNativeInput", 10),
                          self.event("Forwarded", 20, clock="guest_monotonic")])
        self.assertFalse(result["metrics"])
        self.assertEqual(result["excluded"][PAIRS[0]]["cross_clock"], 1)

    def test_independent_frame_has_no_input_latency(self):
        rows = [self.event(stage, i + 1, "input" if i < 2 else "frame")
                for i, stage in enumerate(STAGES)]
        result = analyze(rows)
        self.assertIn(("host_ca", "SurfaceCommit", "MetalPresented"), result["metrics"])
        self.assertNotIn(("host_ca", "HostNativeInput", "MetalPresented"), result["metrics"])

    def test_coalesced_commits_are_all_unmatched_but_render_is_verified(self):
        rows = [self.event(stage, i + 1) for i, stage in enumerate(STAGES)]
        rows.insert(3, self.event("SurfaceCommit", 3))
        result = analyze(rows)
        self.assertEqual(result["multiple_commit_groups"], 1)
        self.assertEqual(result["unmatched_commits"], 2)
        self.assertNotIn(("host_ca", "SurfaceCommit", "ComposeStarted"), result["metrics"])
        self.assertNotIn(("host_ca", "HostNativeInput", "MetalPresented"), result["metrics"])
        self.assertEqual(result["metrics"][("host_ca", "ComposeStarted", "MetalPresented")], [2])

    def test_group_capacity_does_not_hide_late_duplicates(self):
        rows = [self.event(stage, i + 1) for i, stage in enumerate(STAGES[2:])]
        rows += [self.event("Forwarded", 10, correlation=2), self.event("SurfaceCommit", 1)]
        result = analyze(rows, max_groups=1)
        self.assertEqual(result["groups"], 1)
        self.assertEqual(result["unanalyzed"]["Forwarded"], 1)
        self.assertEqual(result["unmatched_commits"], 2)
        self.assertNotIn(("host_ca", "SurfaceCommit", "ComposeStarted"), result["metrics"])
        self.assertIn(("host_ca", "ComposeStarted", "MetalPresented"), result["metrics"])

    def test_session_capacity_and_bounded_line_reader(self):
        result = analyze([self.event("Forwarded", 1), self.event("SurfaceCommit", 2, session="other")],
                         max_sessions=1)
        self.assertEqual(len(result["sessions"]), 1)
        self.assertEqual(result["untracked_records"], 1)
        self.assertEqual(result["unanalyzed"]["SurfaceCommit"], 1)
        self.assertEqual(result["unmatched_commits"], 1)
        stream = io.BytesIO(b"x" * (MAX_LINE_BYTES + 1) + b"\n" + self.event("Forwarded", 1).encode() + b"\n")
        result = analyze(bounded_lines(stream))
        self.assertEqual(result["oversized"], 1)
        self.assertEqual(result["stages"]["Forwarded"], 1)

    def test_missing_final_stats_declares_unknown(self):
        output = io.StringIO()
        with redirect_stdout(output):
            report(analyze([self.event("Forwarded", 1)]))
        self.assertIn("Sessions without final stats: 1", output.getvalue())
        self.assertIn("loss totals are UNKNOWN", output.getvalue())

    def test_duplicates_reversal_windows_and_sessions(self):
        first = self.event("ComposeStarted", 10, window_id=1)
        second = self.event("CPUSubmitted", 20, window_id=1)
        self.assertEqual(analyze([first, first, second])["excluded"][PAIRS[3]]["ambiguous"], 1)
        self.assertEqual(analyze([second, self.event("ComposeStarted", 30)])["excluded"][PAIRS[3]]["reversed_time"], 1)
        self.assertEqual(analyze([first, self.event("CPUSubmitted", 20, window_id=2)])["excluded"][PAIRS[3]]["window_conflict"], 1)
        self.assertFalse(analyze([first, self.event("CPUSubmitted", 20, session="other")])["metrics"])

    def test_missing_stages_and_cumulative_drops(self):
        stats = lambda dropped, final: json.dumps({"schema": 1, "kind": "stats", "session_id": "s",
                                                   "dropped": dropped, "rejected": 1, "final": final})
        result = analyze([self.event("HostNativeInput", 1), stats(3, False), stats(5, True)])
        self.assertEqual(result["stats"]["s"]["dropped"], 5)
        self.assertTrue(result["stats"]["s"]["final"])
        self.assertEqual(result["missing"]["SurfaceCommit"], 1)
        self.assertFalse(result["metrics"])

    def test_bad_json_stage_tag_and_zero_presentation(self):
        rows = ["{", self.event("Bogus", 1), self.event("MetalPresented", 0),
                self.event("Forwarded", -1), self.event("Forwarded", 1, True)]
        self.assertEqual(analyze(rows)["invalid"], 5)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("files", nargs="*")
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--max-groups", type=int, default=DEFAULT_MAX_GROUPS)
    parser.add_argument("--max-sessions", type=int, default=DEFAULT_MAX_SESSIONS)
    args = parser.parse_args()
    if args.self_test:
        unittest.main(argv=[sys.argv[0]])
        return
    if not args.files:
        parser.error("provide at least one JSONL file, or --self-test")
    if args.max_groups < 1 or args.max_sessions < 1:
        parser.error("capacities must be positive")

    def lines():
        for path in args.files:
            with open(path, "rb") as stream:
                yield from bounded_lines(stream)

    try:
        report(analyze(lines(), args.max_groups, args.max_sessions))
    except OSError as error:
        parser.exit(1, f"{error}\n")


if __name__ == "__main__":
    main()
