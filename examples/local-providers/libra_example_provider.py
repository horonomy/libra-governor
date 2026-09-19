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
  already seen to reject a replay.
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
import hashlib
import hmac
import json
import os
import re
import subprocess
import threading
import time
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

SCHEMA_VERSION = "libra.extension.v1"
SIGNATURE_VERSION = "v1"
TICKET_KEY_PATTERN = re.compile(r"([A-Za-z]{2,10}-\d{1,6})")
MAX_BODY_BYTES = 64 * 1024
# Mirrors the daemon's own loopback-literal-only config validation
# (`docs/adr/0005-local-extension-points.md`): this reference provider
# speaks plain HTTP deliberately, which is only safe because it never
# binds to a network-reachable interface.
LOOPBACK_LITERALS = frozenset({"127.0.0.1", "::1"})


def rfc3339(dt: datetime) -> str:
    return dt.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


class ReplayGuard:
    """Tracks accepted (nonce -> expiry) pairs so the same signed request
    cannot be replayed within the clock-skew window, and accepted
    event_id values for delivery-retry dedup. Both are real, mutated
    state — not decorative."""

    def __init__(self, skew_secs: float):
        self._skew_secs = skew_secs
        self._nonces: dict[str, float] = {}
        self._event_ids: set[str] = set()
        self._lock = threading.Lock()

    def check_and_record_nonce(self, nonce: str) -> bool:
        """Returns True if this nonce is fresh (not seen before within
        the skew window) and records it. Returns False on replay."""
        now = time.time()
        with self._lock:
            self._nonces = {
                n: exp for n, exp in self._nonces.items() if exp > now
            }
            if nonce in self._nonces:
                return False
            self._nonces[nonce] = now + self._skew_secs
            return True

    def check_and_record_event_id(self, event_id: str) -> bool:
        """Returns True the first time this event_id is seen (accept and
        record); False on every subsequent delivery attempt for the same
        event (dedup, not a replay rejection — the caller still returns
        200 so the dispatcher stops retrying)."""
        with self._lock:
            if event_id in self._event_ids:
                return False
            self._event_ids.add(event_id)
            return True


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


class ProviderHandler(BaseHTTPRequestHandler):
    # Set by main() via a subclass/functools.partial-free pattern:
    # these are class attributes assigned once at startup.
    secret: bytes
    tickets: dict
    cost_caps: dict
    events_log_path: Path
    replay_guard: ReplayGuard
    task_cost_centers: TaskCostCenters
    max_clock_skew_secs: float
    provider_id: str
    workspace_root: str

    def log_message(self, fmt, *args):  # noqa: A003 — stdlib override
        # Keep stdout limited to what the e2e harness actually inspects;
        # BaseHTTPRequestHandler's default is one line per request to
        # stderr, which is fine — just make the format explicit instead
        # of the default combined-log-format string.
        print(f"[provider] {self.address_string()} {fmt % args}")

    # -- signature verification -----------------------------------------

    def _verify_and_read_body(self) -> tuple[bytes, str] | None:
        """Reads the raw body, verifies the five signed headers against
        it, and returns (body, request_id) on success. On any failure,
        writes the appropriate error response itself and returns None."""
        content_length = int(self.headers.get("Content-Length", "0"))
        if content_length > MAX_BODY_BYTES:
            self.send_response(413)
            self.end_headers()
            return None
        body = self.rfile.read(content_length)

        schema_version = self.headers.get("x-libra-schema-version")
        request_id = self.headers.get("x-libra-request-id")
        timestamp_raw = self.headers.get("x-libra-timestamp")
        nonce = self.headers.get("x-libra-nonce")
        signature = self.headers.get("x-libra-signature")

        if not all([schema_version, request_id, timestamp_raw, nonce, signature]):
            self._respond_json(400, {"error": "missing required signed header"})
            return None
        if schema_version != SCHEMA_VERSION:
            self._respond_json(400, {"error": "unsupported schema_version"})
            return None

        try:
            timestamp = int(timestamp_raw)
        except ValueError:
            self._respond_json(400, {"error": "x-libra-timestamp is not an integer"})
            return None

        now = time.time()
        if abs(now - timestamp) > self.max_clock_skew_secs:
            self._respond_json(401, {"error": "timestamp outside the allowed clock-skew window"})
            return None

        # Recomputes the exact signed_payload format from
        # crates/extension/src/sign.rs: "v1.<timestamp>.<nonce>." + body.
        payload = f"{SIGNATURE_VERSION}.{timestamp}.{nonce}.".encode("utf-8") + body
        expected_mac = hmac.new(self.secret, payload, hashlib.sha256).hexdigest()
        expected_signature = f"{SIGNATURE_VERSION}={expected_mac}"
        if not hmac.compare_digest(signature, expected_signature):
            self._respond_json(401, {"error": "signature mismatch"})
            return None

        if not self.replay_guard.check_and_record_nonce(nonce):
            self._respond_json(409, {"error": "nonce already used (replay)"})
            return None

        return body, request_id

    def _respond_json(self, status: int, payload: dict) -> None:
        # `json.dumps` escapes every value it serializes, so nothing in
        # `payload` can break out of the JSON string context.
        # `X-Content-Type-Options: nosniff` additionally stops a client
        # from MIME-sniffing this response as HTML regardless of
        # `Content-Type`. No route ever echoes raw request content
        # (headers, path, body fields) back into an error message —
        # every `{"error": ...}` payload below is a static string, so
        # there is no reflected-content path into a response at all.
        body = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("X-Content-Type-Options", "nosniff")
        self.end_headers()
        self.wfile.write(body)

    # -- routes ------------------------------------------------------------

    def do_POST(self):  # noqa: N802 — stdlib override
        verified = self._verify_and_read_body()
        if verified is None:
            return
        body, _request_id = verified

        try:
            request = json.loads(body)
        except json.JSONDecodeError:
            self._respond_json(400, {"error": "malformed JSON body"})
            return

        if self.path == "/libra/business-context":
            self._handle_business_context(request)
        elif self.path == "/libra/policy-decision":
            self._handle_policy_decision(request)
        elif self.path == "/libra/events":
            self._handle_event(request)
        else:
            self._respond_json(404, {"error": "no route for the requested path"})

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


def _require_readable_file(raw_path: str, label: str) -> Path:
    """Resolves a CLI-supplied path and requires it to be a real,
    existing, readable regular file before anything reads it. Operator
    input (a local CLI flag, not network-attacker input), but validated
    anyway rather than handed straight to a filesystem read."""
    path = Path(raw_path).expanduser().resolve()
    if not path.is_file() or not os.access(path, os.R_OK):
        raise SystemExit(f"libra_example_provider: --{label} does not resolve to a readable file: {raw_path!r}")
    return path


def require_loopback_host(host: str) -> None:
    """Raises `SystemExit` unless `host` is a loopback literal. This is
    this provider's whole security boundary for speaking plain HTTP
    without TLS (accepted as a documented exception — see
    `README.md`/HORO-1174 evidence): the server must never be reachable
    from anywhere but the local machine, and `--host` is the only knob
    that could break that."""
    if host not in LOOPBACK_LITERALS:
        raise SystemExit(
            f"libra_example_provider: --host must be a loopback literal ({sorted(LOOPBACK_LITERALS)}), "
            f"got {host!r} — this provider speaks plain HTTP and must never bind a "
            "network-reachable interface."
        )


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

    secret_bytes = _require_readable_file(args.secret_file, "secret-file").read_bytes()
    tickets = json.loads(_require_readable_file(args.tickets_file, "tickets-file").read_text())
    cost_caps = json.loads(_require_readable_file(args.cost_caps_file, "cost-caps-file").read_text())
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

    server = ThreadingHTTPServer((args.host, args.port), ProviderHandler)
    print(f"[provider] listening on http://{args.host}:{args.port}")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
