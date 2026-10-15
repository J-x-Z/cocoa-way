#!/usr/bin/env python3
"""Read-only real-host idle/workload sampling; never launches/stops a host/client.

Usage: python3 scripts/audit-host-resources.py --fixture HOST.json --label baseline --output NEW.json
       python3 scripts/audit-host-resources.py --fixture NEW.json --label candidate --output OTHER.json
       python3 scripts/audit-host-resources.py --self-test

Fixture JSON: host_pid, host_executable (immutable absolute path), host_sha256.
Parent owns identical isolated hosts/clients/settings, warmup and cleanup. Capture
serially with matching duration/interval and a static client; do not mix a baseline
idle host with a candidate three-app soak. Kernel CPU, interrupt/package-idle wakes,
RSS and footprint are resource/power PROXIES, not measured watts/battery impact.
Default 20 seconds, one-second intervals; at most 120 seconds/301 observations.
P50/P95/P99 concern interval rates, not event/wake latency. All timestamps are
observation brackets on this host, never guest clocks or presentation timestamps.
"""
import argparse
import ctypes
import importlib.util
import json
import math
import os
from pathlib import Path
import sys
import time
import unittest

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parent.parent


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


resources = module("audit_resource_sampler", ROOT / "scripts/sample-process-resources.py")
native = module("audit_process_identity", ROOT / "scripts/audit-native-probe.py")


def kernel_interval(previous, current):
    elapsed = current["kernel_host_monotonic_ns"] - previous["kernel_host_monotonic_ns"]
    if elapsed <= 0:
        raise ValueError("nonpositive kernel observation interval")
    result = {"wall_delta_ns": elapsed}
    for key in ("cpu_ns", "interrupt_wakes", "package_idle_wakes"):
        delta = current[key] - previous[key]
        if delta < 0:
            raise ValueError("decreasing kernel counter")
        result[key + "_delta"] = delta
        result[key + "_per_second"] = delta * 1e9 / elapsed
    result["cpu_single_core_percent"] = result["cpu_ns_delta"] / elapsed * 100
    return result


def capture(args):
    if sys.platform != "darwin":
        raise RuntimeError("real host kernel counters require macOS")
    if args.output.exists():
        raise FileExistsError("resource output must be a new file")
    if args.fixture.stat().st_size > 16384:
        raise ValueError("fixture metadata exceeds bound")
    fixture = json.loads(args.fixture.read_text(encoding="utf-8"))
    pid, host = fixture["host_pid"], Path(fixture["host_executable"]).resolve()
    if type(pid) is not int or pid <= 1:
        raise ValueError("invalid fixture PID")
    expected_uuid = native.macho_uuid(native.executable(host, fixture["host_sha256"]))
    libproc = ctypes.CDLL("/usr/lib/libproc.dylib")
    libproc.proc_pid_rusage.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_void_p]
    libproc.proc_pid_rusage.restype = ctypes.c_int
    libproc.proc_pidpath.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_uint32]
    libproc.proc_pidpath.restype = ctypes.c_int
    path = ctypes.create_string_buffer(4096)
    if libproc.proc_pidpath(pid, path, 4096) <= 0 or Path(os.fsdecode(path.value)).resolve() != host:
        raise ValueError("PID does not match isolated host executable")
    identity = native.process_identity(libproc, pid)
    if expected_uuid != identity[0]:
        raise ValueError("loaded host UUID differs from verified executable file")
    def read_usage():
        started = time.monotonic_ns()
        data = native.Usage()
        if libproc.proc_pid_rusage(pid, 0, ctypes.byref(data)):
            raise ProcessLookupError("host resource counters unavailable")
        finished = time.monotonic_ns()
        if (bytes(data.uuid), data.start) != identity:
            raise ProcessLookupError("host exited/reused during observation")
        return {"kernel_host_monotonic_ns": (started + finished) // 2,
            "kernel_read_started_ns": started, "kernel_read_finished_ns": finished,
            "cpu_ns": data.user + data.system, "interrupt_wakes": data.interrupt_wakes,
            "package_idle_wakes": data.package_wakes, "resident_bytes": data.resident,
            "physical_footprint_bytes": data.footprint}
    deadline = time.monotonic() + args.duration + 6
    def reader(target):
        if time.monotonic() >= deadline:
            raise TimeoutError("real-host sampler observation deadline exceeded")
        read_usage()
        observation = resources.read_ps(target)
        observation["kernel"] = read_usage()
        if time.monotonic() >= deadline:
            raise TimeoutError("real-host sampler observation deadline exceeded")
        return observation
    report = resources.sample(pid, args.duration, args.interval, reader=reader)
    intervals = [kernel_interval(a["kernel"], b["kernel"]) for a, b in zip(report["samples"], report["samples"][1:])]
    report.update(kind="actual_host_resources", label=args.label, fixture=fixture,
        pid_identity_verified=True, loaded_uuid=identity[0].hex(), process_start_abstime=identity[1],
        kernel_api="macOS proc_pid_rusage RUSAGE_INFO_V0", kernel_intervals=intervals,
        measured_watts=None, battery_impact=None, idle_condition_verified_by_sampler=False,
        host_or_client_lifecycle_modified=False)
    report["kernel_summary"] = {key: resources.quantiles([row[key] for row in intervals]) for key in
        ("cpu_single_core_percent", "interrupt_wakes_per_second", "package_idle_wakes_per_second")}
    report["kernel_summary"]["physical_footprint_bytes"] = resources.quantiles(
        [row["kernel"]["physical_footprint_bytes"] for row in report["samples"]])
    report["limitations"] = ["Parent must attest equal workload, static-client state, warmup, geometry and presentation settings.",
        "Real PID-verified kernel counters, not synthetic idle; nevertheless not machine watts or battery measurements.",
        "Observer ps subprocesses perturb system scheduling; unrelated host/VM/compiler load is not controlled.",
        "Each kernel counter uses its own host monotonic bracket; no guest or screen-presentation clock subtraction."]
    with args.output.open("x", encoding="utf-8") as output:
        output.write(json.dumps(report, indent=2, allow_nan=False) + "\n")
    print(f"Real {args.label} resources: {args.output}; status={report['status']}; intervals={len(intervals)}")
    return int(report["status"] != "ok")


class Tests(unittest.TestCase):
    def test_rates_and_rejection(self):
        before = {"kernel_host_monotonic_ns": 0, "cpu_ns": 0, "interrupt_wakes": 0, "package_idle_wakes": 0}
        after = {"kernel_host_monotonic_ns": 1_000_000_000, "cpu_ns": 50_000_000, "interrupt_wakes": 2, "package_idle_wakes": 1}
        self.assertEqual(kernel_interval(before, after)["cpu_single_core_percent"], 5)
        self.assertEqual(kernel_interval(before, after)["interrupt_wakes_per_second"], 2)
        with self.assertRaises(ValueError):
            kernel_interval(before, before)
        with self.assertRaises(ValueError):
            kernel_interval(after, {**after, "kernel_host_monotonic_ns": 2_000_000_000, "cpu_ns": 0})


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--fixture", type=Path)
    parser.add_argument("--label", choices=("baseline", "candidate"))
    parser.add_argument("--output", type=Path)
    parser.add_argument("--duration", type=float, default=20)
    parser.add_argument("--interval", type=float, default=1)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return int(not unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromTestCase(Tests)).wasSuccessful())
    if not args.fixture or not args.label or not args.output or not math.isfinite(args.duration) or not 0 < args.duration <= 120:
        parser.error("--fixture HOST.json --label baseline|candidate --output NEW.json required; duration (0,120]")
    try:
        resources.schedule(args.duration, args.interval)
    except ValueError as error:
        parser.error(str(error))
    return capture(args)


if __name__ == "__main__":
    sys.exit(main())
