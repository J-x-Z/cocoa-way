#!/usr/bin/env python3
"""Verify vendored Smithay; optionally reproduce it from a local upstream clone.

Normal builds need neither this script nor a Git checkout. --source never fetches
or changes the supplied repository. --write replaces only vendor/smithay.
"""

import argparse
import hashlib
import os
from pathlib import Path
import subprocess
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parent.parent
VENDOR = ROOT / "vendor" / "smithay"


def tree_hash(directory):
    digest = hashlib.sha256()
    for path in sorted(directory.rglob("*")):
        if path.is_symlink():
            raise ValueError(f"Unexpected symlink: {path}")
        if path.is_file():
            if path.name == ".DS_Store":
                continue
            relative = path.relative_to(directory).as_posix().encode()
            digest.update(relative + b"\0")
            digest.update(hashlib.sha256(path.read_bytes()).digest())
    return digest.hexdigest()


def run(command, **kwargs):
    return subprocess.run(command, check=True, **kwargs)


def reproduce(source, destination, manifest):
    def git(*arguments):
        return run(["git", "-C", str(source), *arguments], capture_output=True).stdout

    archive = git("archive", manifest["revision"], *manifest["source_paths"])
    run(["tar", "-x", "-C", str(destination)], input=archive)
    for revision in manifest["backports"]:
        patch = git("show", "--format=", "--binary", revision, "--", "src")
        if not patch:
            raise ValueError(f"Empty backport: {revision}")
        run(["git", "apply", "-"], cwd=destination, input=patch)
    for relative in manifest["patches"]:
        patch = ROOT / "vendor" / relative
        run(["git", "apply", str(patch)], cwd=destination)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, help="Local Smithay Git checkout containing the pinned commits")
    parser.add_argument("--write", action="store_true", help="Replace a clean vendor tree with the reproduced source")
    args = parser.parse_args()
    manifest = tomllib.loads((ROOT / "vendor" / "sources.toml").read_text())["smithay"]
    expected = manifest["tree_sha256"]
    if args.write and not args.source:
        parser.error("--write requires --source")
    if not args.source:
        actual = tree_hash(VENDOR)
        if not expected or actual != expected:
            raise ValueError(f"Vendor hash mismatch: expected {expected}, found {actual}")
        print(f"Smithay vendor verified: {actual}")
        return

    # Keep staging outside a Git checkout so git apply cannot silently skip paths.
    with tempfile.TemporaryDirectory(prefix="cocoa-way-smithay-") as temporary:
        staged = Path(temporary) / "smithay"
        staged.mkdir()
        reproduce(args.source.resolve(), staged, manifest)
        actual = tree_hash(staged)
        if expected and expected != actual:
            raise ValueError(f"Reproduced hash mismatch: expected {expected}, found {actual}")
        if args.write:
            run(["git", "diff", "--exit-code", "--", "vendor/smithay"], cwd=ROOT)
            run(["git", "diff", "--cached", "--exit-code", "--", "vendor/smithay"], cwd=ROOT)
            untracked = run(
                ["git", "ls-files", "--others", "--exclude-standard", "--", "vendor/smithay"],
                cwd=ROOT, capture_output=True,
            ).stdout
            if untracked:
                raise ValueError("Refusing to replace untracked files in vendor/smithay")
            # Preserve the old directory until a same-filesystem rename succeeds.
            import shutil

            (ROOT / "target").mkdir(exist_ok=True)
            with tempfile.TemporaryDirectory(prefix="vendor-smithay-", dir=ROOT / "target") as replacement:
                replacement = Path(replacement)
                shutil.copytree(staged, replacement / "new")
                os.replace(VENDOR, replacement / "old")
                try:
                    os.replace(replacement / "new", VENDOR)
                except OSError:
                    os.replace(replacement / "old", VENDOR)
                    raise
        elif actual != tree_hash(VENDOR):
            raise ValueError("Vendored source differs from the pinned upstream source plus patches")
        print(f"Smithay reproduced: {actual}")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        raise SystemExit(str(error)) from error
