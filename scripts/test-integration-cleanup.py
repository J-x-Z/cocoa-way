#!/usr/bin/env python3
"""Only cleanup authority/race tests; never contacts a runtime."""

import importlib.util
import json
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location(
    "integration", Path(__file__).with_name("audit-integration.py"))
integration = importlib.util.module_from_spec(spec)
spec.loader.exec_module(integration)


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
