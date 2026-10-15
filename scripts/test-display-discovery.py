#!/usr/bin/env python3
"""Private filesystem/socket and CLI stubs only; never connects to a compositor."""

import os
from pathlib import Path
import socket
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "run_waypipe.sh"


class DisplayDiscoveryTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="cwd-audit-", dir="/tmp")
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        self.slot = self.directory.name
        for name, text in [
            ("display.slot", self.slot),
            ("display.parent", str(os.getpid())),
            ("display.worker", str(os.getpid())),
        ]:
            (self.directory / name).write_text(text)
            (self.directory / name).chmod(0o600)
        self.socket_path = self.directory / "wayland-1"
        self.listener = socket.socket(socket.AF_UNIX)
        self.addCleanup(self.listener.close)
        self.listener.bind(str(self.socket_path))
        self.socket_path.chmod(0o600)
        self.bin = self.directory / "bin"
        self.bin.mkdir(mode=0o700)
        (self.bin / "ps").write_text(f"#!/bin/sh\nprintf '%s\\n' '{os.geteuid()} cocoa-way-audit-stub'\n")
        (self.bin / "ps").chmod(0o700)
        stub = self.bin / "waypipe"
        stub.write_text("#!/bin/sh\nprintf 'RUNTIME:%s\\nDISPLAY:%s\\n' \"$XDG_RUNTIME_DIR\" \"$WAYLAND_DISPLAY\"\nprintf 'ARG:%s\\n' \"$@\"\n")
        stub.chmod(0o700)
        self.env = os.environ.copy()
        self.env.update({
            "PATH": str(self.bin) + os.pathsep + os.defpath,
            "COCOA_WAY_WAYPIPE": str(stub),
            "COCOA_WAY_RUNTIME_DIR": str(self.directory),
            "XDG_RUNTIME_DIR": str(self.directory),
            "WAYLAND_DISPLAY": "wayland-1",
            "COCOA_WAY_DISPLAY_SLOT": "",
        })

    def run_script(self, *args):
        return subprocess.run(
            ["/bin/bash", str(SCRIPT), *args], env=self.env,
            text=True, capture_output=True, timeout=3,
        )

    def assert_rejected(self):
        result = self.run_script("--display", self.slot, "client")
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertNotIn("RUNTIME:", result.stdout)
        self.assertNotIn(self.slot + "\t", result.stdout)

    def test_trusted_managed_display_and_ssh_argv(self):
        self.env["COCOA_WAY_COMPRESS"] = "none"
        result = self.run_script("--display", self.slot, "ssh", "-i", "/keys/a b's", "guest@host", "foot --title 'x'")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("ARG:--compress=none\nARG:ssh\nARG:-o\nARG:StreamLocalBindUnlink=yes\n", result.stdout)
        self.assertIn("ARG:-i\nARG:/keys/a b's\nARG:guest@host\nARG:foot --title 'x'\n", result.stdout)
        self.assertIn("RUNTIME:" + str(self.directory), result.stdout)

    def test_explicit_default_display(self):
        result = self.run_script("--display", "default", "client")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("RUNTIME:" + str(self.directory), result.stdout)

    def test_fifo_slot_never_blocks(self):
        marker = self.directory / "display.slot"
        marker.unlink()
        os.mkfifo(marker, 0o600)
        self.assert_rejected()
        listed = self.run_script("--list-displays")
        self.assertEqual(listed.returncode, 0, listed.stderr)

    def test_fifo_pid_never_blocks(self):
        marker = self.directory / "display.parent"
        marker.unlink()
        os.mkfifo(marker, 0o600)
        self.assert_rejected()

    def test_symlink_marker_is_rejected(self):
        marker = self.directory / "display.slot"
        content = self.directory / "slot-content"
        marker.rename(content)
        marker.symlink_to(content)
        self.assert_rejected()

    def test_oversized_marker_is_rejected(self):
        (self.directory / "display.slot").write_text(self.slot + " " * 257)
        self.assert_rejected()

    def test_writable_marker_is_rejected(self):
        (self.directory / "display.slot").chmod(0o622)
        self.assert_rejected()

    def test_writable_directory_is_rejected(self):
        self.directory.chmod(0o777)
        self.assert_rejected()

    def test_writable_socket_is_rejected(self):
        self.socket_path.chmod(0o622)
        self.assert_rejected()

    def test_symlink_socket_is_rejected(self):
        real = self.directory / "real-socket"
        self.socket_path.rename(real)
        self.socket_path.symlink_to(real)
        self.assert_rejected()

    def test_regular_file_is_not_a_socket(self):
        self.socket_path.unlink()
        self.socket_path.write_text("not a socket")
        self.assert_rejected()

    def test_worker_marker_is_mandatory(self):
        (self.directory / "display.worker").unlink()
        self.assert_rejected()

    def test_symlink_runtime_directory_is_not_listed(self):
        alias = Path(str(self.directory) + "-link")
        alias.symlink_to(self.directory)
        self.addCleanup(alias.unlink)
        result = self.run_script("--list-displays")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn(str(alias) + "/wayland-1", result.stdout)

    def test_redirectable_runtime_ancestor_is_not_listed(self):
        nested = self.directory / "nested"
        nested.mkdir(mode=0o700)
        alias = self.directory / "alias"
        alias.symlink_to(nested, target_is_directory=True)
        runtime = nested / "runtime"
        runtime.mkdir(mode=0o700)
        listener = socket.socket(socket.AF_UNIX)
        self.addCleanup(listener.close)
        listener.bind(str(runtime / "wayland-1"))
        (runtime / "wayland-1").chmod(0o600)
        redirectable = str(alias / "runtime")
        self.env["XDG_RUNTIME_DIR"] = redirectable
        self.env["COCOA_WAY_RUNTIME_DIR"] = redirectable
        result = self.run_script("--list-displays")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn(redirectable + "/wayland-1", result.stdout)

    def test_pid_uid_is_checked_with_a_stub_not_another_user(self):
        (self.bin / "ps").write_text(f"#!/bin/sh\nprintf '%s\\n' '{os.geteuid() + 1} cocoa-way-audit-stub'\n")
        self.assert_rejected()


if __name__ == "__main__":
    unittest.main()
