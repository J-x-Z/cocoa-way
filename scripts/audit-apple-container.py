#!/usr/bin/env python3
"""Read-only CLI audit; --isolated-service opts into a disposable service test.

Never installs packages, requests privileges, starts a VM, edits network settings,
or touches the user's image store. The opt-in test requires no registered Apple
Container services and verifies the exact owned launchd path before cleanup.
"""

import argparse
import json
import os
from pathlib import Path
import re
import signal
import shutil
import subprocess
import sys
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--container", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--isolated-service", action="store_true")
    parser.add_argument("--private-launchd-plist", action="store_true",
                        help="bootstrap only a disposable API service using a private temporary home plist; data and binaries remain external")
    args = parser.parse_args()
    executable = args.container.resolve(strict=True)
    output = args.output.resolve()
    if not str(output).startswith("/Volumes/"):
        parser.error("receipts and disposable data must stay on an external volume")
    output.mkdir(parents=True, exist_ok=False)
    report = {"commands": [], "vm_started": False, "system_install_changed": False,
              "isolated_service": args.isolated_service, "success": False}
    start_attempted = False
    bootstrap_metadata = None
    app_root = output / "data"
    environment = os.environ.copy()
    # Do not inherit developer overrides into the disposable runtime.
    for key in list(environment):
        if key.startswith("CONTAINER_"):
            del environment[key]

    def run(argv, timeout=15, accepted=(0,)):
        index = len(report["commands"])
        stdout = output / f"{index:02d}.stdout"
        stderr = output / f"{index:02d}.stderr"
        record = {"argv": list(map(str, argv)), "stdout": stdout.name,
                  "stderr": stderr.name}
        report["commands"].append(record)
        with stdout.open("xb") as out, stderr.open("xb") as err:
            child = subprocess.Popen(argv, stdout=out, stderr=err,
                                     stdin=subprocess.DEVNULL, env=environment,
                                     start_new_session=True)
            try:
                record["exit"] = child.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait()
                record["timed_out"] = True
                raise RuntimeError(f"command {index} exceeded {timeout}s") from None
        if any(path.stat().st_size > 8 * 1024 * 1024 for path in (stdout, stderr)):
            raise RuntimeError(f"command {index} output exceeded inspection budget")
        text = stdout.read_text(errors="replace")
        if record["exit"] not in accepted:
            raise RuntimeError(f"command {index} failed: exit {record['exit']}; see raw logs")
        return text

    def services():
        text = run(["/bin/launchctl", "list"])
        return [line for line in text.splitlines()
                if line.split() and line.split()[-1].startswith("com.apple.container.")]

    def owns_service():
        domain = run(["/bin/launchctl", "managername"]).strip()
        if domain not in ("Aqua", "Background"):
            raise RuntimeError("only the current unprivileged user domain is permitted")
        domain = ("gui" if domain == "Aqua" else "user") + f"/{os.getuid()}"
        text = run(["/bin/launchctl", "print", f"{domain}/com.apple.container.apiserver"],
                   accepted=(0, 113))
        if not text:
            return False
        server = executable.parent / "container-apiserver"
        return (f"program = {server}" in text and str(app_root) in text)

    def owned_bootout():
        if not owns_service():
            return
        domain = run(["/bin/launchctl", "managername"]).strip()
        domain = ("gui" if domain == "Aqua" else "user") + f"/{os.getuid()}"
        run(["/bin/launchctl", "bootout", f"{domain}/com.apple.container.apiserver"], timeout=15)

    try:
        version = run([executable, "--version"])
        match = re.search(r"\bversion (\d+\.\d+\.\d+)\b", version)
        if not match:
            raise RuntimeError("unrecognized CLI version")
        report["cli_version"] = match[1]
        for command, flags in [(["run", "--help"], ["--publish-socket", "--label"]),
                               (["stats", "--help"], ["--no-stream", "--format"]),
                               (["image", "list", "--help"], ["--format"])]:
            help_text = run([executable, *command])
            if not all(flag in help_text.split() for flag in flags):
                raise RuntimeError(f"missing CLI capability in {command}")
        status = json.loads(run([executable, "system", "status", "--format", "json"],
                                accepted=(0, 1)))
        report["initial_status"] = status
        if args.isolated_service:
            if status.get("status") != "unregistered" or services():
                raise RuntimeError("refusing to replace an existing registered runtime")
            start_attempted = True
            if args.private_launchd_plist:
                # launchd rejects plist metadata on no-owners/removable mounts.
                # Keep just this tiny, owned bootstrap record in a private home
                # directory; never relax the volume's permissions or ownership.
                import plistlib
                bootstrap_metadata = Path(tempfile.mkdtemp(
                    prefix="cocoa-way-container-audit-", dir=Path.home() / "Library/Caches"))
                # Plugin launchd plists also live in appRoot. No VM, init pull or
                # image build is performed in this mode; only tiny runtime
                # metadata lives here, and it is removed after verified shutdown.
                app_root = bootstrap_metadata / "data"
                plist = bootstrap_metadata / "apiserver.plist"
                with plist.open("xb") as handle:
                    plistlib.dump({
                        "Label": "com.apple.container.apiserver",
                        "ProgramArguments": [str(executable.parent / "container-apiserver"), "start"],
                        "EnvironmentVariables": {
                            "CONTAINER_APP_ROOT": str(app_root),
                            "CONTAINER_INSTALL_ROOT": str(executable.parent.parent),
                            "CONTAINER_LOG_ROOT": str(output / "logs")},
                        "RunAtLoad": True,
                        "MachServices": {"com.apple.container.apiserver": True}}, handle)
                domain = run(["/bin/launchctl", "managername"]).strip()
                if domain not in ("Aqua", "Background"):
                    raise RuntimeError("unsupported launchd session")
                domain = ("gui" if domain == "Aqua" else "user") + f"/{os.getuid()}"
                run(["/bin/launchctl", "bootstrap", domain, plist])
                report["launch_method"] = "private_temporary_plist_no_kernel_or_init_bootstrap"
            else:
                run([executable, "system", "start", "--app-root", output / "data",
                     "--install-root", executable.parent.parent,
                     "--log-root", output / "logs", "--disable-kernel-install", "--timeout", "30"],
                    timeout=120)
                report["launch_method"] = "official_system_start"
            if not owns_service():
                raise RuntimeError("runtime launch ownership not proven")
            status = json.loads(run([executable, "system", "status", "--format", "json"]))
            report["running_status"] = status
            if (status.get("status") != "running"
                    or status.get("server", {}).get("version") != match[1]
                    or status.get("paths", {}).get("appRoot") != str(app_root)):
                raise RuntimeError("runtime version, namespace or readiness mismatch")
            for command, flags in [(["build", "--help"], ["--progress", "--file", "--tag"]),
                                   (["builder", "--help"], ["start", "stop"])]:
                help_text = run([executable, *command])
                if not all(flag in help_text.split() for flag in flags):
                    raise RuntimeError(f"missing plugin capability in {command}")
            for command in (["list", "--all", "--format", "json"],
                            ["image", "list", "--format", "json"]):
                rows = json.loads(run([executable, *command]))
                if not isinstance(rows, list) or (command[0] == "list" and rows):
                    raise RuntimeError("unexpected disposable inventory")
        report["success"] = True
    except (RuntimeError, OSError, ValueError) as error:
        report["error"] = str(error)
    finally:
        if start_attempted:
            try:
                if owns_service():
                    try:
                        run([executable, "system", "stop"], timeout=35)
                    except RuntimeError as error:
                        report["graceful_stop_error"] = str(error)
                        # A failed XPC server can make the CLI stop hang. This
                        # targets only the proven owned registration, not a PID
                        # name match or a broad launchd prefix.
                        owned_bootout()
                elif services():
                    raise RuntimeError("foreign service detected; no broad cleanup attempted")
                report["cleanup_complete"] = not services()
                if not report["cleanup_complete"]:
                    raise RuntimeError("disposable runtime services remain")
            except (RuntimeError, OSError, ValueError) as error:
                report["cleanup_error"] = str(error)
                report["success"] = False
        if bootstrap_metadata is not None:
            if report.get("cleanup_complete"):
                shutil.rmtree(bootstrap_metadata)
            else:
                report["retained_bootstrap_metadata"] = str(bootstrap_metadata)
        (output / "result.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({key: value for key, value in report.items() if key != "commands"}, indent=2))
    return 0 if report["success"] else 1


if __name__ == "__main__":
    sys.exit(main())
