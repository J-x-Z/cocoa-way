#!/usr/bin/env python3
"""Offline regression tests for supply-chain failure paths; no builds/downloads."""

import copy
from datetime import date
import importlib.util
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import tomllib
import unittest
from unittest import mock

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parent.parent


def load(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / "scripts" / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


checks = load("check-supplychain")
bundle = load("supplychain-bundle")
POLICY = tomllib.loads((ROOT / "supplychain/policy.toml").read_text())
TODAY = date(2026, 10, 15)


def audit_report():
    findings = []
    for exception in POLICY["exceptions"]:
        functions = {f"cgmath::Matrix{size}::swap_columns": ["=0.18.0"] for size in (2, 3, 4)}
        findings.append({
            "advisory": {"id": exception["id"], "informational": exception["kind"]},
            "package": {"name": "cgmath", "version": "0.18.0", "source": checks.REGISTRY},
            "versions": {"patched": []}, "affected": {"functions": functions},
        })
    return {
        "settings": {"ignore": [], "target_os": [], "target_arch": [], "severity": None,
                     "informational_warnings": ["unmaintained", "unsound", "notice"]},
        "database": {"advisory-count": 1329},
        "vulnerabilities": {"count": 0, "list": []},
        "warnings": {"unmaintained": [findings[0]], "unsound": [findings[1]]},
    }


class AuditTests(unittest.TestCase):
    def setUp(self):
        self.report = audit_report()
        self.graphs = {target: {("cgmath", "0.18.0")} for target in POLICY["targets"]}

    def test_api_credential_stays_in_stdin_and_cannot_follow_redirects(self):
        with mock.patch.dict(checks.os.environ, {"GITHUB_TOKEN": "test-token"}), \
                mock.patch.object(checks.subprocess, "check_output", return_value=b"{}") as fetch:
            checks.fetch_bytes("https://api.github.com/repos/actions/checkout/commits/test")
            args, kwargs = fetch.call_args
            self.assertNotIn("--location", args[0])
            self.assertNotIn("test-token", " ".join(args[0]))
            self.assertIn("@-", args[0])
            self.assertEqual(kwargs["input"], b"Authorization: Bearer test-token\n")
            for url in ["https://github.com/source.tar.gz", "https://api.github.com.example.com/test"]:
                checks.fetch_bytes(url)
                args, kwargs = fetch.call_args
                self.assertIn("--location", args[0])
                self.assertNotIn("input", kwargs)
                self.assertNotIn("--header", args[0])

    def test_api_credential_rejects_header_injection(self):
        with mock.patch.dict(checks.os.environ, {"GITHUB_TOKEN": "bad\nheader"}), \
                mock.patch.object(checks.subprocess, "check_output") as fetch:
            with self.assertRaisesRegex(ValueError, "Invalid GitHub API credential"):
                checks.fetch_bytes("https://api.github.com/test")
            fetch.assert_not_called()

    def evaluate(self):
        return checks.evaluate_audit(self.report, POLICY, self.graphs, TODAY)

    def test_only_exact_reviewed_notices_pass_and_remain_visible(self):
        self.assertEqual(len(self.evaluate()), 2)
        self.assertIn("RUSTSEC-2026-0197", self.evaluate()[1])

    def test_new_vulnerability_fails_even_when_lock_only(self):
        finding = copy.deepcopy(self.report["warnings"]["unsound"][0])
        finding["advisory"]["id"] = "RUSTSEC-2099-0001"
        finding["package"]["name"] = "non-macos-dependency"
        self.report["vulnerabilities"] = {"count": 1, "list": [finding]}
        with self.assertRaisesRegex(ValueError, "lock-only"):
            self.evaluate()

    def test_unknown_notice_is_not_silenced(self):
        self.report["warnings"]["notice"] = [{"package": {"name": "new", "version": "1"}}]
        with self.assertRaisesRegex(ValueError, "Unreviewed"):
            self.evaluate()

    def test_expired_exception_fails(self):
        with self.assertRaisesRegex(ValueError, "Expired"):
            checks.evaluate_audit(self.report, POLICY, self.graphs, date(2027, 1, 16))

    def test_new_package_version_invalidates_exception(self):
        self.report["warnings"]["unsound"][0]["package"]["version"] = "0.19.0"
        with self.assertRaisesRegex(ValueError, "version mismatch"):
            self.evaluate()

    def test_new_fix_requires_removal_of_exception(self):
        self.report["warnings"]["unsound"][0]["versions"]["patched"] = [">=0.18.1"]
        with self.assertRaisesRegex(ValueError, "fix is now available"):
            self.evaluate()

    def test_changed_affected_apis_fail(self):
        self.report["warnings"]["unsound"][0]["affected"]["functions"]["cgmath::new_api"] = []
        with self.assertRaisesRegex(ValueError, "affected APIs changed"):
            self.evaluate()

    def test_global_ignores_and_platform_filters_fail(self):
        for key in ("ignore", "target_os", "target_arch"):
            with self.subTest(key=key):
                self.report["settings"][key] = ["anything"]
                with self.assertRaises(ValueError):
                    self.evaluate()
                self.report["settings"][key] = []

    def test_unused_exception_fails(self):
        self.report["warnings"]["unsound"] = []
        with self.assertRaisesRegex(ValueError, "Stale"):
            self.evaluate()

    def test_severity_and_informational_filters_fail(self):
        self.report["settings"]["severity"] = "high"
        with self.assertRaisesRegex(ValueError, "severity filtering"):
            self.evaluate()
        self.report["settings"]["severity"] = None
        self.report["settings"]["informational_warnings"] = []
        with self.assertRaisesRegex(ValueError, "informational"):
            self.evaluate()


class SourceAndProvenanceTests(unittest.TestCase):
    def test_local_backend_lock_is_explicit_and_version_changes_need_review(self):
        lock = tomllib.loads((ROOT / "Cargo.lock").read_text())
        checks.check_lock(lock)
        backend = next(package for package in lock["package"] if package["name"] == "wayland-backend")
        self.assertNotIn("source", backend)
        self.assertEqual(backend["version"], "0.3.11")
        backend["version"] = "0.3.12"
        with self.assertRaisesRegex(ValueError, "Re-review"):
            checks.check_lock(lock)

    def test_workflow_references_are_exact_verified_shas(self):
        for path in (ROOT / ".github/workflows").glob("*.yml"):
            checks.check_actions(path.read_text(), POLICY)
        for reference in ("actions/checkout@v4", "actions/checkout@" + "a" * 40,
                          "unknown/action@" + "a" * 40):
            with self.assertRaises(ValueError):
                checks.check_actions(f"- uses: {reference}\n", POLICY)

    def test_inline_or_quoted_action_keys_cannot_bypass_guard(self):
        action = POLICY["actions"][0]
        prefix = f"- uses: {action['repository']}@{action['sha']}\n"
        for line in ('- { uses: unknown/action@v1 }', '- "uses": unknown/action@v1'):
            with self.assertRaises(ValueError):
                checks.check_actions(prefix + line, POLICY)

    def test_api_guard_rejects_calls_and_ufcs_and_mentions(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "new.rs"
            for text in ("matrix.swap_columns(0, 0);", "Matrix::swap_columns(&mut m, 0, 0);",
                         "use Thing::swap_columns as alias;", "// swap_columns"):
                path.write_text(text)
                with self.assertRaises(ValueError):
                    checks.check_api_sources([path], ["swap_columns"])
            path.write_text("matrix.swap_rows(0, 1);")
            checks.check_api_sources([path], ["swap_columns"])

    def test_inventory_keeps_tracked_readme_and_ignores_only_untracked_docs(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            subprocess.run(["git", "init", "-q", directory], check=True)
            (root / ".gitignore").write_text("docs/\n*.md\n")
            (root / "README.md").write_text("tracked")
            (root / "docs").mkdir()
            (root / "docs/local.md").write_text("ignored")
            subprocess.run(["git", "-C", directory, "add", "-f", "README.md"], check=True)
            (root / "new.rs").write_text("// untracked source")
            files = checks.repository_files(root)
            self.assertIn(root / "README.md", files)
            self.assertIn(root / "new.rs", files)
            self.assertNotIn(root / "docs/local.md", files)

    def test_formula_drift_and_fabricated_checksum_fail(self):
        text = (ROOT / "Formula/cocoa-way.rb").read_text()
        checks.check_formula(text, POLICY, POLICY["workspace_version"])
        with self.assertRaises(ValueError):
            checks.check_formula(text, POLICY, "2.2.0")
        with self.assertRaises(ValueError):
            checks.check_formula(text.replace(POLICY["formula"]["sha256"], "0" * 64), POLICY, POLICY["workspace_version"])

    def test_formula_can_lag_a_reviewed_candidate_but_never_lead_it(self):
        text = (ROOT / "Formula/cocoa-way.rb").read_text()
        policy = copy.deepcopy(POLICY)
        policy["workspace_version"] = policy["formula"]["version"]
        checks.check_formula(text, policy, policy["workspace_version"])
        policy["workspace_version"] = "2.0.9"
        with self.assertRaisesRegex(ValueError, "newer"):
            checks.check_formula(text, policy, "2.0.9")
        policy["workspace_version"] = "2.1.1-rc.1"
        with self.assertRaisesRegex(ValueError, "Invalid stable"):
            checks.check_formula(text, policy, "2.1.1-rc.1")

    def test_graph_excludes_disconnected_non_macos_nodes_and_checks_consumers(self):
        root = str(ROOT / "Cargo.toml")
        metadata = {
            "workspace_members": ["app"],
            "packages": [
                {"id": "app", "name": "cocoa-way", "version": "2.1.0", "source": None, "manifest_path": root},
                {"id": "smithay", "name": "smithay", "version": "0.7.0", "source": None,
                 "manifest_path": str(ROOT / "vendor/smithay/Cargo.toml")},
                {"id": "cgmath", "name": "cgmath", "version": "0.18.0", "source": checks.REGISTRY},
                {"id": "other", "name": "other-platform", "version": "1", "source": checks.REGISTRY},
            ],
            "resolve": {"nodes": [
                {"id": "app", "dependencies": ["smithay"]},
                {"id": "smithay", "dependencies": ["cgmath"]},
                {"id": "cgmath", "dependencies": []},
                {"id": "other", "dependencies": []},
            ]},
        }
        graph = checks.check_graph(metadata, POLICY, [])
        self.assertNotIn(("other-platform", "1"), graph)
        backend = {"id": "backend", "name": "wayland-backend", "version": "0.3.11", "source": None,
                   "manifest_path": str(ROOT / "vendor/wayland-backend/Cargo.toml")}
        metadata["packages"].append(backend)
        metadata["resolve"]["nodes"].append({"id": "backend", "dependencies": []})
        metadata["resolve"]["nodes"][0]["dependencies"].append("backend")
        self.assertIn(("wayland-backend", "0.3.11"), checks.check_graph(metadata, POLICY, []))
        backend["version"] = "0.3.12"
        with self.assertRaisesRegex(ValueError, "Re-review"):
            checks.check_graph(metadata, POLICY, [])
        backend["version"] = "0.3.11"
        backend["manifest_path"] = str(ROOT / "unreviewed/wayland-backend/Cargo.toml")
        with self.assertRaisesRegex(ValueError, "Unapproved local"):
            checks.check_graph(metadata, POLICY, [])
        backend["manifest_path"] = str(ROOT / "vendor/wayland-backend/Cargo.toml")
        metadata["resolve"]["nodes"][0]["dependencies"].append("cgmath")
        with self.assertRaisesRegex(ValueError, "consumers changed"):
            checks.check_graph(metadata, POLICY, [])


class BundleTests(unittest.TestCase):
    def test_every_packaged_helper_is_rewritten_and_signed(self):
        script = (ROOT / "scripts/bundle-macos-app.sh").read_text()
        helpers = re.search(r"^for helper in (.+); do$", script, re.MULTILINE)
        self.assertIsNotNone(helpers)
        self.assertEqual(set(helpers.group(1).split()), set(bundle.EXECUTABLES[1:]))

    def test_bare_loader_and_executable_rpaths_are_valid(self):
        binary = Path("/Example.app/Contents/Frameworks/libxkbcommon.dylib")
        executable = Path("/Example.app/Contents/MacOS/cocoa-way")
        self.assertEqual(bundle.expand_path("@loader_path", binary, executable), binary.parent)
        self.assertEqual(bundle.expand_path("@executable_path", binary, executable), executable.parent)

    def test_load_command_parsing(self):
        commands = """Load command 1
          cmd LC_RPATH
      cmdsize 48
         path @executable_path/../Frameworks (offset 12)
Load command 2
          cmd LC_BUILD_VERSION
      cmdsize 32
     platform 1
        minos 14.0
          sdk 15.0
"""
        self.assertEqual(bundle.rpaths(commands), ["@executable_path/../Frameworks"])
        self.assertEqual(bundle.minimum_versions(commands), ["14.0"])
        self.assertEqual(bundle.version_tuple("14.1"), (14, 1, 0))

    def test_transitive_external_missing_pixman_and_escaping_symlinks_fail(self):
        with tempfile.TemporaryDirectory() as directory:
            app = Path(directory) / "Example.app"
            frameworks = app / "Contents/Frameworks"
            frameworks.mkdir(parents=True)
            executable = app / "Contents/MacOS/cocoa-way"
            binary = frameworks / "libfirst.dylib"
            library = frameworks / "libsecond.dylib"
            library.write_bytes(b"fixture")
            args = (binary, executable, [frameworks], app)
            self.assertEqual(bundle.resolve_dependency("@rpath/libsecond.dylib", *args), library.resolve())
            self.assertIsNone(bundle.resolve_dependency("/usr/lib/libSystem.B.dylib", *args))
            for name in ("/opt/homebrew/lib/libsecond.dylib", "@rpath/missing.dylib",
                         "@rpath/libpixman-1.dylib", "@rpath/../../../outside",
                         "/usr/lib/../../opt/homebrew/lib/bad.dylib"):
                with self.subTest(name=name), self.assertRaises(ValueError):
                    bundle.resolve_dependency(name, *args)
            outside = Path(directory) / "outside"
            outside.write_bytes(b"outside")
            library.unlink()
            library.symlink_to(outside)
            with self.assertRaises(ValueError):
                bundle.resolve_dependency("@rpath/libsecond.dylib", *args)


if __name__ == "__main__":
    unittest.main(verbosity=2)
