#!/usr/bin/env python3
"""A first-class GitHub Business Context / Outcome Provider for Libra
Governor (HORO-1173), built on the same HMAC-verified loopback runtime as
`providers/jira/libra_jira_provider.py`.

This adapter resolves the real pull request associated with a task's
current branch (via the real `owner/repo` read from the workspace's git
remote, and the real branch name — both from `--workspace-root`, an
operator-supplied CLI argument, never from request-supplied `cwd`, which
is used only as an equality selector, mirroring
`libra_jira_provider.py`/`libra_example_provider.py`'s already-reviewed
pattern) and calls the real GitHub REST API to populate business context
and outcome evidence:

- `BusinessContextSummary.cost_center` <- the PR's base repository's
  `full_name` (a safe, short label);
- outcome evidence <- the PR's `merged`/`state` and its combined check-run
  status, pushed as an `AttestationSource::Provider { provider_id:
  "github" }` attestation via `libra-governor outcome record`, the same
  mechanism `report_outcome.sh` and `libra_jira_provider.py` both use.
  This is read-only against GitHub: this adapter never mutates a PR,
  issue, or check run. "GitHub PR/CI/merge evidence can attach to the
  same TaskIdentity" (AC) is a read, not a write, so there is no
  `--enable-write-back` flag here — see `libra_jira_provider.py` for the
  one adapter in this ticket that does mutate its external system.

OAuth/token scope required: a GitHub fine-grained personal access token
(or GitHub App installation token) needs only `pull_requests: read` and
`checks: read` on the target repository. No `contents`, `issues: write`,
or any organization-level scope is required for this adapter's read-only
behavior.

Stdlib only. Every real GitHub call goes through
`providers/common/safe_https_client.py`'s `fetch_json`, which accepts an
injectable opener — this module's own tests never make a real network
call.
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import sys
import threading
import urllib.parse
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "common"))

from libra_provider_runtime import (  # noqa: E402
    BaseProviderHandler,
    ReplayGuard,
    require_readable_file,
    run_server,
)
from safe_https_client import fetch_json, validate_https_base_url  # noqa: E402

SCHEMA_VERSION = "libra.extension.v1"
PROVIDER_ID = "github"
DEFAULT_GITHUB_API_BASE_URL = "https://api.github.com"
# Matches `git@github.com:owner/repo.git` and `https://github.com/owner/repo.git`
# remote URL shapes. Applied only to a URL `git remote get-url origin`
# itself returned for `workspace_root` — never to request-supplied text.
REMOTE_URL_PATTERN = re.compile(r"github\.com[:/]([^/]+)/([^/.]+?)(?:\.git)?$")


def resolve_repo_and_branch(workspace_root: str) -> tuple[str, str, str] | None:
    """Returns `(owner, repo, branch)` for the real git repository at
    `workspace_root`, or `None` if it is not a GitHub-hosted git
    repository or has no current branch. `workspace_root` is always the
    operator-supplied CLI argument — see module docstring."""
    real_root = os.path.realpath(workspace_root)
    if not os.path.isdir(real_root):
        return None
    try:
        remote = subprocess.run(
            ["git", "-C", real_root, "remote", "get-url", "origin"],
            capture_output=True,
            text=True,
            timeout=5,
        )
        branch = subprocess.run(
            ["git", "-C", real_root, "rev-parse", "--abbrev-ref", "HEAD"],
            capture_output=True,
            text=True,
            timeout=5,
        )
    except (OSError, subprocess.TimeoutExpired, ValueError):
        return None
    if remote.returncode != 0 or branch.returncode != 0:
        return None
    match = REMOTE_URL_PATTERN.search(remote.stdout.strip())
    if not match:
        return None
    return match.group(1), match.group(2), branch.stdout.strip()


def fetch_pull_request_for_branch(
    api_base_url: str,
    owner: str,
    repo: str,
    branch: str,
    auth_header: str,
    *,
    opener=None,
    allow_private_network: bool = False,
) -> dict | None:
    """Fetches the open (or most recently updated) pull request whose
    head is `owner:branch`, or `None` if there is none."""
    head_param = urllib.parse.quote(f"{owner}:{branch}", safe="")
    url = f"{api_base_url.rstrip('/')}/repos/{owner}/{repo}/pulls?head={head_param}&state=all"
    results = fetch_json(
        url,
        headers={"Authorization": auth_header, "Accept": "application/vnd.github+json"},
        opener=opener,
        allow_private_network=allow_private_network,
    )
    if not results:
        return None
    return results[0]


def fetch_combined_check_status(
    api_base_url: str,
    owner: str,
    repo: str,
    sha: str,
    auth_header: str,
    *,
    opener=None,
    allow_private_network: bool = False,
) -> dict:
    sha_param = urllib.parse.quote(sha, safe="")
    url = f"{api_base_url.rstrip('/')}/repos/{owner}/{repo}/commits/{sha_param}/status"
    return fetch_json(
        url,
        headers={"Authorization": auth_header, "Accept": "application/vnd.github+json"},
        opener=opener,
        allow_private_network=allow_private_network,
    )


def business_context_from_pull_request(pull_request: dict, provider_id: str = PROVIDER_ID) -> dict:
    """Maps a real GitHub PR response into
    `docs/api/libra-extension-v1.yaml`'s `BusinessContextResponse` shape.
    GitHub PRs have no due date/priority field, so this adapter populates
    only what genuinely exists: the base repository as a cost-center-like
    label and the PR's own URL as an external reference."""
    response: dict = {"schema_version": SCHEMA_VERSION, "provider_id": provider_id}
    base_repo = pull_request.get("base", {}).get("repo", {})
    if base_repo.get("full_name"):
        response["cost_center"] = base_repo["full_name"]
    html_url = pull_request.get("html_url")
    response["advisory_criteria"] = []
    response["external_refs"] = [{"kind": "github", "value": html_url}] if html_url else []
    return response


def outcome_from_pull_request_and_checks(pull_request: dict, combined_status: dict) -> dict | None:
    """Returns an `ExecutionOutcome`-shaped dict (see
    `crates/domain/src/execution_outcome.rs`) if the PR's state plus its
    combined check status together provide real, objective completion
    evidence — never from the PR's title/body text, which could contain
    an agent's own unverified claim of success."""
    if pull_request.get("merged"):
        return {"kind": "completed", "evidence": [pull_request.get("html_url", "")]}
    if pull_request.get("state") == "closed" and not pull_request.get("merged"):
        return {"kind": "aborted", "evidence": [pull_request.get("html_url", "")]}
    if combined_status.get("state") == "failure":
        return {"kind": "failed", "evidence": [combined_status.get("commit_url", "")]}
    return None


def push_outcome(binary: str, task_id: str, pr_url: str, outcome: dict) -> subprocess.CompletedProcess:
    """Records a Provider-sourced outcome attestation via the real
    `libra-governor outcome record` CLI path (HORO-1174)."""
    payload = json.dumps(
        {
            "task_id": task_id,
            "source_id": PROVIDER_ID,
            "idempotency_key": f"github-{pr_url}-{outcome['kind']}",
            "outcome": outcome,
        }
    )
    return subprocess.run([binary, "outcome", "record"], input=payload, capture_output=True, text=True, timeout=15)


class TaskRepoContext:
    """Remembers which `(owner, repo, branch)` a task resolved to during
    its business-context fetch, so a later, independent `/libra/events`
    call for the same task can find its way back to the same PR — the
    two surfaces are separate HTTP calls with no shared session, the same
    cross-call state problem `libra_jira_provider.py`'s `TaskIssueKeys`
    already solves."""

    def __init__(self):
        self._by_task: dict[str, tuple[str, str, str]] = {}
        self._lock = threading.Lock()

    def remember(self, task_id: str, owner: str, repo: str, branch: str) -> None:
        with self._lock:
            self._by_task[task_id] = (owner, repo, branch)

    def lookup(self, task_id: str) -> tuple[str, str, str] | None:
        with self._lock:
            return self._by_task.get(task_id)


class GitHubProviderHandler(BaseProviderHandler):
    api_base_url: str
    auth_header: str
    workspace_root: str
    outcome_cli_binary: str
    task_repo_context: TaskRepoContext
    allow_private_network: bool

    def route_table(self):
        return {
            "/libra/business-context": self._handle_business_context,
            "/libra/events": self._handle_event,
        }

    def _handle_business_context(self, request: dict) -> None:
        task_id = request.get("task_id", "")
        cwd = request.get("cwd", "")
        minimal = {"schema_version": SCHEMA_VERSION, "provider_id": PROVIDER_ID}

        if not cwd or os.path.realpath(cwd) != os.path.realpath(self.workspace_root):
            self.log_message("business-context: task=%s cwd does not match workspace_root", task_id)
            self._respond_json(200, minimal)
            return

        resolved = resolve_repo_and_branch(self.workspace_root)
        if resolved is None:
            self.log_message("business-context: task=%s not a resolvable GitHub repo/branch", task_id)
            self._respond_json(200, minimal)
            return
        owner, repo, branch = resolved
        if task_id:
            self.task_repo_context.remember(task_id, owner, repo, branch)

        try:
            pull_request = fetch_pull_request_for_branch(
                self.api_base_url,
                owner,
                repo,
                branch,
                self.auth_header,
                allow_private_network=self.allow_private_network,
            )
        except Exception as exc:  # noqa: BLE001 — any fetch failure degrades to minimal response, never raises
            self.log_message("business-context: task=%s %s/%s#%s fetch failed: %s", task_id, owner, repo, branch, exc)
            self._respond_json(200, minimal)
            return

        if pull_request is None:
            self.log_message("business-context: task=%s %s/%s#%s has no open PR", task_id, owner, repo, branch)
            self._respond_json(200, minimal)
            return

        response = business_context_from_pull_request(pull_request)
        self.log_message("business-context: task=%s -> %s", task_id, response.get("cost_center"))
        self._respond_json(200, response)

    def _handle_event(self, request: dict) -> None:
        # Always accept the delivery — an outcome-push failure must never
        # cause the dispatcher to treat this as a rejected event.
        self._respond_json(200, {"status": "accepted"})

        task_id = request.get("data", {}).get("task_id", "") or request.get("task_id", "")
        if not task_id:
            return
        resolved = self.task_repo_context.lookup(task_id)
        if resolved is None:
            self.log_message("outcome: task=%s has no known repo/branch, skipping", task_id)
            return
        owner, repo, branch = resolved

        try:
            pull_request = fetch_pull_request_for_branch(
                self.api_base_url,
                owner,
                repo,
                branch,
                self.auth_header,
                allow_private_network=self.allow_private_network,
            )
            if pull_request is None:
                self.log_message("outcome: task=%s %s/%s#%s has no PR, skipping", task_id, owner, repo, branch)
                return
            combined_status = fetch_combined_check_status(
                self.api_base_url,
                owner,
                repo,
                pull_request["head"]["sha"],
                self.auth_header,
                allow_private_network=self.allow_private_network,
            )
        except Exception as exc:  # noqa: BLE001 — a fetch failure must never raise into the handler
            self.log_message("outcome: task=%s %s/%s#%s fetch failed: %s", task_id, owner, repo, branch, exc)
            return

        outcome = outcome_from_pull_request_and_checks(pull_request, combined_status)
        if outcome is None:
            self.log_message("outcome: task=%s %s/%s#%s has no determinable outcome yet", task_id, owner, repo, branch)
            return

        result = push_outcome(self.outcome_cli_binary, task_id, pull_request.get("html_url", ""), outcome)
        self.log_message("outcome: task=%s -> kind=%s rc=%s", task_id, outcome["kind"], result.returncode)


def main() -> None:
    import argparse

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8789)
    parser.add_argument("--secret-file", required=True)
    parser.add_argument("--github-token-file", required=True)
    parser.add_argument("--github-api-base-url", default=DEFAULT_GITHUB_API_BASE_URL, help="For GitHub Enterprise.")
    parser.add_argument("--allow-private-network", action="store_true", help="For a self-hosted GHE instance only.")
    parser.add_argument("--max-clock-skew-secs", type=float, default=120.0)
    parser.add_argument("--workspace-root", default=os.getcwd())
    parser.add_argument(
        "--outcome-cli-binary",
        default="libra-governor",
        help="Binary used to record outcome attestations (HORO-1174's `outcome record` CLI path).",
    )
    args = parser.parse_args()

    api_base_url = validate_https_base_url(args.github_api_base_url, allow_private_network=args.allow_private_network)
    secret_bytes = require_readable_file(args.secret_file, "secret-file", program_name="libra_github_provider").read_bytes()
    token = require_readable_file(
        args.github_token_file, "github-token-file", program_name="libra_github_provider"
    ).read_text().strip()

    GitHubProviderHandler.secret = secret_bytes
    GitHubProviderHandler.replay_guard = ReplayGuard(skew_secs=args.max_clock_skew_secs)
    GitHubProviderHandler.max_clock_skew_secs = args.max_clock_skew_secs
    GitHubProviderHandler.provider_id = PROVIDER_ID
    GitHubProviderHandler.api_base_url = api_base_url
    GitHubProviderHandler.auth_header = f"Bearer {token}"
    GitHubProviderHandler.workspace_root = os.path.realpath(args.workspace_root)
    GitHubProviderHandler.outcome_cli_binary = args.outcome_cli_binary
    GitHubProviderHandler.task_repo_context = TaskRepoContext()
    GitHubProviderHandler.allow_private_network = args.allow_private_network

    run_server(GitHubProviderHandler, args.host, args.port, program_name="libra_github_provider")


if __name__ == "__main__":
    main()
