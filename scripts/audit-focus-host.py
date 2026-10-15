#!/usr/bin/env python3
"""Opt-in <=85 s focus diagnostic on ONE new owned host; no runtime/guest.

Usage: python3 scripts/audit-focus-host.py --run --output EXISTING_HELPER_DIR
Compile audit-own-window.swift into EXISTING_HELPER_DIR/audit-own-window first.
Defaults to the immutable 3fc86d candidate and verified native probe from prior
capture. A later build requires BOTH --candidate FILE --candidate-sha256 HASH;
never infer its provenance from target/release, version text, or modification time.
Starts private HOME/TMPDIR/runtime/control, describes only its own windows, then
requires the exact ID of its uniquely titled display window before focus/click/key
events. No global input, permission prompts, screenshots or physical claims.
Actual probe receipts, not posting/focus-return values, determine success.
This is native macOS, NOT Linux guest receipt or a 5 ms latency-goal assessment.
--inspect-seconds 0..45 leaves a bounded owned-app CUA focus inspection window
before the exact-PID stimulus. Select the exact emitted bundle PATH, not a bundle
ID shared by retained copies; obtain fresh AX/screenshot, then a selected-app
content click if explicitly authorized. No other application input is allowed.
"""
import argparse
import ctypes
import importlib.util
import json
from pathlib import Path
import plistlib
import shutil
import subprocess
import sys
import tempfile
import time

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("audit_live", ROOT / "scripts/audit-live-host.py")
live = importlib.util.module_from_spec(spec)
spec.loader.exec_module(live)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--run", action="store_true")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--inspect-seconds", type=int, default=0)
    parser.add_argument("--candidate", type=Path)
    parser.add_argument("--candidate-sha256")
    parser.add_argument("--bundle-id", default="com.jxz.cocoa-way.audit")
    args = parser.parse_args()
    if not args.run or sys.platform != "darwin" or not 0 <= args.inspect_seconds <= 45:
        parser.error("explicit --run on macOS required; inspect-seconds must be 0..45")
    if bool(args.candidate) != bool(args.candidate_sha256):
        parser.error("--candidate and --candidate-sha256 must be provided together")
    if not args.bundle_id.startswith("com.jxz.cocoa-way.audit") or len(args.bundle_id) > 128 or not all(character.isascii() and (character.isalnum() or character in ".-") for character in args.bundle_id):
        parser.error("--bundle-id must be an ASCII com.jxz.cocoa-way.audit namespace identifier")
    output = args.output.resolve()
    helper = output / "audit-own-window"
    if not helper.is_file() or (output / "focus-report.json").exists():
        parser.error("compiled helper must exist and focus-report.json must not exist")
    original = json.loads((ROOT / "target/audit-live-host-20261015/report.json").read_text())
    original_host = args.candidate.resolve() if args.candidate else Path(original["candidate"])
    expected_host_hash = args.candidate_sha256 if args.candidate else original["candidate_sha256"]
    probe = Path(original["native_probe_provenance"]["binary"])
    if live.digest(original_host) != expected_host_hash or live.digest(probe) != original["native_probe_provenance"]["binary_sha256"]:
        raise ValueError("immutable host/probe provenance mismatch")
    bundle = output / "Cocoa-Way Audit.app"
    executable_directory = bundle / "Contents/MacOS"
    executable_directory.mkdir(parents=True, exist_ok=False)
    host = executable_directory / "cocoa-way"
    shutil.copy2(original_host, host)
    template = ROOT / "target/supplychain/Cocoa-Way.app/Contents/Info.plist"
    metadata = plistlib.loads(template.read_bytes()) if template.exists() else {}
    metadata.update(CFBundleIdentifier=args.bundle_id, CFBundleExecutable="cocoa-way",
        CFBundleName="Cocoa-Way Audit", CFBundlePackageType="APPL", NSPrincipalClass="NSApplication",
        NSHighResolutionCapable=True, LSUIElement=False)
    metadata.pop("LSEnvironment", None)
    (bundle / "Contents/Info.plist").write_bytes(plistlib.dumps(metadata))
    if live.digest(host) != expected_host_hash:
        raise ValueError("isolated bundle changed candidate code hash")
    report = {"schema": 1, "kind": "exact_owned_window_focus_diagnostic", "candidate_sha256": live.digest(host),
        "helper_sha256": live.digest(helper), "status": "running", "global_input_posted": False,
        "visible_physical_validation": False, "guest_latency": None, "five_ms_goal_assessed": False,
        "isolated_bundle": str(bundle), "bundle_identifier": args.bundle_id,
        "native_probe_sha256": live.digest(probe), "driver_source_sha256": live.digest(Path(__file__)),
        "helper_source_sha256": live.digest(ROOT / "scripts/audit-own-window.swift")}
    def save():
        (output / "focus-report.json").write_text(json.dumps(report, indent=2) + "\n")
    save()
    with tempfile.TemporaryDirectory(prefix="cw-focus-", dir="/tmp") as directory:
        root = Path(directory)
        config = root / "home/.config/cocoa-way"
        config.mkdir(parents=True)
        (config / "connections.toml").write_text("")
        (config / "container-sessions.toml").write_text("")
        temporary, runtime = root / "tmp", root / "display"
        temporary.mkdir()
        runtime.mkdir(mode=0o700)
        env = {"PATH": "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin", "HOME": str(root / "home"),
            "TMPDIR": str(temporary) + "/", "COCOA_WAY_DISPLAY_RUNTIME_DIR": str(runtime),
            "COCOA_WAY_CONTROL_SOCKET": str(root / "control.sock"), "COCOA_WAY_LATENCY_TRACE": str(output / "focus-host.jsonl"),
            "COCOA_WAY_PRESENTATION": "desktop", "COCOA_WAY_NATIVE_FRAME_SOURCE": "0",
            "XKB_CONFIG_ROOT": "/opt/homebrew/share/X11/xkb", "RUST_LOG": "cocoa_way=warn", "PYTHONDONTWRITEBYTECODE": "1"}
        client = None
        with (output / "focus.app.log").open("w") as app_log, (output / "focus-native-probe.jsonl").open("w") as probe_log:
            child = subprocess.Popen([str(host)], env=env, stdout=app_log, stderr=subprocess.STDOUT)
            try:
                socket = live.wait_ready(child, runtime)
                client = subprocess.Popen([str(probe), "--socket", str(socket), "--static", "--duration", str(18 + args.inspect_seconds),
                    "--log-stderr", "--tag", "audit-exact-window-focus"], env=env, stdout=subprocess.DEVNULL, stderr=probe_log)
                time.sleep(2)
                if client.poll() is not None:
                    raise RuntimeError("probe exited before focus diagnostic")
                library = ctypes.CDLL("/usr/lib/libproc.dylib")
                library.proc_pid_rusage.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_void_p]
                library.proc_pid_rusage.restype = ctypes.c_int
                identity = live.native.process_identity(library, child.pid)
                fixture = {"host_pid": child.pid, "host_executable": str(host), "host_sha256": live.digest(host),
                    "host_loaded_uuid": identity[0].hex(), "host_process_start_abstime": identity[1],
                    "exclusive_probe_host": True, "expected_window_title": "Cocoa-Way"}
                fixture_path = output / "focus-fixture.json"
                fixture_path.write_text(json.dumps(fixture, indent=2) + "\n")
                if args.inspect_seconds:
                    print(json.dumps({"inspection_ready": True, "fixture": str(fixture_path),
                        "bundle_identifier": report["bundle_identifier"], "host_pid": child.pid,
                        "exact_bundle_path": str(bundle),
                        "seconds_before_stimulus": args.inspect_seconds}), flush=True)
                    time.sleep(args.inspect_seconds)
                code, text = live.bench.command([str(helper), "--fixture", str(fixture_path), "--describe"], timeout=8)
                (output / "window-description.json").write_text(text)
                if code:
                    raise RuntimeError("own window enumeration refused")
                windows = [row for row in json.loads(text)["windows"] if row["title"] == fixture["expected_window_title"]]
                if len(windows) != 1:
                    raise RuntimeError("unique owned display title/window ID unavailable")
                command = [str(helper), "--fixture", str(fixture_path), "--focus", "--window-id", str(windows[0]["window_id"]), "--keys", "20"]
                code, text = live.bench.command(command, timeout=12)
                (output / "focus-action.json").write_text(text)
                report.update(host_pid=child.pid, target_window_id=windows[0]["window_id"], focus_helper_exit_code=code)
                client.wait(timeout=20)
                report["probe_exit_code"] = client.returncode
                report["status"] = "captured" if code == 0 else "focus_refused"
            except BaseException as error:
                report.update(status="error", error_type=type(error).__name__, error=str(error)[:256])
                raise
            finally:
                live.stop_probe(client)
                report["shutdown"] = live.stop_host(child)
                save()
    records = [json.loads(line) for line in (output / "focus-native-probe.jsonl").read_text().splitlines() if line.strip()]
    final = next((row for row in reversed(records) if row.get("record") == "stop"), None)
    report["probe_final_record"] = final
    report["actual_native_probe_key_events"] = final.get("key_events") if final else None
    report["actual_native_probe_button_events"] = final.get("button_events") if final else None
    report["native_focus_delivery_observed"] = bool(final and final.get("key_events") == 40)
    save()
    live.bench.command([sys.executable, str(ROOT / "scripts/audit-stage-latency.py"), str(output / "focus-host.jsonl"),
        "--output", str(output / "focus-stages.json")], timeout=10)
    print(json.dumps({key: report[key] for key in ("status", "actual_native_probe_key_events", "actual_native_probe_button_events", "native_focus_delivery_observed", "shutdown")}))
    return 0


if __name__ == "__main__":
    sys.exit(main())
