#!/usr/bin/env python3
"""Tests for `libra_jira_provider.py` (HORO-1173). No real Jira API call
is made anywhere in this file.
"""

from __future__ import annotations

import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "common"))

import libra_jira_provider as jp  # noqa: E402


class BusinessContextFromIssueTest(unittest.TestCase):
    def test_maps_known_priority_to_the_closed_wire_enum(self):
        issue = {"self": "https://x.atlassian.net/rest/api/3/issue/10001", "fields": {"priority": {"name": "High"}}}
        response = jp.business_context_from_issue(issue)
        self.assertEqual(response["priority"], "high")

    def test_unrecognised_priority_name_is_left_absent_not_guessed(self):
        issue = {"fields": {"priority": {"name": "P0-Custom-Scheme"}}}
        response = jp.business_context_from_issue(issue)
        self.assertNotIn("priority", response)

    def test_duedate_becomes_end_of_day_utc_rfc3339(self):
        issue = {"fields": {"duedate": "2026-12-25"}}
        response = jp.business_context_from_issue(issue)
        self.assertEqual(response["deadline"], "2026-12-25T23:59:59Z")

    def test_no_duedate_means_no_deadline_field(self):
        issue = {"fields": {}}
        response = jp.business_context_from_issue(issue)
        self.assertNotIn("deadline", response)

    def test_project_key_becomes_cost_center(self):
        issue = {"fields": {"project": {"key": "ENGPLAT"}}}
        response = jp.business_context_from_issue(issue)
        self.assertEqual(response["cost_center"], "ENGPLAT")

    def test_advisory_criteria_is_always_empty(self):
        # This adapter never parses issue text into completion criteria —
        # see module docstring's R2 reference.
        issue = {"fields": {}}
        response = jp.business_context_from_issue(issue)
        self.assertEqual(response["advisory_criteria"], [])

    def test_external_ref_kind_matches_the_closed_wire_enum(self):
        issue = {"self": "https://x.atlassian.net/rest/api/3/issue/10001", "fields": {}}
        response = jp.business_context_from_issue(issue)
        self.assertEqual(response["external_refs"], [{"kind": "jira", "value": issue["self"]}])

    def test_schema_version_and_provider_id_always_present(self):
        response = jp.business_context_from_issue({"fields": {}})
        self.assertEqual(response["schema_version"], "libra.extension.v1")
        self.assertEqual(response["provider_id"], "jira")


class IsDoneTest(unittest.TestCase):
    def test_done_status_is_done(self):
        issue = {"fields": {"status": {"name": "Done"}}}
        self.assertTrue(jp.is_done(issue, jp.DEFAULT_DONE_STATUSES))

    def test_in_progress_status_is_not_done(self):
        issue = {"fields": {"status": {"name": "In Progress"}}}
        self.assertFalse(jp.is_done(issue, jp.DEFAULT_DONE_STATUSES))

    def test_missing_status_is_not_done(self):
        self.assertFalse(jp.is_done({"fields": {}}, jp.DEFAULT_DONE_STATUSES))


class ResolveTicketKeyTest(unittest.TestCase):
    def test_cwd_must_match_workspace_root_exactly(self):
        self.assertIsNone(jp.resolve_ticket_key("/some/other/path", "/tmp"))

    def test_nonexistent_workspace_root_resolves_to_none(self):
        self.assertIsNone(jp.resolve_ticket_key("/definitely/does/not/exist", "/definitely/does/not/exist"))

    def test_resolves_a_real_ticket_key_from_the_current_branch(self):
        # Exercises the real `git` subprocess end to end, but against a
        # disposable temp repo with a known branch name created here —
        # not against this worktree's own branch, which is a detached
        # HEAD under `actions/checkout`'s `pull_request` event in CI
        # (`rev-parse --abbrev-ref HEAD` would return `HEAD`, not a
        # ticket key, and the real-branch form of this test would then
        # fail in CI while passing locally).
        with tempfile.TemporaryDirectory() as repo_root:
            env = {
                **os.environ,
                "GIT_AUTHOR_NAME": "test",
                "GIT_AUTHOR_EMAIL": "test@example.invalid",
                "GIT_COMMITTER_NAME": "test",
                "GIT_COMMITTER_EMAIL": "test@example.invalid",
            }
            subprocess.run(
                ["git", "init", "-q", "-b", "v0.0.3/HORO-1173/feat/example"], cwd=repo_root, check=True, env=env
            )
            subprocess.run(
                ["git", "-C", repo_root, "commit", "--allow-empty", "-q", "-m", "init"], check=True, env=env
            )
            key = jp.resolve_ticket_key(repo_root, repo_root)
        self.assertEqual(key, "HORO-1173")


class PushOutcomeTest(unittest.TestCase):
    @patch("subprocess.run")
    def test_builds_a_completed_outcome_payload(self, mock_run):
        mock_run.return_value = subprocess.CompletedProcess(args=[], returncode=0, stdout="", stderr="")
        jp.push_outcome("libra-governor", "task-123", "HORO-1", "https://x.atlassian.net/browse/HORO-1")
        call_args = mock_run.call_args
        self.assertEqual(call_args.args[0], ["libra-governor", "outcome", "record"])
        self.assertIn('"task_id": "task-123"', call_args.kwargs["input"])
        self.assertIn('"source_id": "jira"', call_args.kwargs["input"])
        self.assertIn('"kind": "completed"', call_args.kwargs["input"])


class JiraPriorityMappingTest(unittest.TestCase):
    def test_every_default_jira_priority_maps_to_a_member_of_the_closed_enum(self):
        wire_enum = {"low", "normal", "high", "urgent"}
        for wire_value in jp.JIRA_PRIORITY_TO_WIRE_PRIORITY.values():
            self.assertIn(wire_value, wire_enum)


if __name__ == "__main__":
    unittest.main()
