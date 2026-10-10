#!/usr/bin/env python3
"""Read macOS ps CPU centiseconds/RSS KiB, not power; 100% means one core.
Host monotonic timestamps use the ps bracket midpoint, not exact kernel time.
Errors retain a valid prefix, never fake zeros. PID reuse cannot be excluded.
Self-tests never execute ps; multithreaded CPU percentages may exceed 100%.
"""
import argparse
import json
import math
import os
import re
import subprocess
import sys
import time
import unittest
from unittest.mock import patch

MAX_RECORDS = 301
TIME_PATTERN = re.compile(r"(?:(?P<days>[0-9]+)-)?(?P<body>[0-9]+:[0-9]{2}(?::[0-9]{2})?)\.(?P<cs>[0-9]{2})")
def parse_ps(output):
    lines = [line.strip() for line in output.splitlines() if line.strip()]
    if len(output) > 256 or len(lines) != 1:
        raise ValueError("expected one short ps process row")
    fields = lines[0].split()
    if len(fields) != 2 or not re.fullmatch(r"[0-9]+", fields[1]):
        raise ValueError("expected CPU time and unsigned integer RSS KiB")
    match = TIME_PATTERN.fullmatch(fields[0])
    if not match:
        raise ValueError("expected M:SS.cc, H:MM:SS.cc or D-H:MM:SS.cc")
    parts = [int(part) for part in match["body"].split(":")]
    days = int(match["days"] or 0)
    if len(parts) == 2:
        hours, minutes, seconds = 0, *parts
        if match["days"] is not None:
            raise ValueError("day prefix requires hours")
    else:
        hours, minutes, seconds = parts
        if minutes >= 60 or (match["days"] is not None and hours >= 24):
            raise ValueError("invalid hours/minutes")
    if seconds >= 60:
        raise ValueError("invalid seconds")
    return ((days * 24 + hours) * 3600 + minutes * 60 + seconds) * 100 + int(match["cs"]), int(fields[1])

def read_ps(pid):
    started = time.monotonic_ns()
    result = subprocess.run(
        ["/bin/ps", "-p", str(pid), "-o", "time=,rss="], capture_output=True,
        text=True, encoding="ascii", errors="strict", timeout=5, check=False,
        env={**os.environ, "LC_ALL": "C"})
    finished = time.monotonic_ns()
    if result.returncode or not result.stdout.strip():
        raise ProcessLookupError(f"ps failed/PID exited ({result.returncode}): {result.stderr.strip()[:256]}")
    cpu_cs, rss = parse_ps(result.stdout)
    return {"host_monotonic_ns": (started + finished) // 2,
            "ps_started_ns": started, "ps_finished_ns": finished,
            "cpu_centiseconds": cpu_cs, "rss_kib": rss}

def cpu_interval(previous, current):
    wall = current["host_monotonic_ns"] - previous["host_monotonic_ns"]
    cpu = current["cpu_centiseconds"] - previous["cpu_centiseconds"]
    if wall <= 0 or cpu < 0:
        raise ValueError("nonpositive wall delta or decreasing CPU counter; possible PID reuse")
    return {"wall_delta_ns": wall, "cpu_delta_centiseconds": cpu,
            "cpu_single_core_percent": cpu * 10_000_000 / wall * 100}

def quantiles(values):
    values = sorted(values)
    return {"count": len(values), **{
        name: values[math.ceil(len(values) * q) - 1] if values else None
        for name, q in (("p50", .50), ("p95", .95), ("p99", .99))}}

def schedule(duration, interval):
    if not math.isfinite(duration) or not 0 < duration <= 300:
        raise ValueError("--duration must be finite and in (0, 300]")
    if not math.isfinite(interval) or not 0 < interval <= 300:
        raise ValueError("--interval must be finite and in (0, 300]")
    duration, interval = round(duration * 1e9), round(interval * 1e9)
    if min(duration, interval) < 1:
        raise ValueError("duration/interval must be at least 1 ns")
    count = (duration + interval - 1) // interval + 1
    if count > MAX_RECORDS:
        raise ValueError("duration/interval exceeds 301 observations")
    return [min(i * interval, duration) for i in range(count)]

def sample(pid, duration, interval, reader=read_ps, clock=time.monotonic_ns, sleep=time.sleep):
    offsets, started = schedule(duration, interval), clock()
    report = {
        "schema": 1, "kind": "process_resources", "status": "ok", "pid": pid,
        "platform": sys.platform, "clock": "host_time.monotonic_ns",
        "observation_timestamp": "ps_bracket_midpoint", "cpu_units": "centiseconds",
        "rss_units": "KiB", "cpu_percent_basis": "single_core; 100 percent = one core",
        "percentile_method": "nearest_rank", "duration_seconds": duration,
        "interval_seconds": interval, "max_records": MAX_RECORDS,
        "planned_records": len(offsets), "started_ns": started,
        "pid_identity_verified": False, "samples": [], "error": None}
    current = None
    try:
        for offset in offsets:
            deadline = started + offset
            while True:
                remaining = deadline - clock()
                if remaining <= 0:
                    break
                sleep(remaining / 1e9)
            current = reader(pid)
            current.update({"scheduled_ns": deadline, "wall_delta_ns": None,
                            "cpu_delta_centiseconds": None, "cpu_single_core_percent": None})
            if report["samples"]:
                current.update(cpu_interval(report["samples"][-1], current))
            report["samples"].append(current)
            current = None
    except (OSError, ValueError, subprocess.SubprocessError, KeyboardInterrupt) as error:
        report.update(status="error", error={"type": type(error).__name__, "message": str(error)})
        if current is not None:
            report["failed_observation"] = current
    rows = report["samples"]
    report["finished_ns"] = clock()
    report["summary_scope"] = "all_observations" if report["status"] == "ok" else "valid_prefix"
    report["summary"] = {
        "cpu_single_core_percent": quantiles([row["cpu_single_core_percent"] for row in rows[1:]]),
        "rss_kib": quantiles([row["rss_kib"] for row in rows]),
        "observed_wall_ns": rows[-1]["host_monotonic_ns"] - rows[0]["host_monotonic_ns"] if len(rows) > 1 else None}
    return report

class Tests(unittest.TestCase):
    def test_parser(self):
        for text, expected in (("0:00.00 0", (0, 0)), ("125:00.01 9", (750001, 9)),
                               ("2:03:04.05 8", (738405, 8)), ("1-23:59:59.99 1", (17279999, 1))):
            self.assertEqual(parse_ps(text), expected)
        for text in ("", "TIME RSS", "0:00 1", "0:00.1 1", "0:60.00 1", "1:60:00.00 1",
                     "1-24:00:00.00 1", "1-00:00.00 1", "0:00.00 -1", "0:00.00 1.5",
                     "0:00.00 1 extra", "0:00.00 1\n0:00.00 2", "0:00.00 \u0661"):
            with self.assertRaises(ValueError):
                parse_ps(text)
    def test_ps_reader(self):
        with patch("subprocess.run", return_value=subprocess.CompletedProcess([], 0, "0:01.23 42", "")) as run:
            self.assertEqual(read_ps(123)["cpu_centiseconds"], 123)
            self.assertEqual(run.call_args.args[0], ["/bin/ps", "-p", "123", "-o", "time=,rss="])
        for code in (0, 1):
            with patch("subprocess.run", return_value=subprocess.CompletedProcess([], code, "", "")):
                with self.assertRaises(ProcessLookupError):
                    read_ps(123)
    def test_cpu_quantiles(self):
        before = {"host_monotonic_ns": 1, "cpu_centiseconds": 0}
        self.assertEqual(cpu_interval(before, {"host_monotonic_ns": 1_000_000_001, "cpu_centiseconds": 250})["cpu_single_core_percent"], 250)
        for after in (before, {"host_monotonic_ns": 2, "cpu_centiseconds": -1}):
            with self.assertRaises(ValueError):
                cpu_interval(before, after)
        self.assertEqual(quantiles(range(1, 101)), {"count": 100, "p50": 50, "p95": 95, "p99": 99})
    def test_limits(self):
        self.assertEqual(len(schedule(20, 1)), 21)
        self.assertEqual(len(schedule(300, 1)), 301)
        for d, i in ((301, 1), (0, 1), (20, 0), (300, .5), (math.nan, 1), (20, math.inf)):
            with self.assertRaises(ValueError):
                schedule(d, i)
    def test_json_sampler(self):
        now = [0]
        def sleep(seconds):
            now[0] += round(seconds * 1e9)
        def reader(_):
            return {"host_monotonic_ns": now[0], "cpu_centiseconds": now[0] // 10_000_000, "rss_kib": 42}
        report = sample(123, 2, 1, reader, lambda: now[0], sleep)
        self.assertEqual(json.loads(json.dumps(report, allow_nan=False)), report)
        self.assertEqual(report["summary"]["cpu_single_core_percent"]["p99"], 100)
        self.assertIsNone(report["samples"][0]["cpu_single_core_percent"])
        with patch("subprocess.run", side_effect=ProcessLookupError("exited")):
            failed = sample(123, 1, 1, clock=lambda: now[0], sleep=sleep)
        self.assertEqual(failed["status"], "error")
        self.assertEqual(failed["samples"], [])

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pid", type=int)
    parser.add_argument("--duration", type=float, default=20)
    parser.add_argument("--interval", type=float, default=1)
    parser.add_argument("--output", help="JSON path; default stdout")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return int(not unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromTestCase(Tests)).wasSuccessful())
    if args.pid is None or not 0 < args.pid <= 2_147_483_647:
        parser.error("--pid must be positive")
    try:
        schedule(args.duration, args.interval)
    except ValueError as error:
        parser.error(str(error))
    if sys.platform != "darwin":
        parser.error("sampling requires macOS ps centisecond TIME")
    report = sample(args.pid, args.duration, args.interval)
    try:
        text = json.dumps(report, indent=2, allow_nan=False) + "\n"
        if args.output:
            with open(args.output, "w", encoding="utf-8") as output:
                output.write(text)
        else:
            sys.stdout.write(text)
    except OSError as error:
        print(f"could not write JSON: {error}", file=sys.stderr)
        return 1
    return int(report["status"] != "ok")

if __name__ == "__main__":
    sys.exit(main())
