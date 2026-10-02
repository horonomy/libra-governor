#!/usr/bin/env python3
"""A real, local reference implementation of a Libra Governor extension
provider (HORO-1174) — the three outbound surfaces the daemon calls:
business-context fetch, policy-webhook, and signed event delivery. See
``docs/api/libra-extension-v1.yaml`` for the wire contract this
implements and ``README.md`` in this directory for how to run it end to
end against a real daemon.

Every route here does real work, not a stubbed constant:

- Signature verification recomputes the HMAC-SHA256 exactly as
  ``crates/extension/src/sign.rs`` does, checks a clock-skew window on
  ``x-libra-timestamp``, and tracks ``x-libra-nonce`` values it has
  already seen to reject a replay. This logic, and the loopback-only
  server scaffolding around it, now live in
  ``providers/common/libra_provider_runtime.py`` (HORO-1173) rather than
  being duplicated here — that module was extracted *from* this file so
  a second real adapter (Jira, GitHub) would share one audited
  implementation instead of re-deriving it. This module imports that
  shared runtime rather than keeping its own copy.
- The business-context route resolves a real ticket key by running
  ``git -C <workspace_root> rev-parse --abbrev-ref HEAD`` (the request's
  ``cwd`` is used only as an equality check against ``--workspace-root``,
  never as the command's own directory argument) and matching the branch
  name against a ticket-key pattern, then looks the key up in
  ``tickets.json``.
- The policy-webhook route enforces a real per-cost-center token cap
  (``cost_caps.json``) against the task's ``projected_resource``,
  remembering which cost center a task belongs to from its own earlier
  business-context call.
- The events route tracks ``event_id`` values it has already accepted
  (dedup across delivery retries) and appends every accepted delivery to
  an append-only ``events.jsonl`` log.

Run with no arguments for sane local defaults:

    python3 libra_example_provider.py --secret-file /path/to/secret

Stdlib only — no third-party dependencies, matching this repository's
existing experiment harnesses under ``experiments/``.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import threading
from datetime import datetime, timedelta, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent.parent / "providers" / "common"))

from libra_provider_runtime import (  # noqa: E402
    SCHEMA_VERSION,
    BaseProviderHandler,
    ReplayGuard,
    require_loopback_host as _shared_require_loopback_host,
    require_readable_file,
    rfc3339,
    run_server,
)
# Re-exported for this module's own test suite (test_libra_example_provider.py).
from libra_provider_runtime import LOOPBACK_LITERALS  # noqa: E402,F401

TICKET_KEY_PATTERN = re.compile(r"([A-Za-z]{2,10}-\d{1,6})")


def require_loopback_host(host: str) -> None:
    """Thin wrapper over the shared runtime's check, preserving this
    module's original single-argument public API (this file's own test
    suite, `test_libra_example_provider.py`, calls it this way)."""
    _shared_require_loopback_host(host, program_name="libra_example_provider")


class TaskCostCenters:
    """Remembers which cost center a task was assigned during its
    business-context fetch, so the later, separate policy-webhook call
    for the same task can enforce that cost center's cap. A real
    provider needs exactly this kind of cross-call state — the two
    surfaces are independent HTTP calls with no shared session."""

    def __init__(self):
        self._by_task: dict[str, str] = {}
        self._lock = threading.Lock()

    def remember(self, task_id: str, cost_center: str) -> None:
        with self._lock:
            self._by_task[task_id] = cost_center

    def lookup(self, task_id: str) -> str | None:
        with self._lock:
            return self._by_task.get(task_id)


def resolve_ticket_key(cwd: str, workspace_root: str) -> str | None:
    """Runs a real `git` subprocess against the provider's own
    `--workspace-root` and extracts a ticket key from the current branch
    name (e.g. `v0.0.2/HORO-1174/extension_contracts` -> `HORO-1174`).

    `cwd` is attacker-influenced request input. It is never itself passed
    to a filesystem or subprocess call — it is used **only as an
    equality selector** against `workspace_root` (an operator-supplied
    CLI argument, not request input). A request whose `cwd` does not
    match the provider's configured workspace is simply not resolved;
    the value that actually reaches `os.path.isdir`/`git -C` is always
    `workspace_root`, which the request can select but never set."""
    real_root = os.path.realpath(workspace_root)
    if os.path.realpath(cwd) != real_root:
        return None
    if not os.path.isdir(real_root):
        return None
    try:
        result = subprocess.run(
            ["git", "-C", real_root, "rev-parse", "--abbrev-ref", "HEAD"],
            capture_output=True,
            text=True,
            timeout=5,
        )
    except (OSError, subprocess.TimeoutExpired, ValueError):
        return None
    if result.returncode != 0:
        return None
    match = TICKET_KEY_PATTERN.search(result.stdout.strip())
    return match.group(1).upper() if match else None


class ProviderHandler(BaseProviderHandler):
    # Set by main() via a subclass/functools.partial-free pattern:
    # these are class attributes assigned once at startup.
    tickets: dict
    cost_caps: dict
    events_log_path: Path
    task_cost_centers: TaskCostCenters
    workspace_root: str

    def route_table(self):
        return {
            "/libra/business-context": self._handle_business_context,
            "/libra/policy-decision": self._handle_policy_decision,
            "/libra/events": self._handle_event,
        }

    def _handle_business_context(self, request: dict) -> None:
        task_id = request.get("task_id", "")
        cwd = request.get("cwd", "")
        ticket_key = resolve_ticket_key(cwd, self.workspace_root) if cwd else None
        ticket = self.tickets.get(ticket_key) if ticket_key else None

        response = {
            "schema_version": SCHEMA_VERSION,
            "provider_id": self.provider_id,
        }
        if ticket is not None:
            response["priority"] = ticket["priority"]
            deadline_hours = ticket.get("deadline_hours_from_now")
            if deadline_hours is not None:
                deadline = datetime.now(timezone.utc) + timedelta(hours=deadline_hours)
                response["deadline"] = rfc3339(deadline)
            response["cost_center"] = ticket["cost_center"]
            response["advisory_criteria"] = ticket.get("advisory_criteria", [])
            response["external_refs"] = ticket.get("external_refs", [])
            if task_id:
                self.task_cost_centers.remember(task_id, ticket["cost_center"])

        self.log_message(
            "business-context: task=%s cwd=%s ticket=%s -> %s",
            task_id,
            cwd,
            ticket_key,
            "matched" if ticket is not None else "no match (minimal response)",
        )
        self._respond_json(200, response)

    def _handle_policy_decision(self, request: dict) -> None:
        task_id = request.get("task_id", "")
        projected = request.get("projected_resource", {})
        cost_center = self.task_cost_centers.lookup(task_id) or "unassigned"
        cap = self.cost_caps.get(cost_center, self.cost_caps.get("unassigned", 0))

        if projected.get("kind") != "tokens":
            # Real gating logic only covers the token unit this fixture
            # set's caps are denominated in; anything else abstains
            # rather than guessing a conversion.
            verdict = "abstain"
            reason = None
        else:
            amount = projected.get("amount", 0)
            if amount <= cap:
                verdict = "approve"
                reason = None
            else:
                verdict = "reject"
                reason = (
                    f"cost center {cost_center!r} cap is {cap} tokens; "
                    f"projected usage is {amount} tokens"
                )

        self.log_message(
            "policy-decision: task=%s cost_center=%s cap=%s projected=%s -> %s",
            task_id,
            cost_center,
            cap,
            projected,
            verdict,
        )
        response = {
            "schema_version": SCHEMA_VERSION,
            "provider_id": self.provider_id,
            "verdict": verdict,
        }
        if reason is not None:
            response["reason"] = reason
        self._respond_json(200, response)

    def _handle_event(self, request: dict) -> None:
        event_id = request.get("event_id", "")
        event_kind = request.get("event_kind", "")
        delivery_attempt = self.headers.get("x-libra-delivery-attempt", "1")
        first_time = self.replay_guard.check_and_record_event_id(event_id) if event_id else True

        record = {
            "received_at": rfc3339(datetime.now(timezone.utc)),
            "event_id": event_id,
            "event_kind": event_kind,
            "delivery_attempt": delivery_attempt,
            "deduped": not first_time,
            "data": request.get("data", {}),
        }
        with self.events_log_path.open("a", encoding="utf-8") as f:
            f.write(json.dumps(record) + "\n")

        self.log_message(
            "event: id=%s kind=%s attempt=%s deduped=%s",
            event_id,
            event_kind,
            delivery_attempt,
            not first_time,
        )
        # Always 200 — a dedup is a successful accept from the
        # dispatcher's point of view, exactly like the first delivery.
        self._respond_json(200, {"status": "accepted"})


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8787)
    parser.add_argument(
        "--secret-file",
        required=True,
        help="Path to a file whose exact bytes are the shared HMAC signing secret "
        "(matches the daemon's config.json secret_command output for each surface).",
    )
    parser.add_argument(
        "--tickets-file",
        default=str(Path(__file__).parent / "tickets.json"),
    )
    parser.add_argument(
        "--cost-caps-file",
        default=str(Path(__file__).parent / "cost_caps.json"),
    )
    parser.add_argument(
        "--events-log",
        default=str(Path(__file__).parent / "events.jsonl"),
    )
    parser.add_argument("--max-clock-skew-secs", type=float, default=120.0)
    parser.add_argument("--provider-id", default="example-provider")
    parser.add_argument(
        "--workspace-root",
        default=os.getcwd(),
        help="Allow-listed root directory: a business-context request's `cwd` must "
        "resolve inside this root or resolve_ticket_key() refuses it. Defaults to "
        "this provider's own working directory.",
    )
    args = parser.parse_args()

    require_loopback_host(args.host)

    secret_bytes = require_readable_file(args.secret_file, "secret-file", program_name="libra_example_provider").read_bytes()
    tickets = json.loads(
        require_readable_file(args.tickets_file, "tickets-file", program_name="libra_example_provider").read_text()
    )
    cost_caps = json.loads(
        require_readable_file(args.cost_caps_file, "cost-caps-file", program_name="libra_example_provider").read_text()
    )
    events_log_path = Path(args.events_log).expanduser().resolve()
    events_log_path.parent.mkdir(parents=True, exist_ok=True)

    ProviderHandler.secret = secret_bytes
    ProviderHandler.tickets = tickets
    ProviderHandler.cost_caps = cost_caps
    ProviderHandler.events_log_path = events_log_path
    ProviderHandler.replay_guard = ReplayGuard(skew_secs=args.max_clock_skew_secs)
    ProviderHandler.task_cost_centers = TaskCostCenters()
    ProviderHandler.max_clock_skew_secs = args.max_clock_skew_secs
    ProviderHandler.provider_id = args.provider_id
    ProviderHandler.workspace_root = os.path.realpath(args.workspace_root)

    run_server(ProviderHandler, args.host, args.port, program_name="libra_example_provider")


if __name__ == "__main__":
    main()
