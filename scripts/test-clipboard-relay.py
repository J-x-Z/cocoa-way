#!/usr/bin/env python3
"""Exercise the nested clipboard relay without a running compositor."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


REPO = Path(__file__).resolve().parents[1]
RELAY = REPO / "examples/container-images/cocoa-way-clipboard-relay"


class ClipboardRelayTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        for endpoint in ("outer", "inner"):
            (self.root / endpoint).mkdir()
        self.env = dict(os.environ, PATH=f"{self.bin}:{os.environ['PATH']}", FIXTURE=str(self.root))
        self.env.pop("CLIPBOARD_TYPE", None)
        self.env.pop("CLIPBOARD_STATE", None)
        self.mock("wl-paste", '''#!/bin/sh
case "$*" in
  --list-types) cat "$FIXTURE/$WAYLAND_DISPLAY/types" ;;
  *) printf '%s\\n' "$*" >> "$FIXTURE/requests"; cat "$FIXTURE/$WAYLAND_DISPLAY/data" ;;
esac
''')
        self.mock("wl-copy", '''#!/bin/sh
printf '%s\\n' "$*" >> "$FIXTURE/copies"
printf '%s\\n' "$WAYLAND_DISPLAY" >> "$FIXTURE/destinations"
[ "${FAIL_COPY:-0}" = 0 ] || exit 1
cat > "$FIXTURE/$WAYLAND_DISPLAY/data"
printf '%s\\n' "$2" > "$FIXTURE/$WAYLAND_DISPLAY/types"
cp "$FIXTURE/$WAYLAND_DISPLAY/data" "$FIXTURE/copied"
''')
        self.assertIsNotNone(shutil.which("timeout"), "coreutils timeout is required")

    def mock(self, name, code):
        path = self.bin / name
        path.write_text(code)
        path.chmod(0o755)

    def set_selection(self, endpoint, types, data):
        (self.root / endpoint / "types").write_text("\n".join(types) + "\n")
        (self.root / endpoint / "data").write_bytes(data)

    def destinations(self):
        path = self.root / "destinations"
        return path.read_text().splitlines() if path.exists() else []

    def transfer(self, types, data, watched=b"<meta charset='utf-8'><img src='example'>",
                 source="outer", destination="inner"):
        self.set_selection(source, types, data)
        subprocess.run(
            ["sh", str(RELAY), "--transfer", source, destination, str(self.root / "state")],
            input=watched, env=self.env, check=True, timeout=15,
        )

    def test_old_wl_paste_html_watch_fetches_png_instead(self):
        png = (REPO / "assets/icon.png").read_bytes()
        self.transfer(["text/html", "text/plain", "image/png"], png)
        self.assertEqual((self.root / "copied").read_bytes(), png)
        self.assertEqual((self.root / "copies").read_text(), "--type image/png\n")
        self.assertIn("--type image/png", (self.root / "requests").read_text())

    def test_modern_wl_paste_reuses_png_without_a_second_read(self):
        self.env["CLIPBOARD_TYPE"] = "image/png"
        png = (REPO / "assets/icon.png").read_bytes()
        self.transfer(["text/html", "image/png"], png, watched=png)
        self.assertEqual((self.root / "copied").read_bytes(), png)
        self.assertFalse((self.root / "requests").exists())

    def test_plain_text_keeps_newlines_and_suppresses_echo(self):
        data = b"first\nsecond\n"
        self.transfer(["text/plain"], data)
        self.transfer(["text/plain"], data)
        self.transfer(["text/plain"], data, source="inner", destination="outer")
        self.transfer(["text/plain"], data)
        self.assertEqual((self.root / "copied").read_bytes(), data)
        self.assertEqual(len((self.root / "copies").read_text().splitlines()), 1)

    def test_same_bytes_with_different_mime_are_not_deduplicated(self):
        self.transfer(["text/plain"], b"payload")
        self.transfer(["image/png"], b"payload")
        self.assertEqual(len((self.root / "copies").read_text().splitlines()), 2)

    def test_html_only_is_not_mislabeled_as_plain_text(self):
        self.transfer(["text/html"], b"<img src='example'>")
        self.assertFalse((self.root / "copies").exists())

    def test_same_text_restores_unsupported_destination_in_both_directions(self):
        for source, destination, clipboard_type in (
            ("outer", "inner", None), ("inner", "outer", None),
            ("outer", "inner", "text/plain"), ("inner", "outer", "text/plain"),
        ):
            with self.subTest(source=source, clipboard_type=clipboard_type):
                if clipboard_type:
                    self.env["CLIPBOARD_TYPE"] = clipboard_type
                else:
                    self.env.pop("CLIPBOARD_TYPE", None)
                before = len(self.destinations())
                data = f"A from {source} ({clipboard_type})".encode()
                self.transfer(["text/plain"], data, watched=data, source=source, destination=destination)
                self.transfer(["text/plain"], data, watched=data, source=destination, destination=source)
                self.transfer(["text/html"], b"<b>unsupported</b>",
                              source=destination, destination=source)
                self.assertEqual(len(self.destinations()), before + 1)
                self.assertEqual((self.root / source / "data").read_bytes(), data)
                self.transfer(["text/plain"], data, watched=data, source=source, destination=destination)
                self.assertEqual((self.root / destination / "data").read_bytes(), data)
                self.assertEqual((self.root / destination / "types").read_text(), "text/plain\n")
                self.transfer(["text/plain"], data, watched=data, source=destination, destination=source)
                self.transfer(["text/plain"], data, watched=data, source=source, destination=destination)
                self.assertEqual(self.destinations()[before:], [destination, destination])

    def test_interleaved_endpoint_changes_do_not_create_feedback_copies(self):
        self.transfer(["text/plain"], b"A")
        self.transfer(["text/plain"], b"B", source="inner", destination="outer")
        self.transfer(["text/plain"], b"B")
        self.transfer(["text/html"], b"<b>unsupported</b>")
        self.transfer(["text/plain"], b"B", source="inner", destination="outer")
        self.transfer(["text/plain"], b"B")
        self.transfer(["text/plain"], b"B", source="inner", destination="outer")
        self.assertEqual(self.destinations(), ["inner", "outer", "outer"])
        self.assertEqual((self.root / "inner/data").read_bytes(), b"B")
        self.assertEqual((self.root / "outer/data").read_bytes(), b"B")

    def test_unsupported_source_does_not_forget_unchanged_destination(self):
        for source, destination in (("outer", "inner"), ("inner", "outer")):
            with self.subTest(source=source):
                before = len(self.destinations())
                data = f"unchanged at {destination}".encode()
                self.transfer(["text/plain"], data, source=source, destination=destination)
                self.transfer(["text/html"], b"<b>unsupported</b>",
                              source=source, destination=destination)
                self.transfer(["text/plain"], data, source=source, destination=destination)
                self.transfer(["text/plain"], data, source=destination, destination=source)
                self.assertEqual(self.destinations()[before:], [destination])

    def test_empty_unsupported_watch_invalidates_destination_without_copying_empty(self):
        for source, destination in (("outer", "inner"), ("inner", "outer")):
            with self.subTest(source=source):
                before = len(self.destinations())
                data = f"restore {destination}".encode()
                self.transfer(["text/plain"], data, source=source, destination=destination)
                self.transfer(["text/html"], b"<b>unsupported</b>", watched=b"",
                              source=destination, destination=source)
                self.assertEqual(self.destinations()[before:], [destination])
                self.assertEqual((self.root / source / "data").read_bytes(), data)
                self.transfer(["text/plain"], data, source=source, destination=destination)
                self.transfer(["text/plain"], data, source=destination, destination=source)
                self.assertEqual((self.root / destination / "data").read_bytes(), data)
                self.assertEqual(self.destinations()[before:], [destination, destination])

    def test_empty_startup_selection_does_not_erase_other_endpoint(self):
        for source, destination in (("outer", "inner"), ("inner", "outer")):
            with self.subTest(source=source):
                self.set_selection(destination, ["text/plain"], b"existing clipboard")
                self.transfer([], b"", watched=b"", source=source, destination=destination)
                self.assertEqual((self.root / destination / "data").read_bytes(), b"existing clipboard")
                self.assertEqual(self.destinations(), [])

    def test_failed_copy_does_not_suppress_retry(self):
        self.env["FAIL_COPY"] = "1"
        self.transfer(["text/plain"], b"retry")
        self.env["FAIL_COPY"] = "0"
        self.transfer(["text/plain"], b"retry")
        self.assertEqual((self.root / "copied").read_bytes(), b"retry")
        self.assertEqual(len((self.root / "copies").read_text().splitlines()), 2)


if __name__ == "__main__":
    unittest.main()
