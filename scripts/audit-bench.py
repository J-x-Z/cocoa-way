#!/usr/bin/env python3
"""Bounded, non-GUI audit inventory and CPU-copy/idle experiments.

Never starts a runtime/VM/container or reads process arguments/environment/configs.
Only owned compiler/probe/benchmark children may be terminated at a deadline.
Reports CPU stage elapsed time, NOT GPU execution, presentation or host/guest latency.

Usage: python3 scripts/audit-bench.py --output target/audit-bench-NEW
       python3 scripts/audit-bench.py --inventory-only --output target/audit-inventory-NEW
       python3 scripts/audit-bench.py --self-test
Output directory must be new. Defaults: three geometries, three arms, 3x200 samples,
20 warmups and three 4-second synthetic idle trials/arm. Synthetic idle never
substitutes for actual compositor idle/power measurements.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import re
import selectors
import signal
import shutil
import subprocess
import sys
import time
import unittest

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parent.parent
MAX_OUTPUT = 1_048_576


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    loaded = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(loaded)
    return loaded


resources = module("audit_resources", ROOT / "scripts/sample-process-resources.py")


def command(args, timeout=10, limit=MAX_OUTPUT, tick=None):
    """Bound both subprocess output and wall time; never emit captured stderr."""
    with subprocess.Popen(args, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, start_new_session=True,
                          env={**os.environ, "LC_ALL": "C"}) as child:
        data = bytearray()
        deadline = time.monotonic() + timeout
        with selectors.DefaultSelector() as selector:
            selector.register(child.stdout, selectors.EVENT_READ)
            try:
                while selector.get_map():
                    if tick is not None:
                        tick()
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        raise TimeoutError("owned subprocess deadline exceeded")
                    for key, _ in selector.select(min(remaining, .1)):
                        chunk = os.read(key.fileobj.fileno(), min(65536, limit + 1 - len(data)))
                        if not chunk:
                            selector.unregister(key.fileobj)
                        else:
                            data.extend(chunk)
                            if len(data) > limit:
                                raise ValueError("owned subprocess output limit exceeded")
                child.wait(timeout=max(.01, deadline - time.monotonic()))
            except BaseException:
                # A new session confines cleanup to this owned probe/compiler tree.
                try:
                    os.killpg(child.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                child.wait()
                raise
        return child.returncode, data.decode("utf-8", errors="replace")


def probe(args, pattern=None):
    try:
        code, output = command(args, timeout=8)
        matches = re.findall(pattern, output) if pattern else []
        return {"exit_code": code, "version_or_value": matches[0] if matches else None}
    except (OSError, TimeoutError, ValueError) as error:
        return {"error_type": type(error).__name__}


def inventory():
    names = ("rustc", "cargo", "python3", "clang", "xcrun", "waypipe", "container", "docker",
             "orbctl", "podman", "colima", "limactl", "qemu-system-aarch64", "weston", "niri",
             "foot", "firefox", "powermetrics", "sample", "vmmap")
    result = {"kind": "capabilities", "platform": sys.platform, "machine": os.uname().machine,
              "executables": {name: shutil.which(name) for name in names},
              "runtime_or_app_started": False, "raw_process_arguments_recorded": False,
              "environment_or_config_recorded": False}
    versions = {"rustc": ["--version"], "cargo": ["--version"], "python3": ["--version"],
                "waypipe": ["--version"], "container": ["--version"], "docker": ["--version"],
                "orbctl": ["version"], "qemu-system-aarch64": ["--version"]}
    result["versions"] = {name: probe([result["executables"][name], *args], r"\b(\d+\.\d+\.\d+)\b")
                          for name, args in versions.items() if result["executables"][name]}
    if sys.platform == "darwin":
        result["macos"] = probe(["/usr/bin/sw_vers", "-productVersion"], r"([0-9.]+)")
        result["macos_build"] = probe(["/usr/bin/sw_vers", "-buildVersion"], r"([0-9]+[A-Z][0-9]+)")
        result["hardware"] = {key: probe(["/usr/sbin/sysctl", "-n", key], r"([A-Za-z0-9,]+)")
                              for key in ("hw.model", "hw.ncpu", "hw.memsize")}
        result["metal_compiler_lookup"] = probe(["/usr/bin/xcrun", "--find", "metal"])
    if result["executables"]["container"]:
        code, output = command([result["executables"]["container"], "system", "status"], timeout=8)
        result["apple_container"] = {"exit_code": code, "status":
            "not_running_not_registered" if "not running and not registered" in output else
            "running" if code == 0 and "running" in output.lower() else "unknown"}
    if result["executables"]["docker"]:
        code, output = command([result["executables"]["docker"], "version", "--format", "{{json .Server.Version}}"], timeout=8)
        result["docker_server"] = {"exit_code": code, "reachable": code == 0,
            "version": re.findall(r'"(\d+\.\d+\.\d+)"', output)[0] if code == 0 and re.search(r'"\d+\.\d+\.\d+"', output) else None}
    code, output = command(["/bin/ps", "-axo", "pid=,comm="], timeout=8)
    selected = []
    for row in output.splitlines():
        parts = row.strip().split(None, 1)
        if len(parts) == 2:
            name = Path(parts[1]).name
            if re.fullmatch(r"(?i)(cocoa-way|waypipe|OrbStack|container-apiserver|container-runtime-linux|qemu-system-aarch64)", name):
                selected.append({"pid": int(parts[0]), "executable_basename": name})
    result["selected_processes"] = selected
    result["process_inventory_exit_code"] = code
    result["limitations"] = ["No container/image list or inspect: runtime services unavailable or unverified.",
        "Installed Metal compiler does not prove device execution/presentation.",
        "No GUI, VM, container, network, VPN, screen, input or existing-session modifications.",
        "No privilege escalation or machine-wide power sampling."]
    return result


def sampled_benchmark(binary, args):
    # The child prints only nine bounded aggregate records for one geometry.
    with subprocess.Popen([str(binary), *args], stdout=subprocess.PIPE, stderr=subprocess.PIPE) as child:
        rows = []
        started = time.monotonic()
        try:
            while child.poll() is None:
                if time.monotonic() - started > 120 or len(rows) >= 301:
                    raise TimeoutError("bounded benchmark observation window exceeded")
                if sys.platform == "darwin":
                    try:
                        row = resources.read_ps(child.pid)
                        if rows:
                            row.update(resources.cpu_interval(rows[-1], row))
                        rows.append(row)
                    except ProcessLookupError:
                        break
                time.sleep(.2)
            stdout, stderr = child.communicate(timeout=5)
            if child.returncode:
                raise RuntimeError(f"benchmark failed with exit {child.returncode}; stderr not disclosed")
            if len(stdout) > MAX_OUTPUT or len(stderr) > MAX_OUTPUT:
                raise ValueError("benchmark output exceeded bound")
        except BaseException:
            child.kill()
            child.wait()
            raise
    records = [json.loads(line) for line in stdout.splitlines()]
    return {"cpu_copy": records, "observed_process_resources": {
        "scope": "all_three_arms_resident_together; NOT per-arm RSS; ps observer overhead included",
        "clock": "host_monotonic_ps_bracket_midpoint", "pid_identity": "owned unreaped child",
        "samples": rows, "rss_kib": resources.quantiles([row["rss_kib"] for row in rows]),
        "cpu_single_core_percent": resources.quantiles([row["cpu_single_core_percent"] for row in rows[1:]])}}


def idle_run(binary, arm, seconds):
    with subprocess.Popen([str(binary), "idle", arm, str(seconds + 1)], stdin=subprocess.PIPE,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True) as child:
        try:
            with selectors.DefaultSelector() as selector:
                selector.register(child.stdout, selectors.EVENT_READ)
                if not selector.select(5) or child.stdout.readline() != "ready\n":
                    raise RuntimeError("idle harness did not become ready")
            child.stdin.write("go\n")
            child.stdin.flush()
            sampled = resources.sample(child.pid, seconds, .5) if sys.platform == "darwin" else None
            stdout, stderr = child.communicate(timeout=seconds + 5)
            if child.returncode or len(stdout) > 16384 or stderr:
                raise RuntimeError("idle child failed; stderr not disclosed")
            return {"experiment": json.loads(stdout), "process_resources": sampled}
        except BaseException:
            child.kill()
            child.wait()
            raise


def enrich(record):
    count = record["samples"]
    # Payload throughput counts one byte copied once, not both read+write traffic.
    record["upload_payload_GiB_s"] = record["uploaded_bytes_total"] / record["upload_total_ns"] * 1e9 / 2**30
    record["snapshot_payload_GiB_s"] = (record["snapshot_bytes_per_frame"] * count / record["prepare_total_ns"] * 1e9 / 2**30
                                          if record["snapshot_bytes_per_frame"] else None)
    record["payload_bytes_per_frame"] = record["snapshot_bytes_per_frame"] + record["upload_bytes_per_frame"]
    return record


def run(args):
    output = args.output.resolve()
    output.mkdir(mode=0o700, parents=True, exist_ok=False)
    def save(name, value):
        (output / name).write_text(json.dumps(value, indent=2, allow_nan=False) + "\n", encoding="utf-8")
    save("inventory.json", inventory())
    if args.inventory_only:
        print(f"Read-only inventory: {output / 'inventory.json'}")
        return
    source_paths = [ROOT / "scripts/audit-shm.rs", ROOT / "scripts/audit-bench.py",
                    ROOT / "scripts/sample-process-resources.py", ROOT / "src/render.rs",
                    ROOT / "src/texture_lifetime.rs"]
    # Track local modules included by the actual ownership model, too.
    for parent in source_paths:
        if parent.suffix == ".rs":
            for relative in re.findall(r'#\[path = "([^"]+)"\]', parent.read_text(encoding="utf-8")):
                dependency = (parent.parent / relative).resolve()
                if dependency not in source_paths:
                    source_paths.append(dependency)
    import hashlib
    hashes = {}
    frozen = output / "source_snapshot"
    for path in source_paths:
        data = path.read_bytes()
        relative = path.relative_to(ROOT)
        hashes[str(relative)] = hashlib.sha256(data).hexdigest()
        copy = frozen / relative
        copy.parent.mkdir(parents=True, exist_ok=True)
        copy.write_bytes(data)
    binary = output / "audit-shm"
    compiler = shutil.which("rustc")
    if not compiler:
        raise RuntimeError("rustc unavailable; inventory saved")
    flags = ["--edition=2024", "-C", "opt-level=3", "-C", "codegen-units=1"]
    compile_source = frozen / "scripts/audit-shm.rs"
    code, log = command([compiler, *flags, str(compile_source), "-o", str(binary)], timeout=60)
    (output / "compile.log").write_text(log, encoding="utf-8")
    if code:
        raise RuntimeError("independent rustc compilation failed; see compile.log")
    test_binary = output / "audit-tests"
    code, log = command([compiler, *flags, "--test", str(compile_source), "-o", str(test_binary)], timeout=60)
    (output / "test-compile.log").write_text(log, encoding="utf-8")
    if code:
        raise RuntimeError("standalone tests did not compile")
    code, log = command([str(test_binary)], timeout=30)
    (output / "tests.log").write_text(log, encoding="utf-8")
    if code:
        raise RuntimeError("standalone tests failed")
    report = {"schema": 1, "kind": "audit_cpu_microbenchmark", "status": "in_progress",
              "host_wall_time_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
              "clock": "single_host_rust_Instant_elapsed; NOT trace host_ca or guest clock",
              "percentile_method": "nearest_rank", "source_sha256": hashes,
              "compiled_sources": "frozen source_snapshot directory; live workspace may continue changing",
              "compiled_binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "command_arguments": {"sizes": args.size, "samples_per_round": args.samples,
                  "rounds": args.rounds, "warmup_per_arm": max(args.warmup, 4),
                  "idle_seconds_observed_by_ps": args.idle_seconds,
                  "idle_seconds_per_child": args.idle_seconds + 1, "idle_repeats": args.idle_repeats},
              "compiler_flags": flags, "cases": [], "idle": [],
              "screen_presentation": None, "gpu_execution": None, "host_guest_latency": None,
              "limitations": ["Independent static mmap/copy model, not production SHM/SIGBUS guard or Metal replaceRegion.",
                  "Production completion-owned TextureVersions/FrameGate included directly; GPU completion simulated after 3 submissions; Metal cache budget not invoked by copy model.",
                  "Unsafe direct-single baseline is not a supported optimization or exact historic implementation.",
                  "Arms interleaved with deterministic rotating order; shared source remains warm; host not quiesced or CPU pinned.",
                  "Retained capacities exclude allocator/object metadata, mapping, driver textures and GPU memory.",
                  "ABGR snapshot throughput includes conversion; conversion read/write traffic not included in payload byte count.",
                  "Native BGRA A/B isolates snapshot copying; ABGR also changes from direct row conversion to snapshot in-place conversion.",
                  "ps CPU is centisecond-quantized; synthetic idle is not compositor idle or measured machine power."]}
    save("results.json", report)
    for width, height in args.size:
        experiment = sampled_benchmark(binary, ["bench", str(width), str(height), str(args.samples), str(args.rounds),
                                                  str(args.warmup), str(output / "source-map")])
        for row in experiment["cpu_copy"]:
            enrich(row)
            ns = row["cpu_total_ns"]
            print(f"{width}x{height} {row['case']} {row['arm']}: n={row['samples']} "
                  f"CPU ms P50/P95/P99={ns['p50']/1e6:.4f}/{ns['p95']/1e6:.4f}/{ns['p99']/1e6:.4f}; "
                  f"retained={row['retained_payload_capacity_bytes']/2**20:.2f} MiB")
        report["cases"].append(experiment)
        save("results.json", report)
    for repeat in range(args.idle_repeats):
        for arm in (("event", "poll") if repeat % 2 == 0 else ("poll", "event")):
            report["idle"].append(idle_run(binary, arm, args.idle_seconds))
            save("results.json", report)
    report["idle_summary"] = {}
    for arm in ("event", "poll"):
        runs = [r["experiment"] for r in report["idle"] if r["experiment"]["arm"] == arm]
        rows = [r for r in runs if r["macos_proc_pid_rusage_v0"] is not None]
        report["idle_summary"][arm] = {
            "scope": "per-run rates; small n, NOT individual-wake latency percentiles",
            "wait_returns_per_second": resources.quantiles([r["wait_returns"] * 1e9 / r["wall_ns"] for r in runs]),
            **{key + "_per_second": resources.quantiles([r["macos_proc_pid_rusage_v0"][key] * 1e9 / r["wall_ns"] for r in rows])
               for key in ("interrupt_wakes", "package_idle_wakes", "cpu_ns")}}
    final_hashes = {str(path.relative_to(ROOT)): hashlib.sha256(path.read_bytes()).hexdigest() for path in source_paths}
    report["sources_changed_during_run"] = hashes != final_hashes
    report["source_sha256_at_end"] = final_hashes
    report["finished_wall_time_utc"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    report["status"] = "ok"
    save("results.json", report)
    print(f"Evidence: {output}")


class Tests(unittest.TestCase):
    def test_bounded_subprocess(self):
        self.assertEqual(command([sys.executable, "-c", "print('test')"]), (0, "test\n"))
        with self.assertRaises(ValueError):
            command([sys.executable, "-c", "print('x'*10000)"], limit=100)
        with self.assertRaises(TimeoutError):
            command([sys.executable, "-c", "import time;time.sleep(2)"], timeout=.05)
    def test_quantiles_and_bandwidth(self):
        self.assertEqual(resources.quantiles(range(1, 101)), {"count": 100, "p50": 50, "p95": 95, "p99": 99})
        row = enrich({"samples": 2, "uploaded_bytes_total": 2**30, "upload_total_ns": 10**9,
                      "snapshot_bytes_per_frame": 0, "prepare_total_ns": 1, "upload_bytes_per_frame": 8})
        self.assertEqual(row["upload_payload_GiB_s"], 1)
        self.assertIsNone(row["snapshot_payload_GiB_s"])
    def test_nonzero_probe_not_success(self):
        self.assertEqual(probe([sys.executable, "-c", "raise SystemExit(3)"])["exit_code"], 3)


def size(text):
    try:
        width, height = map(int, text.split("x"))
        if not (64 <= width <= 3840 and 64 <= height <= 2160):
            raise ValueError()
        return width, height
    except ValueError:
        raise argparse.ArgumentTypeError("size must be WIDTHxHEIGHT within 64x64..3840x2160") from None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "target" / ("audit-bench-" + time.strftime("%Y%m%d-%H%M%S")))
    parser.add_argument("--inventory-only", action="store_true")
    parser.add_argument("--size", type=size, action="append")
    parser.add_argument("--samples", type=int, default=200)
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--warmup", type=int, default=20)
    parser.add_argument("--idle-seconds", type=int, default=3)
    parser.add_argument("--idle-repeats", type=int, default=3)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return int(not unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromTestCase(Tests)).wasSuccessful())
    args.size = args.size or [(1280, 720), (1920, 1080), (3840, 2160)]
    if not (1 <= args.samples <= 2000 and 1 <= args.rounds <= 10 and args.samples * args.rounds <= 6000
            and 0 <= args.warmup <= 200 and 1 <= args.idle_seconds <= 29 and 0 <= args.idle_repeats <= 5 and len(args.size) <= 6):
        parser.error("bounded limits: samples 1..2000, rounds 1..10/product <=6000, warmup <=200, idle seconds 1..29/repeats 0..5, sizes <=6")
    try:
        run(args)
    except (OSError, ValueError, RuntimeError, TimeoutError, subprocess.SubprocessError) as error:
        print(f"Audit not complete: {type(error).__name__}: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
