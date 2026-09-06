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
        self.env = dict(os.environ, PATH=f"{self.bin}:{os.environ['PATH']}", FIXTURE=str(self.root))
        self.env.pop("CLIPBOARD_TYPE", None)
        self.env.pop("CLIPBOARD_STATE", None)
        self.mock("wl-paste", '''#!/bin/sh
case "$*" in
  --list-types) cat "$FIXTURE/types" ;;
  *) printf '%s\\n' "$*" >> "$FIXTURE/requests"; cat "$FIXTURE/data" ;;
esac
''')
        self.mock("wl-copy", '''#!/bin/sh
printf '%s\\n' "$*" >> "$FIXTURE/copies"
[ "${FAIL_COPY:-0}" = 0 ] || exit 1
cat > "$FIXTURE/copied"
''')
        self.assertIsNotNone(shutil.which("timeout"), "coreutils timeout is required")

    def mock(self, name, code):
        path = self.bin / name
        path.write_text(code)
        path.chmod(0o755)

    def transfer(self, types, data, watched=b"<meta charset='utf-8'><img src='example'>"):
        (self.root / "types").write_text("\n".join(types) + "\n")
        (self.root / "data").write_bytes(data)
        subprocess.run(
            ["sh", str(RELAY), "--transfer", "outer", "inner", str(self.root / "state")],
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
        self.assertEqual((self.root / "copied").read_bytes(), data)
        self.assertEqual(len((self.root / "copies").read_text().splitlines()), 1)

    def test_same_bytes_with_different_mime_are_not_deduplicated(self):
        self.transfer(["text/plain"], b"payload")
        self.transfer(["image/png"], b"payload")
        self.assertEqual(len((self.root / "copies").read_text().splitlines()), 2)

    def test_html_only_is_not_mislabeled_as_plain_text(self):
        self.transfer(["text/html"], b"<img src='example'>")
        self.assertFalse((self.root / "copies").exists())

    def test_failed_copy_does_not_suppress_retry(self):
        self.env["FAIL_COPY"] = "1"
        self.transfer(["text/plain"], b"retry")
        self.env["FAIL_COPY"] = "0"
        self.transfer(["text/plain"], b"retry")
        self.assertEqual((self.root / "copied").read_bytes(), b"retry")
        self.assertEqual(len((self.root / "copies").read_text().splitlines()), 2)


if __name__ == "__main__":
    unittest.main()
