#!/usr/bin/env python3
"""Owned display worker + real Linux probe through the production Apple V2 relay.

Run only when the coordinator owns the runtime. No service starts, image pulls,
network changes, clipboard helpers, or input injection. READY invites parent CUA
input; --expect-idle validates an idle run without claiming input acceptance.
"""
import argparse
import contextlib
import hashlib
import json
import os
from pathlib import Path
import secrets
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parent.parent
PATH = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
OWNER_LABEL = "org.cocoa-way.owner"
V2_MARKERS = ("cocoa-way container relay: using publish-socket transport v2",
              "cocoa-way transport v2: host channel connected")
FALLBACK_MARKERS = ("using stdio compatibility transport", "falling back to stdio")


def sha(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def save(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")


def stop(child):
    if child is None:
        return
    # Each child has its own session; never signal the coordinator's process group.
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(child.pid, sig)
        except ProcessLookupError:
            break
        try:
            child.wait(timeout=2)
        except subprocess.TimeoutExpired:
            continue
        # The group leader may have exited before its relay descendants.
        if sig == signal.SIGTERM:
            time.sleep(.1)
    child.poll()


def records(path):
    result = []
    if path.exists():
        for line in path.read_text(errors="replace").splitlines():
            if line.startswith("{"):
                try:
                    row = json.loads(line)
                except ValueError:
                    continue
                if isinstance(row, dict):
                    result.append(row)
    return result


def transport_evidence(path):
    text = path.read_text(errors="replace") if path.exists() else ""
    return dict(v2=all(marker in text for marker in V2_MARKERS),
                fallback=any(marker in text for marker in FALLBACK_MARKERS))


def wait_until(check, seconds, children, log=None):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if log and transport_evidence(log)["fallback"]:
            raise RuntimeError("production relay fell back to stdio; not a V2 run")
        if any(child.poll() is not None for child in children):
            raise RuntimeError("fixture process exited before readiness; inspect logs")
        value = check()
        if value:
            return value
        time.sleep(.05)
    raise TimeoutError("fixture readiness deadline exceeded")


def owned_record(text, name, token):
    def unique(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError("duplicate inspect key")
            result[key] = value
        return result
    if len(text) > 1024 * 1024:
        raise ValueError("inspect exceeds 1 MiB")
    rows = json.loads(text, object_pairs_hook=unique)
    if rows == []:
        return None
    if not isinstance(rows, list) or len(rows) != 1:
        raise ValueError("ambiguous container inspect")
    row = rows[0]
    config = row.get("configuration", {})
    if (config.get("id") != name or row.get("id", name) != name
            or config.get("labels", {}).get(OWNER_LABEL) != token):
        raise ValueError("container ownership mismatch; refusing operation")
    return dict(name=name, label={OWNER_LABEL: token}, status=row.get("status"),
                resources=config.get("resources", {}))


def confirmed_missing(text, name):
    lines = [line.strip() for line in text.splitlines() if line.strip()]
    for line in lines:
        for prefix in ("Error: ", "notFound: "):
            if line.startswith(prefix):
                line = line[len(prefix):]
        if line.strip('"') != f"container not found: {name}":
            return False
    return bool(lines)


def guest_script(seconds, animated, name):
    # Only explicit nonsecret identity/version fields leave the guest.
    attest = """import hashlib,json,os,pathlib,shutil,subprocess
wp=pathlib.Path(shutil.which('waypipe')).resolve()
paths={'waypipe':wp,'probe':pathlib.Path('/audit/input-latency-probe'),'clock':pathlib.Path('/audit/measure-clock-offset.py')}
row={'kind':'guest_attestation','boot_id':pathlib.Path('/proc/sys/kernel/random/boot_id').read_text().strip(),'time_namespace':os.readlink('/proc/self/ns/time'),'machine':os.uname().machine,'cpu_count':os.cpu_count(),'mem_total_kib':next(int(line.split()[1]) for line in pathlib.Path('/proc/meminfo').read_text().splitlines() if line.startswith('MemTotal:')),'waypipe_path':str(wp),'waypipe_version':subprocess.check_output([str(wp),'--version'],text=True,timeout=3).strip(),'sha256':{k:hashlib.sha256(p.read_bytes()).hexdigest() for k,p in paths.items()}}
print(json.dumps(row),flush=True)
"""
    body = f"""set -eu
python3 -B -c {shlex.quote(attest)}
python3 -B /audit/measure-clock-offset.py --server --unix /tmp/cocoa-way/clock.sock --timeout 5 --server-timeout {seconds + 40} &
clock=$!
trap 'kill "$clock" 2>/dev/null || true' EXIT
n=0
while [ ! -f /tmp/cocoa-way/probe-go ]; do
  n=$((n + 1)); [ "$n" -le 300 ] || exit 124
  sleep .05
done
/audit/input-latency-probe --width 800 --height 600 --buffers 2 --duration {seconds} --log-stderr --tag {shlex.quote(name)} {'--continuous-animation' if animated else '--static'}
wait "$clock"
"""
    # stdout is the relay control pipe: application output must go to stderr.
    return ('exec env -i PATH=/usr/local/bin:/usr/bin:/bin HOME=/tmp '
            'XDG_RUNTIME_DIR=/tmp/cocoa-way WAYLAND_DISPLAY="$WAYLAND_DISPLAY" '
            'sh -c ' + shlex.quote(body) + ' >&2')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--waypipe-path", type=Path, default=Path("/opt/homebrew/bin/waypipe"))
    parser.add_argument("--container-path", type=Path, default=Path("/usr/local/bin/container"))
    parser.add_argument("--app-root", type=Path,
                        default=Path.home() / "Library/Application Support/com.apple.container")
    parser.add_argument("--image", default="localhost/cocoa-way-niri:2.0-rc2")
    parser.add_argument("--init-image", default="ghcr.io/apple/containerization/vminit:0.52.1")
    parser.add_argument("--probe", type=Path,
                        default=ROOT / "target/latency-audit/guest/input-latency-probe")
    parser.add_argument("--clock-script", type=Path, default=ROOT / "scripts/measure-clock-offset.py")
    parser.add_argument("--animated", action="store_true")
    parser.add_argument("--expect-idle", action="store_true")
    parser.add_argument("--native-frame-source", action="store_true")
    parser.add_argument("--seconds", type=int, default=75,
                        help="probe lifetime, 65..90; READY occurs within first 5 seconds")
    args = parser.parse_args()
    if not 65 <= args.seconds <= 90:
        parser.error("--seconds must be 65..90 (at least 60 seconds after READY)")
    binary, waypipe, container, probe, clock_script, app_root = [p.resolve(strict=True) for p in
        (args.binary, args.waypipe_path, args.container_path, args.probe, args.clock_script, args.app_root)]
    if any("\n" in str(p) or "," in str(p) for p in (probe, clock_script, app_root)):
        parser.error("mount/app-root paths must not contain commas or newlines")
    with probe.open("rb") as stream:
        elf = stream.read(20)
    if elf[:5] != b"\x7fELF\x02" or elf[5] != 1 or elf[18:20] != b"\xb7\x00":
        parser.error("--probe must be a little-endian aarch64 Linux ELF")
    args.output.mkdir(parents=True, mode=0o700, exist_ok=False)
    output = args.output.resolve()
    name, token = "cw-audit-" + secrets.token_hex(8), secrets.token_hex(32)
    runtime_env = dict(HOME=str(Path.home()), PATH=PATH, CONTAINER_APP_ROOT=str(app_root))
    receipt = dict(status="starting", linux_guest=True, input_to_photon_measured=False,
                   input_acceptance=False, input_generator="parent CUA only; no script injection",
                   expect_idle=args.expect_idle, animated=args.animated, seconds=args.seconds,
                   native_frame_source=args.native_frame_source,
                   binary=str(binary), binary_sha256=sha(binary), waypipe_path=str(waypipe),
                   waypipe_sha256=sha(waypipe), probe=str(probe), probe_sha256=sha(probe),
                   clock_script=str(clock_script), clock_sha256=sha(clock_script),
                   runtime_binary=str(container), runtime_sha256=sha(container), app_root=str(app_root),
                   image=args.image, init_image=args.init_image, container_name=name,
                   owner_label={OWNER_LABEL: token}, resources=dict(cpus=2, memory="1G"),
                   mount="private probe/clock staging only; readonly", cleanup=[])
    children = []
    parent = host = relay = calibration = None
    launched = False
    temporary = tempfile.TemporaryDirectory(prefix="cw-gl-", dir="/tmp")
    short = Path(temporary.name)
    trace, relay_log = short / "host.jsonl", output / "relay.log"
    handles = contextlib.ExitStack()

    def run(argv, timeout=5):
        return subprocess.run([str(container)] + argv, env=runtime_env, stdin=subprocess.DEVNULL,
                              capture_output=True, text=True, timeout=timeout)

    def inspect():
        result = run(["inspect", name])
        if result.returncode:
            if confirmed_missing(result.stderr, name):
                return None
            raise RuntimeError("owned inspect failed; cleanup authority unavailable")
        return owned_record(result.stdout, name, token)

    def spawn(argv, env, log):
        child = subprocess.Popen(argv, env=env, stdin=subprocess.DEVNULL, stdout=log,
                                 stderr=subprocess.STDOUT, start_new_session=True)
        children.append(child)
        return child

    def interrupted(signum, frame):
        raise InterruptedError(f"interrupted by signal {signum}")

    previous = {sig: signal.signal(sig, interrupted) for sig in (signal.SIGTERM, signal.SIGINT)}
    try:
        # Inspection never pulls. Missing cached workload/init images abort before run.
        receipt["cached_images"] = {}
        for ref in (args.image, args.init_image):
            result = run(["image", "inspect", ref])
            if result.returncode or not json.loads(result.stdout):
                raise RuntimeError(f"cached image unavailable: {ref}; no download attempted")
            receipt["cached_images"][ref] = dict(inspected=True, metadata_sha256=
                hashlib.sha256(result.stdout.encode()).hexdigest())
        receipt["runtime_version"] = run(["--version"]).stdout.strip()
        receipt["waypipe_version"] = subprocess.check_output(
            [str(waypipe), "--version"], env=runtime_env, text=True, timeout=3).strip()
        runtime, staged = short / "display", short / "guest"
        runtime.mkdir(mode=0o700)
        staged.mkdir(mode=0o700)
        shutil.copy2(probe, staged / "input-latency-probe")
        shutil.copy2(clock_script, staged / "measure-clock-offset.py")
        home = output / "home"
        config = home / ".config/cocoa-way"
        config.mkdir(parents=True)
        (config / "connections.toml").touch()
        (config / "container-sessions.toml").touch()
        app_log = handles.enter_context((output / "app.log").open("x"))
        remote_log = handles.enter_context(relay_log.open("x"))
        clock_log = handles.enter_context((output / "clock.log").open("x"))
        resource_log = handles.enter_context((output / "resources.jsonl").open("x"))
        parent = spawn(["/bin/sleep", str(args.seconds + 90)], runtime_env, app_log)
        host_env = dict(PATH=PATH, HOME=str(home), TMPDIR=str(short) + "/",
                        XKB_CONFIG_ROOT="/opt/homebrew/share/X11/xkb",
                        COCOA_WAY_DISPLAY_WORKER="latency-audit",
                        COCOA_WAY_DISPLAY_RUNTIME_DIR=str(runtime),
                        COCOA_WAY_DISPLAY_PARENT_PID=str(parent.pid),
                        COCOA_WAY_DISPLAY_READY_FILE=str(short / "ready"),
                        COCOA_WAY_PRESENTATION="desktop",
                        COCOA_WAY_NATIVE_FRAME_SOURCE=str(int(args.native_frame_source)),
                        COCOA_WAY_LATENCY_TRACE=str(trace), RUST_LOG="cocoa_way=warn,smithay=warn")
        host = spawn([str(binary)], host_env, app_log)
        display = wait_until(lambda: next((p for p in runtime.glob("wayland-*") if p.is_socket()), None),
                             12, [host, parent])
        receipt["host_pid"] = host.pid
        save(output / "fixture.json", dict(host_pid=host.pid, host_executable=str(binary),
             host_sha256=receipt["binary_sha256"], external_linux_probe_ready=False, tag=name))
        extra = ["--cpus", "2", "--memory", "1G", "--init-image", args.init_image,
                 "--publish-socket", f"{short}/clock.sock:/tmp/cocoa-way/clock.sock"]
        relay_env = dict(runtime_env, XDG_RUNTIME_DIR=str(runtime), WAYLAND_DISPLAY=str(display),
                        COCOA_WAY_CONTAINER_RELAY="1", COCOA_WAY_APPLE_TRANSPORT="v2",
                        COCOA_WAY_RELAY_RUNTIME_BINARY=str(container), COCOA_WAY_RELAY_IMAGE=args.image,
                        COCOA_WAY_RELAY_CONTAINER_NAME=name, COCOA_WAY_RELAY_OWNER_TOKEN=token,
                        COCOA_WAY_RELAY_MOUNTS=f"type=bind,source={staged},target=/audit,readonly",
                        COCOA_WAY_RELAY_RUNTIME_ARGS="\n".join(extra))
        command = [str(waypipe), "--compress", "none", "--threads", "0", "--socket", str(short / "wp.sock"),
                   "--remote-socket", "/tmp/cocoa-way/wp.sock", "--ssh-bin", str(binary),
                   "--remote-bin", "waypipe", "ssh", "apple-container", "sh", "-c",
                   guest_script(args.seconds, args.animated, name)]
        receipt["waypipe_command"] = command
        receipt["runtime_args"] = extra
        launched = True
        relay = spawn(command, relay_env, remote_log)
        wait_until(lambda: transport_evidence(relay_log)["v2"] and
                   "clock-offset server listening" in relay_log.read_text(errors="replace"),
                   25, [host, relay], relay_log)
        receipt["owned_container"] = inspect()
        if not receipt["owned_container"]:
            raise RuntimeError("owned container disappeared")
        attestation = next(row for row in records(relay_log) if row.get("kind") == "guest_attestation")
        receipt["guest_attestation"] = attestation
        if (attestation["sha256"]["probe"] != receipt["probe_sha256"]
                or attestation["sha256"]["clock"] != receipt["clock_sha256"]):
            raise RuntimeError("guest mounted artifact hashes differ")
        if attestation.get("cpu_count") != 2:
            raise RuntimeError("guest does not expose the requested 2 CPUs")
        clock_command = [sys.executable, "-B", str(clock_script), "--unix", str(short / "clock.sock"),
                         "--samples", str(args.seconds * 50), "--interval-ms", "20", "--timeout", "2",
                         "--valid-for", "5", "--output", str(output / "clock.json")]
        receipt["clock_command"] = clock_command
        calibration = spawn(clock_command, runtime_env, clock_log)
        time.sleep(.25)
        if calibration.poll() is not None:
            raise RuntimeError("clock client failed before probe launch")
        gate_time = time.monotonic()
        gate = run(["exec", name, "env", "-i", "PATH=/usr/local/bin:/usr/bin:/bin",
                    "touch", "/tmp/cocoa-way/probe-go"], timeout=3)
        if gate.returncode:
            raise RuntimeError("could not release owned probe start gate")
        wait_until(lambda: any(row.get("record") == "commit_queued" for row in records(relay_log)),
                   max(.01, 5 - (time.monotonic() - gate_time)), [host, relay, calibration], relay_log)
        receipt.update(status="ready", ready_monotonic_ns=time.monotonic_ns(),
                       tag=name, transport=transport_evidence(relay_log))
        save(output / "fixture.json", dict(host_pid=host.pid, host_executable=str(binary),
             host_sha256=receipt["binary_sha256"], external_linux_probe_ready=True, tag=name))
        save(output / "receipt.json", receipt)
        print("OWNED_FIXTURE_READY " + json.dumps(receipt), flush=True)
        deadline = gate_time + args.seconds + 25
        next_resource = 0
        while relay.poll() is None or calibration.poll() is None:
            if host.poll() is not None or time.monotonic() > deadline:
                raise RuntimeError("host exited or guest/clock completion deadline exceeded")
            if time.monotonic() >= next_resource:
                try:
                    sample = subprocess.run(["/bin/ps", "-p", f"{host.pid},{relay.pid},{calibration.pid}",
                        "-o", "pid=,pcpu=,rss=,time="], capture_output=True, text=True, timeout=1,
                        env=runtime_env)
                    resource_log.write(json.dumps(dict(monotonic_ns=time.monotonic_ns(),
                        columns="pid cpu_percent rss_kib cpu_time; host/waypipe/clock, not VM totals",
                        rows=sample.stdout.splitlines(), exit=sample.returncode)) + "\n")
                    resource_log.flush()
                except subprocess.TimeoutExpired:
                    resource_log.write('{"error":"ps timeout"}\n')
                next_resource = time.monotonic() + 5
            time.sleep(.2)
        if relay.returncode or calibration.returncode:
            raise RuntimeError("guest relay or calibration failed; inspect logs")
        clock = json.loads((output / "clock.json").read_text())
        identity = clock.get("guest") or {}
        receipt["clock"] = dict(path=str(output / "clock.json"), usable=clock.get("usable"),
                                guest=identity, summary=clock.get("summary"))
        receipt["guest_clock_verified"] = all(attestation.get(key) and
            identity.get(key) == attestation[key] for key in ("boot_id", "time_namespace"))
        if not clock.get("usable") or not receipt["guest_clock_verified"]:
            raise RuntimeError("unusable calibration or mismatched guest clock identity")
        summary = next((row for row in reversed(records(relay_log)) if row.get("record") == "stop"), {})
        receipt["delivered_input"] = summary
        count = summary.get("key_events", 0)
        idle_input = count + summary.get("button_events", 0)
        if not summary or (idle_input != 0 if args.expect_idle else count == 0):
            raise RuntimeError("missing probe stop or unexpected input count")
        receipt["input_observed"] = idle_input > 0
        receipt["input_acceptance_note"] = "parent must compare CUA delivery/order and stage joins separately"
        receipt["frames_committed"] = summary.get("frames_committed", 0)
        if args.animated and receipt["frames_committed"] < 2:
            raise RuntimeError("animated guest did not commit multiple frames")
        stop(parent)
        host.wait(timeout=5)
        receipt["host_graceful_exit"] = host.returncode == 0
        if not receipt["host_graceful_exit"]:
            raise RuntimeError("host did not exit gracefully")
        receipt["status"] = "ok_idle" if args.expect_idle else "ok"
    except BaseException as error:
        receipt.update(status="error", error=str(error))
    finally:
        for sig in previous:
            signal.signal(sig, signal.SIG_IGN)
        for child in reversed(children):
            try:
                stop(child)
            except OSError as error:
                receipt["cleanup"].append(str(error))
        if launched:
            try:
                if inspect() is not None:
                    run(["stop", "--time", "2", name], timeout=5)
                    if inspect() is not None:
                        result = run(["delete", "--force", name], timeout=5)
                        if result.returncode:
                            raise RuntimeError("owned container delete failed")
                receipt["container_cleanup_confirmed"] = inspect() is None
            except (OSError, ValueError, RuntimeError, subprocess.TimeoutExpired) as error:
                receipt["cleanup"].append(str(error))
                receipt["container_cleanup_confirmed"] = False
            if not receipt.get("container_cleanup_confirmed"):
                receipt["status"] = "error"
        handles.close()
        if trace.exists():
            shutil.copyfile(trace, output / "host.jsonl")
        with (output / "guest.jsonl").open("w") as guest:
            for row in records(relay_log):
                if "probe" in row:
                    guest.write(json.dumps(row) + "\n")
        receipt.update(transport=transport_evidence(relay_log),
                       host_exit=host.returncode if host else None,
                       relay_exit=relay.returncode if relay else None,
                       clock_exit=calibration.returncode if calibration else None)
        save(output / "receipt.json", receipt)
        temporary.cleanup()
        for sig, handler in previous.items():
            signal.signal(sig, handler)
        print(json.dumps(receipt), flush=True)
    return 0 if receipt["status"] in ("ok", "ok_idle") else 1


if __name__ == "__main__":
    sys.exit(main())
