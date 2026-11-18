#!/usr/bin/env python3
"""Opt-in, isolated macOS/Apple Container regression, reusing an existing image.

Creates no images and never edits the user's application profiles. Reports control
and lifecycle observations, not visual correctness or input-to-photon latency.
Cleanup deletes only containers bearing this run's random audit label.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import uuid


def execute(args, **kwargs):
    result = subprocess.run(args, capture_output=True, text=True, timeout=kwargs.pop("timeout", 40), **kwargs)
    if result.returncode:
        raise RuntimeError(f"{args[0]} failed ({result.returncode}): {result.stderr[-2048:]}")
    return result.stdout


def verify_app_root(container, expected, run=execute):
    status = json.loads(run([container, "system", "status", "--format", "json"]))
    paths = status.get("paths") if isinstance(status, dict) else None
    actual = paths.get("appRoot") if isinstance(paths, dict) else None
    if (
        not isinstance(status, dict)
        or status.get("status") != "running"
        or not isinstance(actual, str)
        or not Path(actual).is_absolute()
    ):
        raise RuntimeError("running Apple Container service did not report its data root")
    actual = Path(actual).resolve(strict=True)
    if actual != expected:
        raise RuntimeError(f"Apple Container data root is {actual}, expected {expected}")
    return str(actual)


def cleanup_owned_containers(container, label, token, run=execute):
    def inventory():
        rows = json.loads(run([container, "list", "--all", "--format", "json"]))
        if not isinstance(rows, list) or any(
            not isinstance(row, dict)
            or not isinstance(row.get("configuration"), dict)
            or not isinstance(row["configuration"].get("id"), str)
            for row in rows
        ):
            raise RuntimeError("container inventory schema cannot establish cleanup authority")
        return rows

    rows = inventory()
    resolved_races = []
    for row in rows:
        configuration = row.get("configuration", {})
        if configuration.get("labels", {}).get(label) != token:
            continue
        name = configuration.get("id")
        if not isinstance(name, str) or not name:
            raise RuntimeError("test container inspect schema missing identity")
        try:
            run([container, "delete", "--force", name], timeout=30)
        except (OSError, RuntimeError, subprocess.SubprocessError):
            # The app's asynchronous cleanup can win after our inventory read.
            # Only a successful fresh inventory proving absence resolves this.
            current = inventory()
            if any(item.get("configuration", {}).get("id") == name for item in current):
                raise
            resolved_races.append(name)
    return resolved_races


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--run", action="store_true", help="actually launch isolated test windows/containers")
    parser.add_argument("--binary", type=Path, default=Path("target/debug/cocoa-way"))
    parser.add_argument("--ctl", type=Path, default=Path("target/debug/cocoa-wayctl"))
    parser.add_argument("--image", default="localhost/cocoa-way-niri:2.0-rc2")
    parser.add_argument("--seconds", type=int, default=30)
    parser.add_argument("--shutdown-during-launch", action="store_true")
    parser.add_argument("--require-content", action="store_true",
                        help="also require each instance's worker to submit a frame containing client buffers")
    parser.add_argument("--require-app-root", type=Path,
                        help="refuse to launch or delete containers unless the active service uses this data root")
    parser.add_argument("--output", type=Path, default=Path("target/audit-20261015/integration.json"))
    options = parser.parse_args()
    if not options.run:
        parser.error("use --run to explicitly start tests")
    if sys.platform != "darwin" or not 1 <= options.seconds <= 300:
        parser.error("requires macOS and --seconds 1..300")
    expected_root = None
    if options.require_app_root is not None:
        try:
            expected_root = options.require_app_root.resolve(strict=True)
            if not expected_root.is_dir():
                parser.error("--require-app-root must be a directory")
        except (OSError, RuntimeError) as error:
            parser.error(f"invalid --require-app-root: {error}")
    binary, ctl = options.binary.resolve(), options.ctl.resolve()
    container = "/usr/local/bin/container"
    token = uuid.uuid4().hex
    label = "org.cocoa-way.audit"
    report = {"schema": 1, "run": token, "image": options.image, "observations": [],
              "scope": "control/lifecycle smoke and bounded soak; visual and input latency not inferred",
              "require_content": options.require_content,
              "success": False, "cleanup_errors": []}
    report["binary_sha256"] = hashlib.sha256(binary.read_bytes()).hexdigest()
    report["ctl_sha256"] = hashlib.sha256(ctl.read_bytes()).hexdigest()
    options.output.parent.mkdir(parents=True, exist_ok=True)
    # macOS sockaddr_un has a small path limit; do not put sockets under target/.
    with tempfile.TemporaryDirectory(prefix="cw-audit-", dir="/tmp") as temporary:
        root = Path(temporary)
        root.chmod(0o700)
        home = root / "home"
        config = home / ".config/cocoa-way"
        config.mkdir(parents=True)
        runtime = root / "display"
        socket = root / "control/control.sock"
        prefix = f"audit-{token[:8]}"
        profiles = [
            (f"{prefix}-niri", "desktop", "niri"),
            (f"{prefix}-foot", "rootless", "foot /bin/sh -c 'printf cocoa-way-audit; sleep 300'"),
            (f"{prefix}-firefox", "rootless", "firefox --no-remote --new-instance --profile /tmp/cw-audit-firefox about:blank"),
        ]
        lines = []
        for index, (name, presentation, command) in enumerate(profiles):
            lines += ["[[session]]", f'id = "{index + 1:032x}"',
                      f"name = {json.dumps(name)}", f"image = {json.dumps(options.image)}",
                      'runtime = "container"', 'display = "auto"',
                      f"presentation = {json.dumps(presentation)}", f"command = {json.dumps(command)}",
                      "audio = false", f'runtime_args = ["--memory", "2G", "--cpus", "2", "--label", "{label}={token}"]', ""]
        (config / "container-sessions.toml").write_text("\n".join(lines))
        (config / "connections.toml").write_text("")
        env = {**os.environ, "HOME": str(home), "COCOA_WAY_CONTROL_SOCKET": str(socket),
               "COCOA_WAY_DISPLAY_RUNTIME_DIR": str(runtime), "RUST_LOG": "cocoa_way=info"}
        process = None

        def request(command, name=None):
            args = [str(ctl), "--json", "--socket", str(socket), command]
            if name is not None:
                args.append(name)
            return json.loads(execute(args, env=env))

        def observe(kind, data):
            report["observations"].append({"kind": kind, "host_monotonic_ns": time.monotonic_ns(), "data": data})

        def check_app_root():
            if expected_root is not None:
                return verify_app_root(container, expected_root)

        def launch(name):
            check_app_root()
            return request("launch", name)

        def active(name):
            profile_id = next(f"{index + 1:032x}" for index, profile in enumerate(profiles) if profile[0] == name)
            rows = request("running")["data"]
            return any(row.get("profile_id") == profile_id for row in rows)

        def content_submitted(name):
            profile_id = next(f"{index + 1:032x}" for index, profile in enumerate(profiles) if profile[0] == name)
            instance = next((row for row in request("running")["data"]
                             if row.get("profile_id") == profile_id), None)
            if instance is None:
                return None
            progress = request("performance")["data"].get("render_progress", [])
            expected_pid = instance.get("display_pid") or process.pid
            return next((row for row in progress
                         if row.get("slot") == instance["display_slot"]
                         and row.get("worker_pid") == expected_pid
                         and isinstance(row.get("progress"), dict)
                         and row["progress"].get("submitted_content_frames", 0) > 0), None)

        def wait_for(predicate, message, timeout=90):
            deadline = time.monotonic() + timeout
            last = None
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    raise RuntimeError(f"Cocoa-Way exited unexpectedly: {process.returncode}")
                try:
                    last = predicate()
                    if last:
                        return last
                except (OSError, RuntimeError, json.JSONDecodeError):
                    pass
                time.sleep(0.25)
            raise RuntimeError(f"{message}; last={str(last)[:1024]}")

        log_path = options.output.with_suffix(".app.log")
        try:
            if expected_root is None:
                execute([container, "system", "status"])
            else:
                observe("verified_app_root", check_app_root())
            with log_path.open("w") as app_log:
                process = subprocess.Popen([str(binary)], env=env, stdout=app_log, stderr=subprocess.STDOUT)
                wait_for(lambda: socket.exists() and request("status"), "control service startup", 20)
                observe("startup", request("applications"))
                for name, _, _ in profiles:
                    observe("launch_accepted", launch(name))
                    wait_for(lambda: active(name), f"instance {name} did not become active")
                    if options.require_content:
                        evidence = wait_for(lambda: content_submitted(name),
                            f"instance {name} became active but submitted no client content", 30)
                        observe("client_content_submitted", {"name": name, "snapshot": evidence,
                            "meaning": "CPU submission with client buffers, not visual or screen-presentation acceptance"})
                observe("three_active", request("running"))
                observe("displays", request("displays"))
                deadline = time.monotonic() + options.seconds
                while time.monotonic() < deadline:
                    observe("soak", request("running"))
                    observe("soak_displays", request("displays"))
                    if not all(active(profile[0]) for profile in profiles):
                        raise RuntimeError("an application exited during the bounded soak")
                    time.sleep(min(2, max(0, deadline - time.monotonic())))
                for directory in Path("/tmp").glob(f"cwd-{process.pid}-*"):
                    log = directory / "display.log"
                    if log.is_file():
                        with log.open("rb") as stream:
                            options.output.with_name(f"{options.output.stem}-{directory.name}.log").write_bytes(stream.read(1024 * 1024))
                    performance = directory / "display-performance.json"
                    if performance.is_file():
                        observe("worker_performance", json.loads(performance.read_text()))
                for name, _, _ in reversed(profiles):
                    observe("application_logs", request("logs", name))
                    observe("stop_accepted", request("stop", name))
                    wait_for(lambda: not active(name), f"instance {name} did not stop", 30)
                # Relaunch exercises owned-name lease release and stale generations.
                name = profiles[1][0]
                observe("relaunch_accepted", launch(name))
                wait_for(lambda: active(name), "terminal reconnect", 90)
                if options.require_content:
                    observe("relaunch_content_submitted", wait_for(lambda: content_submitted(name),
                        "restarted terminal submitted no client content", 30))
                observe("reconnected", request("running"))
                observe("final_stop", request("stop", name))
                wait_for(lambda: not active(name), "terminal final stop", 30)
                if options.shutdown_during_launch:
                    time.sleep(1)
                    observe("shutdown_launch_accepted", launch(name))
                    time.sleep(0.45)
                    observe("shutdown_tasks", request("tasks"))
                    spec = importlib.util.spec_from_file_location("audit_live_host", Path(__file__).with_name("audit-live-host.py"))
                    helper = importlib.util.module_from_spec(spec)
                    spec.loader.exec_module(helper)
                    accepted = helper.app_action(process.pid, "terminate")
                    process.wait(timeout=30)
                    observe("shutdown_during_launch", {"AppKit_terminate_accepted": accepted, "exit_code": process.returncode})
                    if not accepted or process.returncode != 0:
                        raise RuntimeError("graceful shutdown during launch failed")
                report["success"] = True
        except (OSError, RuntimeError, subprocess.SubprocessError, ValueError) as error:
            report["error"] = str(error)
        finally:
            if process is not None and process.poll() is None:
                process.send_signal(signal.SIGTERM)
                try:
                    process.wait(timeout=20)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
            # Never match names alone; a user could reuse a test name externally.
            try:
                if process is not None:
                    check_app_root()
                if report["success"] and options.shutdown_during_launch:
                    remaining = json.loads(execute([container, "list", "--all", "--format", "json"]))
                    if any(row.get("configuration", {}).get("labels", {}).get(label) == token
                           for row in remaining):
                        report["cleanup_errors"].append("graceful shutdown left an owned audit container; fallback cleanup was required")
                if process is not None:
                    report["cleanup_races_resolved"] = cleanup_owned_containers(container, label, token)
            except (OSError, RuntimeError, subprocess.SubprocessError, ValueError) as error:
                report["cleanup_errors"].append(str(error))
            options.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"success": report["success"], "output": str(options.output),
                      "error": report.get("error"), "cleanup_errors": report["cleanup_errors"]}))
    return int(not report["success"] or bool(report["cleanup_errors"]))


if __name__ == "__main__":
    raise SystemExit(main())
