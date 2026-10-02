#!/usr/bin/env python3
"""A minimal, SSRF-aware HTTPS client for first-class Libra providers
calling a real third-party API (Jira Cloud, GitHub) — HORO-1173.

The provider's own inbound surface (the loopback HTTP server in
`libra_provider_runtime.py`) is a different, already-solved problem
(HORO-1174's loopback-only trust boundary). This module is the *outbound*
side: a provider process making a real call to a real remote API using an
operator-configured base URL. That URL is operator config, not
request-derived, but it is still validated before every call rather than
trusted once at startup — a provider that reads its config from a file an
attacker could modify deserves the same discipline as one that reads it
from a request.

Two SSRF bypass classes that a naive "validate the URL, then call
`urllib.request.urlopen`" implementation misses, both closed here:

1. **DNS rebinding / TOCTOU.** If validation resolves a hostname and
   `urlopen` independently re-resolves it moments later, an attacker
   controlling DNS for that hostname can answer safely the first time and
   unsafely the second. This module resolves a hostname exactly once per
   connection attempt, validates *that* resolution, and pins the actual
   TCP connection to the validated IP — never letting a second,
   independent resolution happen later in the stack.
2. **Redirect-following bypass.** `urlopen` follows redirects
   automatically without re-running SSRF validation on the `Location`
   target. This module never delegates to that automatic handling: it
   follows redirects itself, in a bounded loop, re-running the exact same
   validation-then-pin step on every hop before connecting to it.

Stdlib only. Every actual real-network call goes through `fetch_json`/
`post_json`, which accept an injectable `opener` for tests — when an
`opener` is supplied, these functions use it directly instead of the
pinned/redirect-validated path, exactly the seam this module's own test
suite uses so no test ever makes a real network call or real DNS
resolution.
"""

from __future__ import annotations

import http.client
import ipaddress
import json
import socket
import ssl
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass

DEFAULT_TIMEOUT_SECS = 10.0
MAX_RESPONSE_BYTES = 256 * 1024
MAX_REDIRECTS = 5
_REDIRECT_STATUSES = frozenset({301, 302, 303, 307, 308})


class UnsafeBaseUrlError(ValueError):
    """Raised when a configured base URL fails SSRF-safety validation."""


@dataclass(frozen=True)
class HttpResponse:
    status: int
    body: bytes


def _validate_url_structure(raw_url: str) -> urllib.parse.SplitResult:
    """Scheme/host/credentials checks only — no DNS resolution. Resolution
    happens exactly once, at the one call site that also pins the
    connection (`_resolve_validated_ip`), so this function never performs
    a second, independent lookup a rebinding attacker could race."""
    parsed = urllib.parse.urlsplit(raw_url)
    if parsed.scheme != "https":
        raise UnsafeBaseUrlError(f"base URL must use https, got {raw_url!r}")
    if not parsed.hostname:
        raise UnsafeBaseUrlError(f"base URL has no host: {raw_url!r}")
    if parsed.username or parsed.password:
        raise UnsafeBaseUrlError("base URL must not carry embedded credentials")
    return parsed


def _resolve_all(hostname: str) -> list[str]:
    """Returns every IP address `hostname` resolves to. A hostname that
    resolves to more than one address only needs one private/loopback
    member to be rejected — a multi-homed record is exactly the case a
    single `getaddrinfo` result would miss."""
    try:
        infos = socket.getaddrinfo(hostname, None)
    except socket.gaierror as exc:
        raise UnsafeBaseUrlError(f"could not resolve host {hostname!r}: {exc}") from exc
    return [info[4][0] for info in infos]


def _resolve_validated_ip(hostname: str, *, allow_private_network: bool) -> str:
    """Resolves `hostname` exactly once, validates every resolved address
    unless `allow_private_network` is set, and returns one validated IP to
    pin the actual connection to. Called from exactly one place
    (`_request_once`) so the address that is validated is always the
    address that is actually connected to — closing the DNS-rebinding/
    TOCTOU gap a "validate, then let the HTTP client re-resolve" design
    would leave open."""
    addresses = _resolve_all(hostname)
    if not allow_private_network:
        for address in addresses:
            ip = ipaddress.ip_address(address)
            if ip.is_loopback or ip.is_link_local or ip.is_private or ip.is_reserved or ip.is_multicast:
                raise UnsafeBaseUrlError(
                    f"host {hostname!r} resolves to a non-public address ({address}); "
                    "pass allow_private_network=True for a deliberately self-hosted instance"
                )
    return addresses[0]


def validate_https_base_url(raw_url: str, *, allow_private_network: bool = False) -> str:
    """Rejects a base URL that is not plain HTTPS, or that resolves to a
    loopback/link-local/private/reserved address — the inverse of
    HORO-1174's loopback-only rule, since here the destination is a real
    remote API and a private-network target is the SSRF case, not the
    safety requirement. Returns the validated URL unchanged (never
    rewritten) so a caller cannot be surprised by a silently-altered
    destination.

    `allow_private_network` exists only for a provider operator pointed
    at a self-hosted Jira/GitHub Enterprise instance on their own network
    — it is never the default, and a caller setting it is making an
    explicit, documented choice, not falling back to one.

    This function performs its own resolution for a fail-fast startup
    check; it is not the function the real request path relies on for
    TOCTOU safety — `_resolve_validated_ip` is, because it resolves and
    pins in the same step that actually connects.
    """
    parsed = _validate_url_structure(raw_url)
    if not allow_private_network:
        _resolve_validated_ip(parsed.hostname, allow_private_network=allow_private_network)
    return raw_url


class _PinnedHTTPSConnection(http.client.HTTPSConnection):
    """An `HTTPSConnection` whose `connect()` dials a specific, already-
    validated IP address instead of re-resolving `host` — the mechanism
    that actually closes the DNS-rebinding/TOCTOU gap. TLS SNI and
    hostname verification still use the real `host`, via `server_hostname`,
    so certificate validation is unaffected by connecting to a raw IP."""

    def __init__(self, hostname: str, pinned_ip: str, port: int, *, timeout: float):
        context = ssl.create_default_context()
        # Stated explicitly rather than relied on as create_default_context()'s
        # implicit default: this connection dials a raw, pinned IP address
        # rather than `hostname`, which is exactly the shape a static SSRF
        # scanner expects to see a verification bypass in. There is none —
        # certificate hostname verification against the real `hostname` (via
        # `server_hostname` in `connect()` below) is mandatory here, not
        # merely the unstated default.
        context.check_hostname = True
        context.verify_mode = ssl.CERT_REQUIRED
        super().__init__(hostname, port, timeout=timeout, context=context)
        self._pinned_ip = pinned_ip

    def connect(self) -> None:
        sock = socket.create_connection((self._pinned_ip, self.port), self.timeout)
        self.sock = self._context.wrap_socket(sock, server_hostname=self.host)


def _request_once(
    method: str, url: str, *, headers: dict[str, str], body: bytes | None, timeout_secs: float, allow_private_network: bool
) -> tuple[int, str | None, bytes]:
    """Performs exactly one HTTP request — no redirect-following — against
    a freshly resolved-and-pinned IP. Returns `(status, location_header,
    body_bytes)`; the caller decides whether to follow `location_header`."""
    parsed = _validate_url_structure(url)
    hostname = parsed.hostname
    port = parsed.port or 443
    pinned_ip = _resolve_validated_ip(hostname, allow_private_network=allow_private_network)
    path = urllib.parse.urlunsplit(("", "", parsed.path or "/", parsed.query, ""))

    conn = _PinnedHTTPSConnection(hostname, pinned_ip, port, timeout=timeout_secs)
    try:
        conn.putrequest(method, path)
        for key, value in headers.items():
            conn.putheader(key, value)
        if body is not None:
            conn.putheader("Content-Length", str(len(body)))
            conn.endheaders(body)
        else:
            conn.endheaders()

        response = conn.getresponse()
        raw = response.read(MAX_RESPONSE_BYTES + 1)
        if len(raw) > MAX_RESPONSE_BYTES:
            raise ValueError(f"response from {url} exceeded {MAX_RESPONSE_BYTES} bytes, refusing to parse")
        return response.status, response.getheader("Location"), raw
    finally:
        conn.close()


def _request_with_validated_redirects(
    method: str,
    url: str,
    *,
    headers: dict[str, str],
    body: bytes | None,
    timeout_secs: float,
    allow_private_network: bool,
    max_redirects: int = MAX_REDIRECTS,
) -> tuple[int, bytes]:
    """Follows redirects itself, in a bounded loop, re-running
    `_request_once` (full validate-then-pin) on every hop's target before
    connecting to it — never the automatic, unvalidated redirect-following
    a stock `urlopen` call would perform."""
    current_url, current_method, current_body = url, method, body
    for _ in range(max_redirects + 1):
        status, location, raw = _request_once(
            current_method,
            current_url,
            headers=headers,
            body=current_body,
            timeout_secs=timeout_secs,
            allow_private_network=allow_private_network,
        )
        if status not in _REDIRECT_STATUSES or not location:
            return status, raw
        current_url = urllib.parse.urljoin(current_url, location)
        if status == 303 or (status in (301, 302) and current_method == "POST"):
            current_method, current_body = "GET", None
    raise UnsafeBaseUrlError(f"too many redirects fetching {url!r} (limit {max_redirects})")


def fetch_json(
    url: str,
    *,
    headers: dict[str, str],
    timeout_secs: float = DEFAULT_TIMEOUT_SECS,
    opener: urllib.request.OpenerDirector | None = None,
    allow_private_network: bool = False,
) -> dict:
    """GETs `url` and parses the body as JSON. In real usage (`opener` not
    supplied), every hop — the initial request and any redirect — is
    independently resolved, validated, and pinned by
    `_request_with_validated_redirects`."""
    if opener is not None:
        request = urllib.request.Request(url, headers=headers, method="GET")
        with opener.open(request, timeout=timeout_secs) as response:
            raw = response.read(MAX_RESPONSE_BYTES + 1)
            if len(raw) > MAX_RESPONSE_BYTES:
                raise ValueError(f"response from {url} exceeded {MAX_RESPONSE_BYTES} bytes, refusing to parse")
            return json.loads(raw)

    status, raw = _request_with_validated_redirects(
        "GET", url, headers=headers, body=None, timeout_secs=timeout_secs, allow_private_network=allow_private_network
    )
    if status >= 400:
        raise urllib.error.HTTPError(url, status, f"HTTP {status} fetching {url}", None, None)
    return json.loads(raw)


def post_json(
    url: str,
    *,
    payload: dict,
    headers: dict[str, str],
    timeout_secs: float = DEFAULT_TIMEOUT_SECS,
    opener: urllib.request.OpenerDirector | None = None,
    allow_private_network: bool = False,
) -> HttpResponse:
    """POSTs `payload` as JSON to `url`. Used only by a provider's
    explicit, opt-in write-back path (see each provider's
    `--enable-write-back` flag) — never called from a read-only code
    path. Same pinned/redirect-validated real request path as
    `fetch_json` when `opener` is not supplied."""
    body = json.dumps(payload).encode("utf-8")
    full_headers = {**headers, "Content-Type": "application/json"}

    if opener is not None:
        request = urllib.request.Request(url, data=body, headers=full_headers, method="POST")
        try:
            with opener.open(request, timeout=timeout_secs) as response:
                return HttpResponse(status=response.status, body=response.read(MAX_RESPONSE_BYTES))
        except urllib.error.HTTPError as exc:
            return HttpResponse(status=exc.code, body=exc.read(MAX_RESPONSE_BYTES))

    status, raw = _request_with_validated_redirects(
        "POST",
        url,
        headers=full_headers,
        body=body,
        timeout_secs=timeout_secs,
        allow_private_network=allow_private_network,
    )
    return HttpResponse(status=status, body=raw)
