#!/usr/bin/env python3
"""Bounded owned-window latency fixture. No containers, global input or permissions.

Runs the real native SHM probe directly or over same-host Waypipe. The coordinator
uses CUA to focus and stimulate only this owned worker. Host and probe clocks remain
separate. CPU submission and callback timestamps are not screen presentation.
"""
import argparse
import hashlib
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


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def stop(child):
    if child is not None and child.poll() is None:
        child.terminate()
        try:
            child.wait(timeout=3)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait(timeout=3)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--animated", action="store_true")
    parser.add_argument("--waypipe", action="store_true")
    parser.add_argument("--waypipe-path", type=Path, default=Path("/opt/homebrew/bin/waypipe"))
    parser.add_argument("--expect-idle", action="store_true")
    parser.add_argument("--native-frame-source", action="store_true")
    parser.add_argument("--seconds", type=int, default=60)
    args = parser.parse_args()
    if not 10 <= args.seconds <= 180:
        parser.error("seconds must be 10..180")
    binary = args.binary.resolve(strict=True)
    probe = (ROOT / "target/release/input-latency-probe").resolve(strict=True)
    args.output.mkdir(parents=True, mode=0o700, exist_ok=False)
    output = args.output.resolve()
    receipt = dict(binary=str(binary), binary_sha256=sha(binary), probe_sha256=sha(probe),
                   animated=args.animated, waypipe=args.waypipe,
                   native_frame_source=args.native_frame_source, linux_guest=False,
                   input_to_photon_measured=False, expect_idle=args.expect_idle,
                   trace_storage="temporary host filesystem, copied after graceful exit",
                   input_generator="CUA; actual delivery counts in probe.jsonl")
    parent = host = client = bridge = None
    try:
        with tempfile.TemporaryDirectory(prefix="cw-ab-", dir="/tmp") as temporary:
            short = Path(temporary)
            runtime = short / "display"
            runtime.mkdir(mode=0o700)
            trace = short / "host.jsonl"
            home = output / "home"
            config = home / ".config/cocoa-way"
            config.mkdir(parents=True)
            (config / "connections.toml").touch()
            (config / "container-sessions.toml").touch()
            parent = subprocess.Popen(["/bin/sleep", str(args.seconds + 30)])
            env = dict(PATH="/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
                       HOME=str(home), TMPDIR=str(short) + "/", XKB_CONFIG_ROOT="/opt/homebrew/share/X11/xkb",
                       COCOA_WAY_DISPLAY_WORKER="latency-audit", COCOA_WAY_DISPLAY_RUNTIME_DIR=str(runtime),
                       COCOA_WAY_DISPLAY_PARENT_PID=str(parent.pid), COCOA_WAY_DISPLAY_READY_FILE=str(short / "ready"),
                       COCOA_WAY_PRESENTATION="desktop", COCOA_WAY_NATIVE_FRAME_SOURCE=str(int(args.native_frame_source)),
                       COCOA_WAY_LATENCY_TRACE=str(trace), RUST_LOG="cocoa_way=warn,smithay=warn")
            with (output / "app.log").open("x") as log, (output / "probe.jsonl").open("x") as probe_log:
                host = subprocess.Popen([str(binary)], env=env, stdout=log, stderr=subprocess.STDOUT)
                deadline = time.monotonic() + 12
                socket = None
                while time.monotonic() < deadline and host.poll() is None:
                    socket = next((p for p in runtime.glob("wayland-*") if p.is_socket()), None)
                    if socket:
                        break
                    time.sleep(.05)
                if socket is None:
                    raise RuntimeError("owned host did not create Wayland socket")
                receipt.update(host_pid=host.pid)
                (output / "fixture.json").write_text(json.dumps(dict(
                    host_pid=host.pid, host_executable=str(binary), host_sha256=sha(binary))))
                command = [str(probe), "--width", "800", "--height", "600", "--buffers", "2",
                           "--duration", str(args.seconds), "--log-stderr", "--tag", "audit-20261023",
                           "--continuous-animation" if args.animated else "--static"]
                if args.waypipe:
                    waypipe = args.waypipe_path.resolve(strict=True)
                    receipt["waypipe_path"] = str(waypipe)
                    receipt["waypipe_sha256"] = sha(waypipe)
                    env.update(XDG_RUNTIME_DIR=str(runtime), WAYLAND_DISPLAY=str(socket))
                    transport = short / "waypipe.sock"
                    prefix = [str(waypipe), "--compress", "none", "--oneshot", "--socket", str(transport)]
                    bridge = subprocess.Popen(prefix + ["client"], env=env, stdout=log, stderr=subprocess.STDOUT)
                    deadline = time.monotonic() + 5
                    while not transport.is_socket():
                        if bridge.poll() is not None or time.monotonic() >= deadline:
                            raise RuntimeError("owned Waypipe bridge unavailable")
                        time.sleep(.05)
                    command = prefix + ["server"] + command
                else:
                    command += ["--socket", str(socket)]
                client = subprocess.Popen(command, env=env, stdout=probe_log, stderr=subprocess.STDOUT)
                time.sleep(2)
                if client.poll() is not None:
                    raise RuntimeError("probe exited early")
                print("OWNED_FIXTURE_READY " + json.dumps(receipt), flush=True)
                # Natural duration completion supplies event loss/order counters.
                client.wait(timeout=args.seconds + 3)
                if client.returncode:
                    raise RuntimeError("probe failed")
                stop(bridge)
                stop(parent)
                host.wait(timeout=5)
                receipt["host_graceful_exit"] = host.returncode == 0
                shutil.copyfile(trace, output / "host.jsonl")
                records = [json.loads(row) for row in (output / "probe.jsonl").read_text().splitlines() if row.startswith("{")]
                summary = next((row for row in reversed(records) if row.get("record") == "stop"), {})
                receipt["delivered_input"] = summary
                received = summary.get("key_events", 0)
                receipt["status"] = "ok" if (received == 0 if args.expect_idle else received > 0) else "unexpected_input_count"
                if receipt["status"] != "ok":
                    raise RuntimeError(receipt["status"])
    except BaseException as error:
        receipt.update(status="error", error=str(error))
        raise
    finally:
        for child in (client, bridge, parent, host):
            stop(child)
        receipt.update(host_exit=host.returncode if host else None, probe_exit=client.returncode if client else None)
        (output / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
        print(json.dumps(receipt), flush=True)


if __name__ == "__main__":
    main()
