#!/usr/bin/env python3
"""Test integration storage and cleanup guards without contacting a runtime."""

import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location(
    "integration", Path(__file__).with_name("audit-integration.py"))
integration = importlib.util.module_from_spec(spec)
spec.loader.exec_module(integration)


class AppRootTests(unittest.TestCase):
    def verify(self, expected, payload):
        def run(argv):
            self.assertEqual(argv, ["container", "system", "status", "--format", "json"])
            return json.dumps(payload)

        return integration.verify_app_root("container", expected, run)

    def test_accepts_the_live_root_with_a_trailing_slash(self):
        with tempfile.TemporaryDirectory() as root:
            expected = Path(root).resolve()
            self.assertEqual(self.verify(expected, {
                "status": "running", "paths": {"appRoot": f"{root}/"},
            }), str(expected))

    def test_rejects_a_live_service_using_a_different_root(self):
        with tempfile.TemporaryDirectory() as root, tempfile.TemporaryDirectory() as other:
            with self.assertRaisesRegex(RuntimeError, "expected"):
                self.verify(Path(root).resolve(), {
                    "status": "running", "paths": {"appRoot": other},
                })

    def test_refuses_missing_relative_or_non_running_roots(self):
        for payload in (
            [],
            {"status": "unregistered"},
            {"status": "running", "paths": {}},
            {"status": "running", "paths": {"appRoot": "relative"}},
        ):
            with self.subTest(payload=payload), self.assertRaisesRegex(RuntimeError, "did not report"):
                self.verify(Path("/unused"), payload)


class CleanupTests(unittest.TestCase):
    def scenario(self, current, deletion_fails=True):
        calls = []
        owned = {"configuration": {"id": "owned", "labels": {"audit": "token"}}}
        foreign = {"configuration": {"id": "foreign", "labels": {"audit": "other"}}}

        def run(argv, **kwargs):
            calls.append(argv)
            if argv[1] == "delete":
                self.assertEqual(argv[-1], "owned")
                if deletion_fails:
                    raise RuntimeError("delete failed")
                return ""
            if len(calls) == 1:
                return json.dumps([foreign, owned])
            return json.dumps(current)

        return run, calls

    def test_app_winning_cleanup_is_confirmed_by_fresh_inventory(self):
        run, calls = self.scenario([])
        self.assertEqual(integration.cleanup_owned_containers("container", "audit", "token", run), ["owned"])
        self.assertEqual(len(calls), 3)

    def test_persistent_container_does_not_hide_delete_failure(self):
        run, _ = self.scenario([{"configuration": {"id": "owned"}}])
        with self.assertRaisesRegex(RuntimeError, "delete failed"):
            integration.cleanup_owned_containers("container", "audit", "token", run)

    def test_same_name_with_new_owner_is_not_retried_or_accepted_as_absent(self):
        run, calls = self.scenario([{"configuration": {"id": "owned", "labels": {"audit": "other"}}}])
        with self.assertRaises(RuntimeError):
            integration.cleanup_owned_containers("container", "audit", "token", run)
        self.assertEqual(sum(argv[1] == "delete" for argv in calls), 1)

    def test_successful_delete_never_targets_foreign_container(self):
        run, calls = self.scenario([], deletion_fails=False)
        self.assertEqual(integration.cleanup_owned_containers("container", "audit", "token", run), [])
        self.assertEqual(len(calls), 2)

    def test_malformed_refresh_does_not_count_as_confirmed_absence(self):
        for payload in ({}, [None], [{"configuration": {}}]):
            run, _ = self.scenario(payload)
            with self.subTest(payload=payload), self.assertRaisesRegex(RuntimeError, "schema"):
                integration.cleanup_owned_containers("container", "audit", "token", run)


if __name__ == "__main__":
    unittest.main()
