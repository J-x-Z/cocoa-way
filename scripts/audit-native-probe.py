#!/usr/bin/env python3
"""Opt-in native-probe fixture and PID-targeted key stimulus; no host lifecycle.

Requires parent-provided dedicated probe-only host/socket. Never launches a host,
container or runtime, activates applications, requests permissions, or posts global
input. Validates executable hash + loaded Mach-O UUID + process start identity.
Only the owned probe is stopped at a deadline. Probe CLOCK_MONOTONIC records remain
separate from host_ca; use audit-stage-latency.py on the REAL host trace afterwards.

Usage: python3 scripts/audit-native-probe.py --run --fixture FIXTURE.json --output NEW_DIR
       python3 scripts/audit-native-probe.py --self-test
Fixture JSON fields: host_pid, host_executable (absolute immutable path),
host_sha256, wayland_socket (owned Unix socket), probe_binary, probe_sha256,
exclusive_probe_host=true and keyboard_focus_ready=true. Optional host_trace is
parent-owned metadata. Defaults: 120 key pairs at 0.2-second intervals. If parent
has already started a Linux probe, external_linux_probe_ready=true skips native
client launch and posts only PID-targeted stimulus; parent owns guest logs/cleanup.
"""
import argparse
import ctypes
import hashlib
import json
import math
import os
from pathlib import Path
import stat
import struct
import subprocess
import sys
import time
import unittest

sys.dont_write_bytecode = True


def executable(path, expected):
    if path.stat().st_size > 128 * 2**20:
        raise ValueError("fixture executable exceeds 128 MiB")
    with path.open("rb") as stream:
        data = stream.read(128 * 2**20 + 1)
    if len(data) > 128 * 2**20 or hashlib.sha256(data).hexdigest() != expected:
        raise ValueError("fixture executable SHA256 mismatch/size limit")
    return data


def macho_uuid(data):
    if len(data) < 32 or struct.unpack_from("<I", data)[0] != 0xfeedfacf:
        raise ValueError("expected a native thin 64-bit Mach-O executable")
    commands, size = struct.unpack_from("<II", data, 16)
    end, offset = 32 + size, 32
    if end > len(data) or commands > 4096:
        raise ValueError("invalid Mach-O load commands")
    for _ in range(commands):
        if offset + 8 > end:
            raise ValueError("truncated Mach-O command")
        kind, length = struct.unpack_from("<II", data, offset)
        if length < 8 or offset + length > end:
            raise ValueError("invalid Mach-O command length")
        if kind == 0x1b and length == 24:
            return data[offset + 8:offset + 24]
        offset += length
    raise ValueError("Mach-O UUID unavailable; refusing PID-only injection")


class Usage(ctypes.Structure):
    _fields_ = [("uuid", ctypes.c_ubyte * 16)] + [(name, ctypes.c_uint64) for name in
        ("user", "system", "package_wakes", "interrupt_wakes", "pageins", "wired", "resident", "footprint", "start", "exit")]


def process_identity(libproc, pid):
    data = Usage()
    if libproc.proc_pid_rusage(pid, 0, ctypes.byref(data)):
        raise ProcessLookupError("could not verify host process identity")
    return bytes(data.uuid), data.start


def run(args):
    if sys.platform != "darwin":
        raise RuntimeError("native input fixture requires macOS")
    if args.fixture.stat().st_size > 16384:
        raise ValueError("fixture metadata exceeds bound")
    fixture = json.loads(args.fixture.read_text(encoding="utf-8"))
    if fixture.get("exclusive_probe_host") is not True or fixture.get("keyboard_focus_ready") is not True:
        raise ValueError("parent must attest dedicated probe-only host and keyboard focus readiness")
    host, probe = Path(fixture["host_executable"]).resolve(), Path(fixture["probe_binary"]).resolve()
    expected_uuid = macho_uuid(executable(host, fixture["host_sha256"]))
    executable(probe, fixture["probe_sha256"])
    socket = Path(fixture["wayland_socket"])
    metadata = socket.lstat()
    if not stat.S_ISSOCK(metadata.st_mode) or metadata.st_uid != os.getuid():
        raise ValueError("Wayland fixture must be an owned native Unix socket")
    pid = fixture["host_pid"]
    if type(pid) is not int or pid <= 1:
        raise ValueError("invalid host PID")
    libproc = ctypes.CDLL("/usr/lib/libproc.dylib")
    libproc.proc_pid_rusage.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_void_p]
    libproc.proc_pid_rusage.restype = ctypes.c_int
    libproc.proc_pidpath.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_uint32]
    libproc.proc_pidpath.restype = ctypes.c_int
    path_buffer = ctypes.create_string_buffer(4096)
    if libproc.proc_pidpath(pid, path_buffer, 4096) <= 0 or Path(os.fsdecode(path_buffer.value)).resolve() != host:
        raise ValueError("live PID executable path does not match isolated fixture")
    identity = process_identity(libproc, pid)
    if identity[0] != expected_uuid:
        raise ValueError("loaded executable UUID differs from hash-verified file")
    cg = ctypes.CDLL("/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics")
    cf = ctypes.CDLL("/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation")
    cg.CGPreflightPostEventAccess.argtypes, cg.CGPreflightPostEventAccess.restype = [], ctypes.c_bool
    if not cg.CGPreflightPostEventAccess():
        raise PermissionError("posting access unavailable; no permission prompt requested")
    cg.CGEventSourceCreate.argtypes, cg.CGEventSourceCreate.restype = [ctypes.c_int], ctypes.c_void_p
    cg.CGEventCreateKeyboardEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint16, ctypes.c_bool]
    cg.CGEventCreateKeyboardEvent.restype = ctypes.c_void_p
    cg.CGEventSetFlags.argtypes = [ctypes.c_void_p, ctypes.c_uint64]
    cg.CGEventSetFlags.restype = None
    cg.CGEventPostToPid.argtypes = [ctypes.c_int, ctypes.c_void_p]
    cg.CGEventPostToPid.restype = None
    cf.CFRelease.argtypes = [ctypes.c_void_p]
    cf.CFRelease.restype = None
    args.output.mkdir(mode=0o700, parents=True, exist_ok=False)
    report = {"schema": 1, "kind": "native_probe_stimulus", "status": "in_progress", "fixture": fixture,
        "host_loaded_uuid": identity[0].hex(), "host_process_start_abstime": identity[1],
        "source": "PID-targeted_CGEventPostToPid_private_event_source", "keys_posted": 0,
        "probe_clock": "native_macos_CLOCK_MONOTONIC_NOT_Linux_guest_and_NOT_host_ca",
        "latency_computed_from_stimulus": False, "host_or_container_lifecycle_modified": False}
    source = cg.CGEventSourceCreate(-1)
    if not source:
        raise RuntimeError("private event source unavailable")
    def save():
        (args.output / "stimulus.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    save()
    log = args.output / "native-probe.jsonl"
    duration = min(90, math.ceil(args.keys * args.interval + 10))
    def key(down):
        if process_identity(libproc, pid) != identity:
            raise ProcessLookupError("host exited/reused; refusing further input")
        event = cg.CGEventCreateKeyboardEvent(source, 0, down)
        if not event:
            raise RuntimeError("could not allocate targeted key event")
        try:
            cg.CGEventSetFlags(event, 0)
            cg.CGEventPostToPid(pid, event)
        finally:
            cf.CFRelease(event)
    def stimulate(log_path=None):
        report["stimulus_started_host_monotonic_ns"] = time.monotonic_ns()
        for _ in range(args.keys):
            key(True)
            try:
                time.sleep(.03)
            finally:
                key(False)
            report["keys_posted"] += 1
            if log_path is not None and log_path.stat().st_size > 16 * 2**20:
                raise ValueError("probe log exceeds 16 MiB")
            time.sleep(args.interval - .03)
    try:
        if fixture.get("external_linux_probe_ready") is True:
            # Parent owns guest readiness/lifecycle. Do not also launch a native client.
            report["probe_clock"] = "external_Linux_CLOCK_MONOTONIC; no_host_guest_subtraction_performed"
            stimulate()
            report["status"] = "ok"
            print(f"External Linux probe stimulus evidence: {args.output}")
            return
        with log.open("xb") as output, subprocess.Popen([str(probe), "--socket", str(socket), "--static",
                "--duration", str(duration), "--log-stderr", "--tag", "audit-native-probe"],
                stdout=subprocess.DEVNULL, stderr=output) as child:
            try:
                deadline = time.monotonic() + 5
                with log.open("rb") as reader:
                    ready = False
                    while time.monotonic() < deadline and child.poll() is None:
                        row = reader.readline(16385)
                        if len(row) > 16384:
                            raise ValueError("probe line exceeds bound")
                        if row:
                            if not row.endswith(b"\n"):
                                reader.seek(-len(row), 1)
                                time.sleep(.01)
                                continue
                            if json.loads(row).get("record") == "frame_commit":
                                ready = True
                                break
                        else:
                            time.sleep(.05)
                if not ready:
                    raise RuntimeError("probe did not commit its initial frame")
                stimulate(log)
                child.wait(timeout=max(1, duration + 2 - args.keys * args.interval))
                if child.returncode:
                    raise RuntimeError("native probe exited with an error")
                report["status"] = "ok"
            finally:
                if child.poll() is None:
                    child.terminate()
                    try:
                        child.wait(timeout=2)
                    except subprocess.TimeoutExpired:
                        child.kill()
                        child.wait()
    except BaseException as error:
        report["status"] = "error"
        report["error_type"] = type(error).__name__
        raise
    finally:
        cf.CFRelease(source)
        save()
    print(f"Stimulus evidence: {args.output}; analyze the parent-owned real host trace separately")


class Tests(unittest.TestCase):
    def test_uuid_parser(self):
        header = struct.pack("<8I", 0xfeedfacf, 0, 0, 0, 1, 24, 0, 0)
        self.assertEqual(macho_uuid(header + struct.pack("<II", 0x1b, 24) + bytes(range(16))), bytes(range(16)))
        with self.assertRaises(ValueError):
            macho_uuid(header)
    def test_rusage_abi_size(self):
        self.assertEqual(ctypes.sizeof(Usage), 96)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--keys", type=int, default=120)
    parser.add_argument("--interval", type=float, default=.2)
    parser.add_argument("--run", action="store_true")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        return int(not unittest.TextTestRunner().run(unittest.defaultTestLoader.loadTestsFromTestCase(Tests)).wasSuccessful())
    if not args.run or not args.fixture or not args.output or not (1 <= args.keys <= 300 and .1 <= args.interval <= 1 and args.keys * args.interval <= 75):
        parser.error("explicit --run --fixture PATH --output NEW_DIR required; keys 1..300, interval .1..1, duration <=75 s")
    run(args)
    return 0


if __name__ == "__main__":
    sys.exit(main())
