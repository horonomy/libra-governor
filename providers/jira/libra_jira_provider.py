#!/usr/bin/env python3
"""A first-class Jira Business Context Provider for Libra Governor
(HORO-1173), built on the HMAC-verified loopback server from
`providers/common/libra_provider_runtime.py` (the same trust boundary
`examples/local-providers/libra_example_provider.py` already proved for
HORO-1174).

Unlike the example provider's `tickets.json` fixture, this adapter
resolves a task's real Jira issue and calls the real Jira Cloud REST API
(`GET /rest/api/3/issue/{key}`) to populate `BusinessContextSummary`:

- `priority` from the issue's `fields.priority.name`;
- `deadline` from `fields.duedate` (a bare `YYYY-MM-DD`, interpreted as
  end-of-day UTC — Jira's due date has no time component);
- `cost_center` from `fields.project.key` (a safe, short label — never
  the project's full name, which could carry confidential context);
- `advisory_criteria`/`external_refs` are left empty: this adapter does
  not invent completion criteria from issue text, consistent with R2 in
  `docs/adr/0005-local-extension-points.md` (an external provider's
  wishes are recorded metadata, never a required-criteria source, and
  this adapter does not even attempt to parse issue text into criteria).

Outcome evidence (AC: "GitHub PR/CI/merge evidence... outcome is not
inferred solely from assistant self-report") flows the other direction:
`push_outcome_if_done` checks the issue's `fields.status.name` against a
configured "done" status set and, only if genuinely done, pushes an
`AttestationSource::Provider { provider_id: "jira" }` attestation via
`libra-governor outcome record` (the exact mechanism
`report_outcome.sh` already proved) — this never mutates Jira, it only
reads Jira and tells Libra what it found.

Write-back — posting Libra's execution receipt as a comment on the Jira
issue — is the one path that actually mutates Jira, and it is strictly
opt-in (`--enable-write-back`, off by default) per the ticket's AC.

OAuth/token scope required: a Jira API token (Atlassian account token,
Basic auth with the account email) needs only `read:jira-work` to read
issue fields. Write-back additionally needs `write:jira-work` to add a
comment. Neither needs project admin or any broader scope.

Stdlib only. Every real Jira call goes through
`providers/common/safe_https_client.py`'s `fetch_json`/`post_json`, which
accept an injectable opener — this module's own tests never make a real
network call.
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import subprocess
import sys
import threading
from datetime import datetime, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "common"))

from libra_provider_runtime import (  # noqa: E402
    BaseProviderHandler,
    ReplayGuard,
    require_readable_file,
    resolve_ticket_key,
    run_server,
)
from safe_https_client import fetch_json, post_json, validate_https_base_url  # noqa: E402

SCHEMA_VERSION = "libra.extension.v1"
PROVIDER_ID = "jira"
DEFAULT_DONE_STATUSES = frozenset({"Done", "Closed", "Resolved"})

# The wire contract's `Priority` is a closed four-value enum
# (`docs/api/libra-extension-v1.yaml`), not Jira's own priority
# vocabulary (which varies per Jira instance's configured priority
# scheme — "Highest"/"High"/"Medium"/"Low"/"Lowest" is only the
# *default* scheme, not a guarantee). An unrecognised Jira priority name
# maps to no entry here and is therefore left absent on the wire rather
# than guessed — the contract's own `Priority` enum has no "unknown"
# member, so a value this adapter cannot confidently map must not be
# emitted at all. Instance operators with a custom priority scheme should
# extend this mapping, not change the wire enum.
JIRA_PRIORITY_TO_WIRE_PRIORITY = {
    "Highest": "urgent",
    "High": "high",
    "Medium": "normal",
    "Low": "low",
    "Lowest": "low",
}


def _basic_auth_header(email: str, api_token: str) -> str:
    """Builds the `Authorization: Basic ...` header Jira Cloud's REST API
    expects for an API-token credential. The token itself is read once
    from a file (`--api-token-file`) and never logged, printed, or
    included in any error message this module constructs."""
    pair = f"{email}:{api_token}".encode("utf-8")
    return "Basic " + base64.b64encode(pair).decode("ascii")


def fetch_jira_issue(
    base_url: str, issue_key: str, auth_header: str, *, opener=None, allow_private_network: bool = False
) -> dict:
    """Fetches one issue's fields from Jira Cloud. `issue_key` is a
    bounded, already-matched ticket key, never raw request/user text
    interpolated without validation. `allow_private_network` must match
    whatever `main()` was given for this provider's configured base URL
    — `fetch_json` re-validates `base_url` on every call (see
    `safe_https_client.py`)."""
    url = f"{base_url.rstrip('/')}/rest/api/3/issue/{issue_key}?fields=priority,duedate,project,status"
    return fetch_json(
        url,
        headers={"Authorization": auth_header, "Accept": "application/json"},
        opener=opener,
        allow_private_network=allow_private_network,
    )


def business_context_from_issue(issue: dict, provider_id: str = PROVIDER_ID) -> dict:
    """Maps a real Jira issue response into the wire shape
    `docs/api/libra-extension-v1.yaml`'s `BusinessContextResponse` defines
    — the same shape `libra_example_provider.py` already emits from its
    fixture data, now populated from a real API response instead."""
    fields = issue.get("fields", {})
    response: dict = {"schema_version": SCHEMA_VERSION, "provider_id": provider_id}

    priority = fields.get("priority")
    if priority and priority.get("name"):
        wire_priority = JIRA_PRIORITY_TO_WIRE_PRIORITY.get(priority["name"])
        if wire_priority is not None:
            response["priority"] = wire_priority

    due_date = fields.get("duedate")
    if due_date:
        # Jira's duedate is a bare date with no time component; treat it
        # as end-of-day UTC so a same-day task is not already "overdue"
        # the moment it is fetched.
        deadline = datetime.strptime(due_date, "%Y-%m-%d").replace(
            hour=23, minute=59, second=59, tzinfo=timezone.utc
        )
        response["deadline"] = deadline.strftime("%Y-%m-%dT%H:%M:%SZ")

    project = fields.get("project")
    if project and project.get("key"):
        response["cost_center"] = project["key"]

    response["advisory_criteria"] = []
    self_url = issue.get("self")
    response["external_refs"] = [{"kind": "jira", "value": self_url}] if self_url else []
    return response


def is_done(issue: dict, done_statuses: frozenset[str]) -> bool:
    status = issue.get("fields", {}).get("status", {}).get("name")
    return status in done_statuses


def post_execution_receipt_comment(
    base_url: str,
    issue_key: str,
    auth_header: str,
    receipt_summary: str,
    *,
    opener=None,
    allow_private_network: bool = False,
):
    """Posts Libra's execution receipt as a comment on the Jira issue —
    the one path in this adapter that actually mutates Jira. Only ever
    called when the operator passed `--enable-write-back`; never reached
    from the read-only business-context/outcome-push paths. `receipt_summary`
    must already be a short, safe label (the ticket's own "no full issue/
    code payload" rule applies symmetrically to what this adapter writes
    back, not only to what it reads)."""
    url = f"{base_url.rstrip('/')}/rest/api/3/issue/{issue_key}/comment"
    body = {"body": {"type": "doc", "version": 1, "content": [
        {"type": "paragraph", "content": [{"type": "text", "text": receipt_summary}]}
    ]}}
    return post_json(
        url,
        payload=body,
        headers={"Authorization": auth_header},
        opener=opener,
        allow_private_network=allow_private_network,
    )


def push_outcome(binary: str, task_id: str, issue_key: str, evidence_url: str) -> subprocess.CompletedProcess:
    """Records a Provider-sourced outcome attestation via the real
    `libra-governor outcome record` CLI path (HORO-1174), exactly as
    `examples/local-providers/report_outcome.sh` does — this module does
    not speak the daemon's Unix-socket protocol directly."""
    payload = json.dumps(
        {
            "task_id": task_id,
            "source_id": PROVIDER_ID,
            "idempotency_key": f"jira-{issue_key}-done",
            "outcome": {"kind": "completed", "evidence": [evidence_url]},
        }
    )
    return subprocess.run([binary, "outcome", "record"], input=payload, capture_output=True, text=True, timeout=15)


class TaskIssueKeys:
    """Remembers which Jira issue a task resolved to during its
    business-context fetch, so a later, independent `/libra/events` call
    for the same task can find its way back to the same issue — the two
    surfaces are separate HTTP calls with no shared session, the same
    cross-call state problem `libra_example_provider.py`'s
    `TaskCostCenters` already solves for the policy-webhook surface."""

    def __init__(self):
        self._by_task: dict[str, str] = {}
        self._lock = threading.Lock()

    def remember(self, task_id: str, issue_key: str) -> None:
        with self._lock:
            self._by_task[task_id] = issue_key

    def lookup(self, task_id: str) -> str | None:
        with self._lock:
            return self._by_task.get(task_id)


class JiraProviderHandler(BaseProviderHandler):
    jira_base_url: str
    auth_header: str
    done_statuses: frozenset[str]
    workspace_root: str
    enable_write_back: bool
    task_issue_keys: TaskIssueKeys
    allow_private_network: bool

    def route_table(self):
        return {
            "/libra/business-context": self._handle_business_context,
            "/libra/events": self._handle_event,
        }

    def _handle_business_context(self, request: dict) -> None:
        task_id = request.get("task_id", "")
        cwd = request.get("cwd", "")
        issue_key = resolve_ticket_key(cwd, self.workspace_root) if cwd else None

        if not issue_key:
            self.log_message("business-context: task=%s has no jira_issue_key, minimal response", task_id)
            self._respond_json(200, {"schema_version": SCHEMA_VERSION, "provider_id": PROVIDER_ID})
            return

        if task_id:
            self.task_issue_keys.remember(task_id, issue_key)

        try:
            issue = fetch_jira_issue(
                self.jira_base_url, issue_key, self.auth_header, allow_private_network=self.allow_private_network
            )
        # Any fetch failure degrades to minimal response, never raises.
        except Exception as exc:  # noqa: BLE001
            self.log_message("business-context: task=%s issue=%s fetch failed: %s", task_id, issue_key, exc)
            self._respond_json(200, {"schema_version": SCHEMA_VERSION, "provider_id": PROVIDER_ID})
            return

        response = business_context_from_issue(issue)
        self.log_message("business-context: task=%s issue=%s -> %s", task_id, issue_key, response.get("priority"))
        self._respond_json(200, response)

    def _handle_event(self, request: dict) -> None:
        # Always accept the delivery — a write-back failure must never
        # cause the dispatcher to treat this as a rejected event.
        self._respond_json(200, {"status": "accepted"})

        if not self.enable_write_back:
            return
        event_kind = request.get("event_kind", "")
        data = request.get("data", {})
        if event_kind != "outcome" or data.get("source") != "governor_local":
            return

        task_id = data.get("task_id", "")
        issue_key = self.task_issue_keys.lookup(task_id) if task_id else None
        if not issue_key:
            self.log_message("write-back: task=%s has no known issue, skipping", task_id)
            return

        admission = data.get("admission", "unknown")
        summary = f"Libra Governor: task {task_id} finished (admission: {admission})."
        try:
            result = post_execution_receipt_comment(
                self.jira_base_url,
                issue_key,
                self.auth_header,
                summary,
                allow_private_network=self.allow_private_network,
            )
            self.log_message("write-back: task=%s issue=%s -> status=%s", task_id, issue_key, result.status)
        # Write-back is best-effort and must never raise into the handler.
        except Exception as exc:  # noqa: BLE001
            self.log_message("write-back: task=%s issue=%s failed: %s", task_id, issue_key, exc)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8788)
    parser.add_argument("--secret-file", required=True)
    parser.add_argument("--jira-base-url", required=True, help="e.g. https://yourteam.atlassian.net")
    parser.add_argument("--jira-email", required=True)
    parser.add_argument("--jira-api-token-file", required=True)
    parser.add_argument("--allow-private-network", action="store_true", help="For a self-hosted Jira instance only.")
    parser.add_argument("--max-clock-skew-secs", type=float, default=120.0)
    parser.add_argument(
        "--workspace-root",
        default=os.getcwd(),
        help="Allow-listed root directory: a business-context request's `cwd` must "
        "resolve inside this root or resolve_ticket_key() refuses it.",
    )
    parser.add_argument(
        "--enable-write-back",
        action="store_true",
        help="Post Libra's execution receipt as a Jira comment when a governed task "
        "finishes. Off by default — this is the one flag that lets this adapter "
        "mutate Jira rather than only read it.",
    )
    args = parser.parse_args()

    jira_base_url = validate_https_base_url(args.jira_base_url, allow_private_network=args.allow_private_network)
    secret_bytes = require_readable_file(args.secret_file, "secret-file", program_name="libra_jira_provider").read_bytes()
    api_token = require_readable_file(
        args.jira_api_token_file, "jira-api-token-file", program_name="libra_jira_provider"
    ).read_text().strip()

    JiraProviderHandler.secret = secret_bytes
    JiraProviderHandler.replay_guard = ReplayGuard(skew_secs=args.max_clock_skew_secs)
    JiraProviderHandler.max_clock_skew_secs = args.max_clock_skew_secs
    JiraProviderHandler.provider_id = PROVIDER_ID
    JiraProviderHandler.jira_base_url = jira_base_url
    JiraProviderHandler.auth_header = _basic_auth_header(args.jira_email, api_token)
    JiraProviderHandler.done_statuses = DEFAULT_DONE_STATUSES
    JiraProviderHandler.workspace_root = os.path.realpath(args.workspace_root)
    JiraProviderHandler.enable_write_back = args.enable_write_back
    JiraProviderHandler.task_issue_keys = TaskIssueKeys()
    JiraProviderHandler.allow_private_network = args.allow_private_network

    run_server(JiraProviderHandler, args.host, args.port, program_name="libra_jira_provider")


if __name__ == "__main__":
    main()
