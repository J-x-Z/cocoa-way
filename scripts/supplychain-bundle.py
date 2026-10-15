#!/usr/bin/env python3
"""Build and verify a macOS app using the same path in CI and releases.

Requires already-built --locked release binaries and native macOS tools. Never
downloads or starts a container/VM/image. Refuses an existing output directory.
Use --check to verify an existing bundle without changing it.
"""

import argparse
import os
from pathlib import Path
import platform
import plistlib
import re
import shutil
import subprocess
import tomllib

ROOT = Path(__file__).resolve().parent.parent
EXECUTABLES = ("cocoa-way", "cocoa-wayctl", "cocoa-way-mcp")


def require(condition, message):
    if not condition:
        raise ValueError(message)


def run(*command):
    return subprocess.check_output(command, text=True, cwd=ROOT)


def inside(path, app):
    require(path.resolve().is_relative_to(app.resolve()), f"Path escapes app: {path}")
    require(path.is_file(), f"Missing bundled dependency: {path}")
    return path.resolve()


def system_library(name):
    # Normalize before checking so /usr/lib/../../opt is not mistaken for system.
    normalized = os.path.normpath(name)
    return normalized.startswith(("/usr/lib/", "/System/Library/"))


def expand_path(name, binary, executable):
    for prefix, base in (("@loader_path", binary.parent), ("@executable_path", executable.parent)):
        if name == prefix:
            return base
        if name.startswith(prefix + "/"):
            return base / name[len(prefix) + 1:]
    raise ValueError(f"External or unsupported load path in {binary}: {name}")


def rpaths(load_commands):
    return re.findall(r"cmd LC_RPATH\n.*?\n\s*path (.+?) \(offset \d+\)", load_commands)


def minimum_versions(load_commands):
    return re.findall(r"cmd LC_(?:BUILD_VERSION|VERSION_MIN_MACOSX)\n.*?\n\s*(?:minos|version) ([0-9.]+)", load_commands, re.DOTALL)


def version_tuple(version):
    parts = tuple(int(part) for part in version.split("."))
    return parts + (0,) * (3 - len(parts))


def resolve_dependency(name, binary, executable, search_paths, app):
    require("pixman" not in name.lower(), f"Unexpected pixman linkage: {name}")
    if system_library(name):
        return None
    if name.startswith("@rpath/"):
        candidates = [path / name[len("@rpath/"):] for path in search_paths]
        require(candidates, f"No rpath for {name} in {binary}")
        # Reject an escaping candidate even if another rpath would resolve it.
        for path in candidates:
            require(path.resolve().is_relative_to(app.resolve()), f"rpath escapes app: {path}")
        matches = [inside(path, app) for path in candidates if path.is_file()]
        require(matches and len(set(matches)) == 1, f"Missing or ambiguous rpath dependency: {name}")
        return matches[0]
    return inside(expand_path(name, binary, executable), app)


def check_bundle(app, version, arch):
    app = app.resolve()
    with (app / "Contents/Info.plist").open("rb") as stream:
        info = plistlib.load(stream)
    require(info["CFBundleVersion"] == info["CFBundleShortVersionString"] == version, "Bundle version mismatch")
    require(info["CFBundleExecutable"] == "cocoa-way", "Wrong bundle executable")
    declared_minimum = version_tuple(info["LSMinimumSystemVersion"])
    binaries = [app / "Contents/MacOS" / name for name in EXECUTABLES]
    framework_dir = app / "Contents/Frameworks"
    require(framework_dir.is_dir() and not framework_dir.is_symlink(), "Missing/invalid Frameworks directory")
    for path in framework_dir.rglob("*"):
        require(not path.is_symlink(), f"Symlinked framework needs review: {path}")
    libraries = [path for path in framework_dir.rglob("*") if path.is_file()]
    for binary in binaries + libraries:
        inside(binary, app)
        require(not binary.is_symlink(), f"Symlinked Mach-O: {binary}")
        require(arch in run("lipo", "-archs", str(binary)).split(), f"Wrong architecture in {binary}")
        commands = run("otool", "-l", str(binary))
        minimums = minimum_versions(commands)
        require(minimums, f"No macOS deployment target in {binary}")
        require(all(version_tuple(value) <= declared_minimum for value in minimums),
                f"Bundle understates minimum macOS for {binary}")
        for entry in rpaths(commands):
            path = expand_path(entry, binary, binaries[0])
            require(path.resolve().is_relative_to(app), f"External LC_RPATH in {binary}: {entry}")
        run("codesign", "--verify", "--strict", str(binary))
    visited = set()
    for executable in binaries:
        require(os.access(executable, os.X_OK), f"Non-executable helper: {executable}")
        executable_rpaths = [expand_path(name, executable, executable)
                             for name in rpaths(run("otool", "-l", str(executable)))]
        pending = [executable]
        per_executable = set()
        while pending:
            binary = pending.pop()
            if binary in per_executable:
                continue
            per_executable.add(binary)
            visited.add(binary)
            search_paths = [expand_path(name, binary, executable)
                            for name in rpaths(run("otool", "-l", str(binary)))] + executable_rpaths
            for line in run("otool", "-L", str(binary)).splitlines()[1:]:
                name = line.strip().split(" (compatibility version", 1)[0]
                require(name, f"Malformed dependency in {binary}")
                resolved = resolve_dependency(name, binary, executable, search_paths, app)
                if resolved:
                    pending.append(resolved)
    require(set(libraries).issubset(visited), "Bundle has unexpected unreferenced framework files")
    resources = app / "Contents/Resources"
    for relative in ("rules/evdev", "keycodes/evdev", "symbols/us"):
        inside(resources / "xkb" / relative, app)
    run("bash", "scripts/bundle-example-resources.sh", "--check", str(resources / "container-images"))
    run("codesign", "--verify", "--deep", "--strict", str(app))
    subprocess.run([str(binaries[1]), "--help"], check=True, stdout=subprocess.DEVNULL, timeout=15)
    subprocess.run([str(binaries[2])], check=True, input=b"", stdout=subprocess.DEVNULL, timeout=15)
    print(f"Verified {len(binaries)} executables, {len(libraries)} transitive dylibs, resources and ad-hoc signatures")


def create_bundle(app, version):
    require(not app.exists() and not app.is_symlink(), f"Output already exists: {app}")
    binaries = app / "Contents/MacOS"
    resources = app / "Contents/Resources"
    binaries.mkdir(parents=True)
    resources.mkdir()
    for name in EXECUTABLES:
        shutil.copy2(ROOT / "target/release" / name, binaries / name)
    shutil.copy2(ROOT / "assets/icon.png", resources / "AppIcon.png")
    info = {
        "CFBundleExecutable": "cocoa-way", "CFBundleIdentifier": "com.jxz.cocoa-way",
        "CFBundleName": "Cocoa-Way", "CFBundleDisplayName": "Cocoa-Way",
        "CFBundleVersion": version, "CFBundleShortVersionString": version,
        "CFBundlePackageType": "APPL", "CFBundleIconFile": "AppIcon",
        "LSMinimumSystemVersion": "11.0", "NSHighResolutionCapable": True,
        "LSApplicationCategoryType": "public.app-category.developer-tools",
    }
    with (app / "Contents/Info.plist").open("wb") as stream:
        plistlib.dump(info, stream)
    run("bash", "scripts/bundle-example-resources.sh", str(resources / "container-images"))
    run("bash", "scripts/bundle-macos-app.sh", str(app))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("app", type=Path)
    parser.add_argument("--version")
    parser.add_argument("--arch", default=platform.machine())
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    require(platform.system() == "Darwin", "Native macOS bundle tools are required")
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    require(args.version in (None, version), "Release tag/package version mismatch")
    app = args.app.absolute()
    if not args.check:
        create_bundle(app, version)
    check_bundle(app, version, args.arch)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, KeyError, OSError, subprocess.SubprocessError) as error:
        raise SystemExit(f"bundle check failed: {error}") from error
