#!/usr/bin/env python3
"""Check the workspace lock, CURRENT RustSec advisories, API scope and provenance.

Run with Python 3.11+ and cargo-audit at the version in supplychain/policy.toml.
--db/--no-fetch allow an explicitly selected local RustSec snapshot for replay.
The workspace Cargo.lock is authoritative; Cargo ignores test-client/Cargo.lock.
No target-wide or advisory-wide ignores are passed to cargo-audit. Unknown
findings, including lock-only findings and informational notices, fail closed.
"""

import argparse
from datetime import date
import hashlib
import io
import json
from pathlib import Path
import re
import subprocess
import tarfile
import tomllib

ROOT = Path(__file__).resolve().parent.parent
REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"


def require(condition, message):
    if not condition:
        raise ValueError(message)


def run(*command, root=ROOT):
    return subprocess.check_output(command, cwd=root, text=True)


def repository_files(root=ROOT):
    # Tracked README.md is included even if local docs are ignored. Do not use
    # a blanket Markdown/docs exclusion to construct this inventory.
    names = run("git", "ls-files", "-z", "--cached", "--others", "--exclude-standard", root=root)
    return sorted({root / name for name in names.split("\0") if name})


def check_actions(text, policy):
    allowed = {(entry["repository"], entry["sha"]) for entry in policy["actions"]}
    uses = re.findall(r"^\s*(?:-\s+)?uses:\s*([^#\n]+)", text, re.MULTILINE)
    require(uses, "Workflow has no auditable action references")
    require(len(uses) == len(re.findall(r"\b[\"']?uses[\"']?\s*:", text)),
            "Action references must use explicit block-style uses: entries")
    for value in uses:
        match = re.fullmatch(r"([\w.-]+/[\w.-]+)@([0-9a-f]{40})", value.strip().strip("\"'"))
        require(match and match.groups() in allowed, f"Unverified or mutable action: {value.strip()}")


def check_lock(lock, root=ROOT):
    require(not (root / "test-client/Cargo.lock").exists(),
            "Remove the stale workspace-member lockfile; the root Cargo.lock is authoritative")
    for package in lock["package"]:
        if "source" in package:
            require(package["source"] == REGISTRY, f"Unapproved lock source: {package['name']}")
            require(re.fullmatch(r"[0-9a-f]{64}", package.get("checksum", "")),
                    f"Missing registry checksum: {package['name']}")
        else:
            require(package["name"] in {"cocoa-way", "test-client", "smithay"},
                    f"Unapproved path package: {package['name']}")
    client = tomllib.loads((root / "test-client/Cargo.toml").read_text())
    require(set(client["dependencies"]) == {"wayland-client"}, "Review test-client dependencies")


def reachable_graph(metadata):
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    pending = list(metadata["workspace_members"])
    reachable = set()
    while pending:
        identifier = pending.pop()
        if identifier not in reachable:
            reachable.add(identifier)
            pending.extend(nodes[identifier]["dependencies"])
    return {key: nodes[key] for key in reachable}


def check_api_sources(paths, tokens):
    pattern = re.compile(rb"\b(?:" + b"|".join(re.escape(token.encode()) for token in tokens) + rb")\b")
    for path in paths:
        if path.suffix == ".rs" and path.is_file():
            require(not path.is_symlink(), f"Symlinked Rust source needs review: {path}")
            require(not pattern.search(path.read_bytes()), f"Unreviewed advisory API in {path}")


def check_graph(metadata, policy, paths, root=ROOT):
    graph = reachable_graph(metadata)
    packages = {package["id"]: package for package in metadata["packages"]}
    local = {root.resolve(), (root / "test-client").resolve(), (root / "vendor/smithay").resolve()}
    for identifier in graph:
        package = packages[identifier]
        if package["source"] is None:
            require(Path(package["manifest_path"]).parent.resolve() in local,
                    f"Unapproved local dependency: {package['manifest_path']}")
        else:
            require(package["source"] == REGISTRY, f"Unapproved dependency source: {identifier}")
    cgmath = [identifier for identifier in graph if packages[identifier]["name"] == "cgmath"]
    require(len(cgmath) == 1, "cgmath exception needs re-review after graph changes")
    identifier = cgmath[0]
    require(packages[identifier]["version"] == policy["cgmath"]["version"], "cgmath version changed")
    consumers = {f"{packages[key]['name']}@{packages[key]['version']}" for key, node in graph.items()
                 if identifier in node["dependencies"]}
    require(consumers == set(policy["cgmath"]["consumers"]), f"cgmath consumers changed: {consumers}")
    check_api_sources(paths, policy["cgmath"]["forbidden_rust_tokens"])
    return {(packages[key]["name"], packages[key]["version"]) for key in graph}


def evaluate_audit(report, policy, graphs, today=None):
    today = today or date.today()
    require(report["settings"]["ignore"] == [], "cargo-audit ignores are forbidden")
    require(not report["settings"]["target_os"] and not report["settings"]["target_arch"],
            "Audit must cover the entire lockfile, not filter by advisory platform metadata")
    require(report["settings"]["severity"] is None, "Audit severity filtering is forbidden")
    require(set(report["settings"]["informational_warnings"]) == {"unmaintained", "unsound", "notice"},
            "All informational advisory classes must be reported")
    require(report["database"]["advisory-count"] > 0, "Empty advisory database")
    exceptions = {entry["id"]: entry for entry in policy["exceptions"]}
    findings = list(report["vulnerabilities"]["list"])
    require(report["vulnerabilities"]["count"] == len(findings), "Inconsistent vulnerability report")
    for warnings in report["warnings"].values():
        findings.extend(warnings)
    seen = set()
    lines = []
    for finding in findings:
        advisory = finding.get("advisory") or {}
        identifier = advisory.get("id", "unknown warning")
        package = finding["package"]
        key = (package["name"], package["version"])
        targets = [target for target, graph in graphs.items() if key in graph]
        scope = ", ".join(targets) if targets else "lock-only (not in supported macOS graphs)"
        exception = exceptions.get(identifier)
        require(exception is not None, f"Unreviewed finding {identifier} for {key}: {scope}")
        require(exception["expires"] >= today, f"Expired advisory exception: {identifier}")
        require(key == (exception["package"], exception["version"]), f"Exception version mismatch: {identifier}")
        require(package["source"] == REGISTRY, f"Exception source mismatch: {identifier}")
        require(advisory.get("informational") == exception["kind"], f"Advisory classification changed: {identifier}")
        require(not finding["versions"]["patched"], f"A fix is now available for {identifier}; remove the exception")
        if identifier == "RUSTSEC-2026-0197":
            expected = {f"cgmath::Matrix{size}::swap_columns" for size in (2, 3, 4)}
            require(set(finding["affected"]["functions"]) == expected, "cgmath affected APIs changed")
        seen.add(identifier)
        lines.append(f"REVIEWED {identifier} {key[0]} {key[1]} [{scope}]: {exception['reason']}")
    require(seen == set(exceptions), "Stale advisory exceptions; re-review and remove unused entries")
    return lines


def check_formula(text, policy, version):
    formula = policy["formula"]
    require(formula["version"] == version, "Formula/repository version drift; verify a real release before updating")
    for field in ("url", "sha256"):
        match = re.search(rf'^  {field} "([^"]+)"$', text, re.MULTILINE)
        require(match and match[1] == formula[field], f"Formula {field} differs from verified release")
    require('depends_on "pixman"' not in text, "Unused pixman dependency returned")
    require('system "cargo", "install", *std_cargo_args' in text, "Formula must use Homebrew's locked Cargo arguments")


def fetch_bytes(url, limit=20_000_000):
    require(url.startswith("https://"), "HTTPS is required")
    # Use macOS's configured curl trust store, not a Python-specific CA bundle.
    data = subprocess.check_output([
        "curl", "--fail", "--silent", "--show-error", "--location", "--proto", "=https",
        "--proto-redir", "=https", "--max-time", "60", "--max-filesize", str(limit),
        "--user-agent", "cocoa-way-supplychain", url,
    ])
    require(len(data) <= limit, f"Unexpectedly large response: {url}")
    return data


def verify_upstreams(policy):
    for action in policy["actions"]:
        url = f"https://api.github.com/repos/{action['repository']}/commits/{action['sha']}"
        commit = json.loads(fetch_bytes(url))
        require(commit["sha"] == action["sha"] and commit["commit"]["verification"]["verified"],
                f"Action commit provenance failed: {action['repository']}")
    formula = policy["formula"]
    data = fetch_bytes(formula["url"])
    require(hashlib.sha256(data).hexdigest() == formula["sha256"], "Published Formula archive checksum changed")
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as archive:
        member = archive.extractfile(f"cocoa-way-{formula['version']}/Cargo.toml")
        require(member is not None, "Release archive has no package manifest")
        require(tomllib.loads(member.read().decode())["package"]["version"] == formula["version"],
                "Release archive version mismatch")
    tag = json.loads(fetch_bytes("https://api.github.com/repos/J-x-Z/cocoa-way/git/ref/tags/v" + formula["version"]))
    require(tag["object"]["sha"] == formula["tag_object"], "Published release tag moved")
    tag = json.loads(fetch_bytes(tag["object"]["url"]))
    require(tag["object"]["sha"] == formula["commit"], "Published release commit changed")
    print("Verified upstream action commits and published Formula archive/tag")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--db", type=Path)
    parser.add_argument("--no-fetch", action="store_true")
    parser.add_argument("--verify-upstreams", action="store_true", help="Recheck GitHub provenance and download only the small source archive")
    args = parser.parse_args()
    require(not args.no_fetch or args.db is not None, "--no-fetch requires an explicit --db snapshot")
    policy = tomllib.loads((ROOT / "supplychain/policy.toml").read_text())
    files = repository_files()
    for path in files:
        if path.parent == ROOT / ".github/workflows" and path.suffix in {".yml", ".yaml"}:
            check_actions(path.read_text(), policy)
    lock_bytes = (ROOT / "Cargo.lock").read_bytes()
    lock = tomllib.loads(lock_bytes.decode())
    check_lock(lock)
    cgmath = [package for package in lock["package"] if package["name"] == "cgmath"]
    require(len(cgmath) == 1 and cgmath[0]["checksum"] == policy["cgmath"]["checksum"], "cgmath checksum changed")
    manifest = tomllib.loads((ROOT / "Cargo.toml").read_text())
    check_formula((ROOT / "Formula/cocoa-way.rb").read_text(), policy, manifest["package"]["version"])
    print(run("python3", "scripts/vendor-smithay.py").strip())
    graphs = {}
    for target in policy["targets"]:
        metadata = json.loads(run("cargo", "metadata", "--locked", "--format-version", "1", "--filter-platform", target))
        graphs[target] = check_graph(metadata, policy, files)
        print(f"Verified locked graph and cgmath API scope: {target} ({len(graphs[target])} packages)")
    require((ROOT / "Cargo.lock").read_bytes() == lock_bytes, "Cargo.lock changed during metadata checks")
    # cargo-audit 0.22.2 currently reports "cargo-audit-audit" via Cargo.
    scanner = run("cargo", "audit", "--version").strip().split()
    require(len(scanner) == 2 and scanner[0] in {"cargo-audit", "cargo-audit-audit"}
            and scanner[1] == policy["cargo_audit_version"], "Install the policy's pinned cargo-audit version")
    command = ["cargo", "audit", "--json", "--deny", "warnings", "--file", str(ROOT / "Cargo.lock"),
               "--url", "https://github.com/RustSec/advisory-db.git"]
    if args.db:
        command.extend(["--db", str(args.db.resolve())])
    if args.no_fetch:
        command.append("--no-fetch")
        print(f"Snapshot replay only, not a fresh fetch: {run('git', '-C', str(args.db.resolve()), 'rev-parse', 'HEAD').strip()}")
    result = subprocess.run(command, cwd=ROOT, text=True, capture_output=True)
    if result.stderr:
        print(result.stderr, end="")
    require(result.returncode in (0, 1), f"cargo-audit failed with exit {result.returncode}")
    report = json.loads(result.stdout)
    output = ROOT / "target/supplychain-audit.json"
    output.parent.mkdir(exist_ok=True)
    output.write_text(json.dumps(report, indent=2) + "\n")
    print(f"RustSec database: {report['database']['last-commit'] or 'see explicit snapshot revision above'}")
    for line in evaluate_audit(report, policy, graphs):
        print(line)
    if args.verify_upstreams:
        verify_upstreams(policy)
    print(f"Supply-chain checks passed; raw unsuppressed audit: {output}")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, KeyError, OSError, subprocess.CalledProcessError) as error:
        raise SystemExit(f"supply-chain check failed: {error}") from error
