#!/usr/bin/env python3
"""Opt-in isolated real-host idle A/B and native stage capture; no containers.

Usage: python3 scripts/audit-live-host.py --run --wait-parent-root /tmp/cw-audit-XXX --output NEW_DIR
Defaults: verified existing first-batch baseline, copied current candidate, 20 s
hidden/no-client idle and 20 s static-client idle per host; separate candidate
stage host with 120 PID-targeted key pairs then 75 s animation. All hosts use new
HOME/TMPDIR/runtime/control sockets and empty configs. Tracing is OFF for idle.
Only owned hosts/probes are stopped. Never build, download, start/stop a runtime,
create a container, change networking or inject into the parent integration.
Parent-root removal gates all app launches; wait has a 240-second deadline.
CPU/wake/RSS/footprint are resource proxies, NOT machine watts/battery evidence.
"""
import argparse
import ctypes
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parent.parent


def imported(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / "scripts" / (name.replace("_", "-") + ".py"))
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


bench = imported("audit_bench")
native = imported("audit_native_probe")


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def app_action(pid, action):
    """Public AppKit targeting an owned child only; never global AppleScript/input."""
    ctypes.CDLL("/System/Library/Frameworks/AppKit.framework/AppKit")
    objc = ctypes.CDLL("/usr/lib/libobjc.A.dylib")
    objc.objc_getClass.argtypes, objc.objc_getClass.restype = [ctypes.c_char_p], ctypes.c_void_p
    objc.sel_registerName.argtypes, objc.sel_registerName.restype = [ctypes.c_char_p], ctypes.c_void_p
    address = ctypes.cast(objc.objc_msgSend, ctypes.c_void_p).value
    get_app = ctypes.CFUNCTYPE(ctypes.c_void_p, ctypes.c_void_p, ctypes.c_void_p, ctypes.c_int)(address)
    app = get_app(objc.objc_getClass(b"NSRunningApplication"), objc.sel_registerName(b"runningApplicationWithProcessIdentifier:"), pid)
    if not app:
        return False
    if action == "activate":
        call = ctypes.CFUNCTYPE(ctypes.c_bool, ctypes.c_void_p, ctypes.c_void_p, ctypes.c_ulong)(address)
        return call(app, objc.sel_registerName(b"activateWithOptions:"), 2)
    call = ctypes.CFUNCTYPE(ctypes.c_bool, ctypes.c_void_p, ctypes.c_void_p)(address)
    return call(app, objc.sel_registerName(b"terminate"))


def stop_host(child):
    if child.poll() is not None:
        return {"method": "already_exited", "exit_code": child.returncode}
    try:
        requested = app_action(child.pid, "terminate")
        child.wait(timeout=8)
        return {"method": "AppKit_terminate", "request_accepted": requested, "exit_code": child.returncode}
    except (subprocess.TimeoutExpired, OSError, RuntimeError, AttributeError):
        child.terminate()
        try:
            child.wait(timeout=3)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait(timeout=3)
        return {"method": "owned_child_signal_fallback", "exit_code": child.returncode}


def stop_probe(child):
    if child is not None and child.poll() is None:
        child.terminate()
        try:
            child.wait(timeout=3)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait(timeout=3)


def wait_ready(child, runtime):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        if child.poll() is not None:
            raise RuntimeError(f"owned host exited during startup ({child.returncode})")
        sockets = [path for path in runtime.glob("wayland-*") if not path.name.endswith(".lock") and path.is_socket()]
        if sockets:
            return sockets[0]
        time.sleep(.1)
    raise TimeoutError("owned host socket startup deadline")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--run", action="store_true")
    parser.add_argument("--wait-parent-root", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, default=ROOT / "target/release/cocoa-way")
    parser.add_argument("--candidate-sha256", default="3fc86d0101dd26f8b0cbd139ed9d52435888d6ee88f2e660f8dc04982c100416")
    parser.add_argument("--idle-seconds", type=int, default=20)
    parser.add_argument("--stage-seconds", type=int, default=75)
    args = parser.parse_args()
    if not args.run or sys.platform != "darwin" or not 5 <= args.idle_seconds <= 60 or not 60 <= args.stage_seconds <= 120:
        parser.error("--run requires macOS, idle 5..60 s and stage 60..120 s")
    output = args.output.resolve()
    output.mkdir(mode=0o700, parents=True, exist_ok=False)
    baseline_record = json.loads((ROOT / "target/audit-baseline-3e3d6b91/provenance.json").read_text())
    baseline = Path(baseline_record["binary"])
    if baseline_record["status"] != "ok" or digest(baseline) != baseline_record["binary_sha256"]:
        raise ValueError("verified baseline provenance does not match existing binary")
    if digest(args.candidate) != args.candidate_sha256:
        raise ValueError("candidate differs from parent-captured release hash")
    candidate = output / "candidate-cocoa-way"
    shutil.copy2(args.candidate, candidate)
    if digest(candidate) != args.candidate_sha256:
        raise ValueError("immutable candidate copy hash mismatch")
    probe_record = json.loads((ROOT / "target/audit-bench-20261015-frozen/native-probe-provenance.json").read_text())
    probe = Path(probe_record["binary"])
    if digest(probe) != probe_record["binary_sha256"]:
        raise ValueError("native probe provenance mismatch")
    report = {"schema": 1, "kind": "isolated_actual_host_audit", "status": "waiting_parent_cleanup",
        "baseline_provenance": baseline_record, "candidate": str(candidate), "candidate_sha256": digest(candidate),
        "candidate_source_attestation": "parent-reported latest release build; hash verified, no mtime/version inference",
        "native_probe_provenance": probe_record, "parent_root": str(args.wait_parent_root),
        "hosts": [], "containers_created": False, "runtime_lifecycle_modified": False,
        "measured_watts": None, "battery_impact": None}
    report["limitations"] = ["Parent-reported privacy curtain: no visible GUI, physical pointer or photon validation.",
        "Single sequential 20-second interval set per idle condition; uncontrolled audit/build load and no causal significance claim.",
        "Host Forwarded observations require protocol tags and actual probe receipt evidence before claiming delivery.",
        "AppKit terminate exit 0 does not prove trace writer flush; inspect final stats and malformed/truncated tail explicitly."]
    def save():
        (output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    save()
    print("Waiting only for parent fixture cleanup: " + str(args.wait_parent_root), flush=True)
    deadline = time.monotonic() + 240
    while args.wait_parent_root.exists():
        if time.monotonic() >= deadline:
            raise TimeoutError("parent integration not finished; no host launched")
        time.sleep(1)
    report["status"] = "running"
    save()

    def scenario(label, binary, traced=False):
        with tempfile.TemporaryDirectory(prefix="cw-probe-", dir="/tmp") as directory:
            root = Path(directory)
            home, runtime, temporary = root / "home", root / "display", root / "tmp"
            config = home / ".config/cocoa-way"
            config.mkdir(parents=True)
            temporary.mkdir()
            runtime.mkdir(mode=0o700)
            (config / "connections.toml").write_text("")
            (config / "container-sessions.toml").write_text("")
            env = {"PATH": "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin", "HOME": str(home),
                "TMPDIR": str(temporary) + "/", "XKB_CONFIG_ROOT": "/opt/homebrew/share/X11/xkb",
                "COCOA_WAY_CONTROL_SOCKET": str(root / "control.sock"), "COCOA_WAY_DISPLAY_RUNTIME_DIR": str(runtime),
                "COCOA_WAY_PRESENTATION": "desktop", "COCOA_WAY_NATIVE_FRAME_SOURCE": "0",
                "RUST_LOG": "cocoa_way=warn,smithay=warn", "PYTHONDONTWRITEBYTECODE": "1"}
            trace = output / (label + "-host.jsonl")
            if traced:
                env["COCOA_WAY_LATENCY_TRACE"] = str(trace)
            entry = {"label": label, "binary": str(binary), "binary_sha256": digest(binary), "trace_enabled": traced,
                "presentation": "desktop", "native_frame_source": False, "runtime": str(runtime), "client": "native verified input-latency-probe"}
            report["hosts"].append(entry)
            save()
            with (output / (label + ".app.log")).open("w") as log:
                child = subprocess.Popen([str(binary)], env=env, stdout=log, stderr=subprocess.STDOUT)
                client = None
                try:
                    socket = wait_ready(child, runtime)
                    fixture = {"host_pid": child.pid, "host_executable": str(binary), "host_sha256": entry["binary_sha256"],
                        "wayland_socket": str(socket), "host_trace": str(trace), "probe_binary": str(probe),
                        "probe_sha256": probe_record["binary_sha256"], "exclusive_probe_host": True, "keyboard_focus_ready": True}
                    fixture_path = output / (label + "-fixture.json")
                    fixture_path.write_text(json.dumps(fixture, indent=2) + "\n")
                    entry.update(pid=child.pid, socket=str(socket), fixture=str(fixture_path))
                    save()
                    if not traced:
                        time.sleep(3)
                        for condition in ("hidden_no_client", "static_client"):
                            if condition == "static_client":
                                client = subprocess.Popen([str(probe), "--socket", str(socket), "--static", "--duration",
                                    str(args.idle_seconds + 10)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                                time.sleep(3)
                                if client.poll() is not None:
                                    raise RuntimeError("static probe exited before idle sample")
                            target = output / (label + "-" + condition + ".json")
                            command = [sys.executable, str(ROOT / "scripts/audit-host-resources.py"), "--fixture", str(fixture_path),
                                "--label", label, "--duration", str(args.idle_seconds), "--interval", "1", "--output", str(target)]
                            code, text = bench.command(command, timeout=args.idle_seconds + 20)
                            if code:
                                raise RuntimeError("attached real host resource sampler failed")
                            print(text.strip(), flush=True)
                        stop_probe(client)
                    else:
                        entry["own_host_activation_accepted"] = app_action(child.pid, "activate")
                        command = [sys.executable, str(ROOT / "scripts/audit-native-probe.py"), "--run", "--fixture", str(fixture_path),
                            "--output", str(output / "targeted-input"), "--keys", "120", "--interval", ".2"]
                        code, text = bench.command(command, timeout=50)
                        entry["targeted_input_exit_code"] = code
                        (output / "targeted-input-command.log").write_text(text)
                        with (output / "animation-native-probe.jsonl").open("w") as probe_log:
                            client = subprocess.Popen([str(probe), "--socket", str(socket), "--continuous-animation", "--duration",
                                str(args.stage_seconds), "--log-stderr", "--tag", "audit-stage-native"], env=env,
                                stdout=subprocess.DEVNULL, stderr=probe_log)
                            client.wait(timeout=args.stage_seconds + 8)
                            entry["animation_exit_code"] = client.returncode
                            if client.returncode:
                                raise RuntimeError("animation probe failed")
                finally:
                    stop_probe(client)
                    entry["shutdown"] = stop_host(child)
                    save()
            if traced:
                code, text = bench.command([sys.executable, str(ROOT / "scripts/audit-stage-latency.py"), str(trace),
                    "--output", str(output / "real-host-stages.json")], timeout=30)
                if code:
                    (output / "stage-analysis-error.log").write_text(text)
                    raise RuntimeError("real stage analyzer failed")
    try:
        scenario("baseline", baseline)
        scenario("candidate", candidate)
        scenario("stage", candidate, True)
        report["status"] = "ok"
    except BaseException as error:
        report.update(status="error", error_type=type(error).__name__, error=str(error)[:256])
        raise
    finally:
        save()
    print("Real host evidence: " + str(output), flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
