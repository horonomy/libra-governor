#!/usr/bin/env python3
"""Shared HMAC-verified loopback server scaffolding for first-class Libra
Governor extension providers (HORO-1173), extracted from the pattern
proven in ``examples/local-providers/libra_example_provider.py``
(HORO-1174) rather than re-implemented per adapter.

Every security-critical piece here — signature verification, replay
rejection, loopback-only binding, the response sink — is copied
byte-for-byte in spirit from that reference provider, which is the
normative implementation of ``docs/api/libra-extension-v1.yaml``. This
module exists so a *second* real adapter (Jira, GitHub, and whatever
comes after) does not re-derive or subtly diverge from that same
security logic. ``libra_example_provider.py`` itself is intentionally
left untouched — it is already merged, tested, and CI-green; this module
is additive, not a refactor of it.

A concrete provider subclasses `BaseProviderHandler`, sets the class
attributes in its own `main()`, and implements only its own routes
(business-context / policy-decision / events) by overriding
`route_table()`. Stdlib only, matching the rest of this repository's
Python tooling.
"""

from __future__ import annotations

import hashlib
import hmac
import json
import os
import threading
import time
from collections.abc import Callable
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

SCHEMA_VERSION = "libra.extension.v1"
SIGNATURE_VERSION = "v1"
MAX_BODY_BYTES = 64 * 1024

# Mirrors the daemon's own loopback-literal-only config validation
# (`docs/adr/0005-local-extension-points.md`): a provider built on this
# runtime speaks plain HTTP deliberately, which is only safe because it
# never binds to a network-reachable interface.
LOOPBACK_LITERALS = frozenset({"127.0.0.1", "::1"})


def rfc3339(dt: datetime) -> str:
    return dt.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def require_loopback_host(host: str, *, program_name: str) -> None:
    """Raises `SystemExit` unless `host` is a loopback literal — the
    whole security boundary for a provider that speaks plain HTTP
    without TLS (see `libra_example_provider.py`'s identical, already
    reviewed and accepted rule)."""
    if host not in LOOPBACK_LITERALS:
        raise SystemExit(
            f"{program_name}: --host must be a loopback literal ({sorted(LOOPBACK_LITERALS)}), "
            f"got {host!r} — this provider speaks plain HTTP and must never bind a "
            "network-reachable interface."
        )


def require_readable_file(raw_path: str, label: str, *, program_name: str) -> Path:
    """Resolves a CLI-supplied path and requires it to be a real,
    existing, readable regular file before anything reads it."""
    path = Path(raw_path).expanduser().resolve()
    if not path.is_file() or not os.access(path, os.R_OK):
        raise SystemExit(f"{program_name}: --{label} does not resolve to a readable file: {raw_path!r}")
    return path


class ReplayGuard:
    """Tracks accepted (nonce -> expiry) pairs so a signed request cannot
    be replayed within the clock-skew window, and accepted `event_id`
    values for delivery-retry dedup."""

    def __init__(self, skew_secs: float):
        self._skew_secs = skew_secs
        self._nonces: dict[str, float] = {}
        self._event_ids: set[str] = set()
        self._lock = threading.Lock()

    def check_and_record_nonce(self, nonce: str) -> bool:
        now = time.time()
        with self._lock:
            self._nonces = {n: exp for n, exp in self._nonces.items() if exp > now}
            if nonce in self._nonces:
                return False
            self._nonces[nonce] = now + self._skew_secs
            return True

    def check_and_record_event_id(self, event_id: str) -> bool:
        with self._lock:
            if event_id in self._event_ids:
                return False
            self._event_ids.add(event_id)
            return True


class BaseProviderHandler(BaseHTTPRequestHandler):
    """HMAC verification, replay rejection, and the JSON response sink,
    shared by every first-class provider built on this runtime.

    Subclass attributes a concrete provider must set once at startup (via
    its own `main()`, mirroring `libra_example_provider.py`'s pattern):
    `secret` (bytes), `replay_guard` (`ReplayGuard`),
    `max_clock_skew_secs` (float), `provider_id` (str).
    """

    secret: bytes
    replay_guard: ReplayGuard
    max_clock_skew_secs: float
    provider_id: str

    def log_message(self, fmt, *args):  # noqa: A003 — stdlib override
        print(f"[{self.provider_id}] {self.address_string()} {fmt % args}")

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
        # `json.dumps` escapes every value it serializes. `nosniff`
        # additionally stops a client from MIME-sniffing this response as
        # HTML. No route may echo raw request content into an error
        # message — every `{"error": ...}` payload must be a static
        # string, mirroring the already-reviewed rule in
        # `libra_example_provider.py`.
        body = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("X-Content-Type-Options", "nosniff")
        self.end_headers()
        self.wfile.write(body)

    def route_table(self) -> dict[str, Callable[[dict], None]]:
        """Maps a request path to a handler taking the parsed JSON body.
        A concrete provider overrides this; the default is empty so a
        provider that forgets to wire a route gets an explicit 404
        rather than a silent no-op."""
        return {}

    def do_POST(self) -> None:  # noqa: N802 — stdlib override
        verified = self._verify_and_read_body()
        if verified is None:
            return
        body, _request_id = verified

        try:
            request = json.loads(body)
        except json.JSONDecodeError:
            self._respond_json(400, {"error": "malformed JSON body"})
            return

        handler = self.route_table().get(self.path)
        if handler is None:
            self._respond_json(404, {"error": "no route for the requested path"})
            return
        handler(request)


def run_server(handler_cls: type[BaseProviderHandler], host: str, port: int, *, program_name: str) -> None:
    """Starts `handler_cls` as a `ThreadingHTTPServer` on `host:port`.
    Refuses to start unless `host` is a loopback literal."""
    require_loopback_host(host, program_name=program_name)
    server = ThreadingHTTPServer((host, port), handler_cls)
    print(f"[{program_name}] listening on http://{host}:{port}")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
