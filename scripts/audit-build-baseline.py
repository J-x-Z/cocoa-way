#!/usr/bin/env python3
"""Build a provenance-recorded first-batch baseline without trusting target/release.

No checkout, source edits, network, GUI or containers. A fresh release-only target
is capped at 900 MiB, not a copy of the multi-GiB debug target. This helper never
overwrites the working tree's candidate binary or shares mutable Cargo artifacts.
"""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import sys
import tarfile
import time

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parent.parent
COMMIT = "3e3d6b91ee2c0caba0cfce9eff7f99e96d8e4be2"
LIMIT_BYTES = 900 * 2**20


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "target/audit-baseline-3e3d6b91")
    args = parser.parse_args()
    spec = importlib.util.spec_from_file_location("audit_bench", ROOT / "scripts/audit-bench.py")
    bench = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(bench)
    command = bench.command
    output = args.output.resolve()
    output.mkdir(mode=0o700, parents=True, exist_ok=False)
    source = output / "source"
    source.mkdir()
    report = {"schema": 1, "kind": "verified_source_baseline_build", "status": "in_progress",
              "commit": COMMIT, "network": "cargo --offline", "profile": "release", "jobs": 2,
              "build_target_limit_bytes": LIMIT_BYTES, "candidate_target_modified": False,
              "app_or_container_started": False}
    def save():
        (output / "provenance.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    save()
    try:
        code, resolved = command(["git", "-C", str(ROOT), "rev-parse", COMMIT + "^{commit}"])
        if code or resolved.strip() != COMMIT:
            raise RuntimeError("requested immutable baseline commit is unavailable")
        code, tree = command(["git", "-C", str(ROOT), "rev-parse", COMMIT + "^{tree}"])
        if code or not re.fullmatch(r"[0-9a-f]{40}\n", tree):
            raise RuntimeError("could not verify baseline tree")
        report["tree"] = tree.strip()
        code, listing = command(["git", "-C", str(ROOT), "ls-tree", "-r", "-l", COMMIT])
        if code:
            raise RuntimeError("could not bound baseline source tree")
        source_bytes = sum(int(line.split(None, 4)[3]) for line in listing.splitlines())
        if source_bytes > 64 * 2**20:
            raise ValueError("baseline source exceeds 64 MiB cap")
        archive = output / "source.tar"
        code, _ = command(["git", "-C", str(ROOT), "archive", "--format=tar", "--output=" + str(archive), COMMIT])
        if code:
            raise RuntimeError("baseline archive failed")
        report["source_archive_sha256"] = hashlib.sha256(archive.read_bytes()).hexdigest()
        report["tracked_source_bytes"] = source_bytes
        with tarfile.open(archive) as stream:
            stream.extractall(source, filter="data")
        report["cargo_lock_sha256"] = hashlib.sha256((source / "Cargo.lock").read_bytes()).hexdigest()
        code, compiler = command(["rustc", "--version", "--verbose"])
        if code:
            raise RuntimeError("rustc unavailable")
        report["rustc"] = compiler.strip()
        build = output / "build"
        build_args = ["cargo", "build", "--manifest-path", str(source / "Cargo.toml"), "--offline", "--locked",
                      "--release", "--bin", "cocoa-way", "--jobs", "2", "--target-dir", str(build)]
        report["build_command"] = build_args
        save()
        checked = [0.0]
        def storage_guard():
            if time.monotonic() - checked[0] < 5:
                return
            checked[0] = time.monotonic()
            size = sum(path.stat().st_size for path in build.rglob("*") if path.is_file())
            report["build_target_logical_bytes"] = size
            if size > LIMIT_BYTES:
                raise ValueError("release-only target exceeded 900 MiB; owned compiler group stopped")
        code, log = command(build_args, timeout=600, tick=storage_guard)
        (output / "build.log").write_text(log, encoding="utf-8")
        if code:
            raise RuntimeError("offline baseline build failed; no dependency fetch attempted")
        binary = build / "release/cocoa-way"
        report["binary"] = str(binary)
        report["binary_sha256"] = hashlib.sha256(binary.read_bytes()).hexdigest()
        report["build_target_logical_bytes"] = sum(path.stat().st_size for path in build.rglob("*") if path.is_file())
        report["status"] = "ok"
        save()
        print(f"Verified baseline build: {binary}")
        print(f"SHA256: {report['binary_sha256']}")
        print(f"Release-only build target: {report['build_target_logical_bytes']/2**20:.1f} MiB")
    except BaseException as error:
        report["status"] = "error"
        report["error_type"] = type(error).__name__
        save()
        raise
    return 0


if __name__ == "__main__":
    sys.exit(main())
