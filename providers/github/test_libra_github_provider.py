#!/usr/bin/env python3
"""Tests for `libra_github_provider.py` (HORO-1173). No real network or git
remote call is made anywhere in this file."""

from __future__ import annotations

import subprocess
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "common"))

import libra_github_provider as gp  # noqa: E402


def _completed(stdout: str = "", returncode: int = 0) -> subprocess.CompletedProcess:
    return subprocess.CompletedProcess(args=[], returncode=returncode, stdout=stdout, stderr="")


class ResolveRepoAndBranchTest(unittest.TestCase):
    def test_nonexistent_workspace_root_resolves_to_none(self):
        self.assertIsNone(gp.resolve_repo_and_branch("/definitely/does/not/exist"))

    @patch("subprocess.run")
    def test_https_remote_url_is_parsed_into_owner_and_repo(self, mock_run):
        mock_run.side_effect = [
            _completed(stdout="https://github.com/horonomy/libra-governor.git\n"),
            _completed(stdout="v0.0.3/HORO-1173/feat/business_context_outcome_adapters\n"),
        ]
        with patch("os.path.isdir", return_value=True):
            result = gp.resolve_repo_and_branch("/some/repo")
        self.assertEqual(result, ("horonomy", "libra-governor", "v0.0.3/HORO-1173/feat/business_context_outcome_adapters"))

    @patch("subprocess.run")
    def test_ssh_remote_url_is_parsed_into_owner_and_repo(self, mock_run):
        mock_run.side_effect = [
            _completed(stdout="git@github.com:horonomy/libra-governor.git\n"),
            _completed(stdout="main\n"),
        ]
        with patch("os.path.isdir", return_value=True):
            result = gp.resolve_repo_and_branch("/some/repo")
        self.assertEqual(result, ("horonomy", "libra-governor", "main"))

    @patch("subprocess.run")
    def test_non_github_remote_resolves_to_none(self, mock_run):
        mock_run.side_effect = [
            _completed(stdout="https://gitlab.com/horonomy/libra-governor.git\n"),
            _completed(stdout="main\n"),
        ]
        with patch("os.path.isdir", return_value=True):
            self.assertIsNone(gp.resolve_repo_and_branch("/some/repo"))

    @patch("subprocess.run")
    def test_git_failure_resolves_to_none_rather_than_raising(self, mock_run):
        mock_run.side_effect = [_completed(returncode=128), _completed(returncode=0)]
        with patch("os.path.isdir", return_value=True):
            self.assertIsNone(gp.resolve_repo_and_branch("/some/repo"))


class BusinessContextFromPullRequestTest(unittest.TestCase):
    def test_base_repo_full_name_becomes_cost_center(self):
        pr = {"base": {"repo": {"full_name": "horonomy/libra-governor"}}, "html_url": "https://github.com/horonomy/libra-governor/pull/42"}
        response = gp.business_context_from_pull_request(pr)
        self.assertEqual(response["cost_center"], "horonomy/libra-governor")

    def test_external_ref_kind_matches_the_closed_wire_enum(self):
        pr = {"base": {"repo": {}}, "html_url": "https://github.com/horonomy/libra-governor/pull/42"}
        response = gp.business_context_from_pull_request(pr)
        self.assertEqual(response["external_refs"], [{"kind": "github", "value": pr["html_url"]}])

    def test_missing_html_url_yields_no_external_refs(self):
        response = gp.business_context_from_pull_request({"base": {"repo": {}}})
        self.assertEqual(response["external_refs"], [])

    def test_advisory_criteria_is_always_empty(self):
        response = gp.business_context_from_pull_request({"base": {"repo": {}}})
        self.assertEqual(response["advisory_criteria"], [])


class OutcomeFromPullRequestAndChecksTest(unittest.TestCase):
    def test_merged_pr_is_completed(self):
        pr = {"merged": True, "html_url": "https://github.com/x/y/pull/1"}
        outcome = gp.outcome_from_pull_request_and_checks(pr, {})
        self.assertEqual(outcome, {"kind": "completed", "evidence": [pr["html_url"]]})

    def test_closed_unmerged_pr_is_aborted(self):
        pr = {"merged": False, "state": "closed", "html_url": "https://github.com/x/y/pull/1"}
        outcome = gp.outcome_from_pull_request_and_checks(pr, {})
        self.assertEqual(outcome, {"kind": "aborted", "evidence": [pr["html_url"]]})

    def test_failing_combined_status_on_an_open_pr_is_failed(self):
        pr = {"merged": False, "state": "open"}
        status = {"state": "failure", "commit_url": "https://api.github.com/repos/x/y/commits/abc/status"}
        outcome = gp.outcome_from_pull_request_and_checks(pr, status)
        self.assertEqual(outcome, {"kind": "failed", "evidence": [status["commit_url"]]})

    def test_open_pr_with_pending_checks_has_no_outcome_yet(self):
        pr = {"merged": False, "state": "open"}
        status = {"state": "pending"}
        self.assertIsNone(gp.outcome_from_pull_request_and_checks(pr, status))

    def test_merged_takes_priority_over_a_failing_combined_status(self):
        # A PR merged despite a later-reported flaky check is still a real
        # completion — merge state is the authoritative signal here.
        pr = {"merged": True, "html_url": "https://github.com/x/y/pull/1"}
        status = {"state": "failure"}
        outcome = gp.outcome_from_pull_request_and_checks(pr, status)
        self.assertEqual(outcome["kind"], "completed")


class PushOutcomeTest(unittest.TestCase):
    @patch("subprocess.run")
    def test_builds_an_idempotency_key_from_pr_url_and_outcome_kind(self, mock_run):
        mock_run.return_value = _completed()
        outcome = {"kind": "completed", "evidence": ["https://github.com/x/y/pull/1"]}
        gp.push_outcome("libra-governor", "task-123", "https://github.com/x/y/pull/1", outcome)
        call_args = mock_run.call_args
        self.assertEqual(call_args.args[0], ["libra-governor", "outcome", "record"])
        self.assertIn('"source_id": "github"', call_args.kwargs["input"])
        self.assertIn("github-https://github.com/x/y/pull/1-completed", call_args.kwargs["input"])


class _FakeHandler:
    """A minimal stand-in for `GitHubProviderHandler` carrying only the
    attributes `_handle_event` actually reads — avoids standing up a real
    `BaseHTTPRequestHandler` (which needs a real socket) just to exercise
    this one route's business logic."""

    def __init__(self):
        self.api_base_url = gp.DEFAULT_GITHUB_API_BASE_URL
        self.auth_header = "Bearer test-token"
        self.allow_private_network = False
        self.outcome_cli_binary = "libra-governor"
        self.task_repo_context = gp.TaskRepoContext()
        self.responded: list[tuple[int, dict]] = []
        self.logged: list[str] = []

    def _respond_json(self, status, payload):
        self.responded.append((status, payload))

    def log_message(self, fmt, *args):
        self.logged.append(fmt % args)


class HandleEventTest(unittest.TestCase):
    def test_unknown_task_is_a_noop_after_accepting_the_delivery(self):
        handler = _FakeHandler()
        gp.GitHubProviderHandler._handle_event(handler, {"data": {"task_id": "unknown-task"}})
        self.assertEqual(handler.responded, [(200, {"status": "accepted"})])

    @patch("libra_github_provider.push_outcome")
    @patch("libra_github_provider.fetch_combined_check_status")
    @patch("libra_github_provider.fetch_pull_request_for_branch")
    def test_merged_pr_pushes_a_completed_outcome(self, mock_fetch_pr, mock_fetch_status, mock_push):
        handler = _FakeHandler()
        handler.task_repo_context.remember("task-123", "horonomy", "libra-governor", "v0.0.3/HORO-1173/feat/x")
        mock_fetch_pr.return_value = {
            "merged": True,
            "html_url": "https://github.com/horonomy/libra-governor/pull/42",
            "head": {"sha": "abc123"},
        }
        mock_fetch_status.return_value = {"state": "success"}
        mock_push.return_value = _completed()

        gp.GitHubProviderHandler._handle_event(handler, {"data": {"task_id": "task-123"}})

        mock_push.assert_called_once()
        pushed_outcome = mock_push.call_args.args[3]
        self.assertEqual(pushed_outcome["kind"], "completed")

    @patch("libra_github_provider.fetch_pull_request_for_branch")
    def test_no_pr_yet_skips_without_pushing_an_outcome(self, mock_fetch_pr):
        handler = _FakeHandler()
        handler.task_repo_context.remember("task-123", "horonomy", "libra-governor", "v0.0.3/HORO-1173/feat/x")
        mock_fetch_pr.return_value = None

        with patch("libra_github_provider.push_outcome") as mock_push:
            gp.GitHubProviderHandler._handle_event(handler, {"data": {"task_id": "task-123"}})
            mock_push.assert_not_called()


if __name__ == "__main__":
    unittest.main()
