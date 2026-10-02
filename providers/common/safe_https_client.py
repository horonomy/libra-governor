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

Stdlib only (`urllib.request`), and every actual network call goes
through `fetch_json`/`post_json_or_none`, which accept an injectable
`opener` for tests — nothing in this module's own test suite makes a real
network call.
"""

from __future__ import annotations

import ipaddress
import json
import socket
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass

DEFAULT_TIMEOUT_SECS = 10.0
MAX_RESPONSE_BYTES = 256 * 1024


class UnsafeBaseUrlError(ValueError):
    """Raised when a configured base URL fails SSRF-safety validation."""


@dataclass(frozen=True)
class HttpResponse:
    status: int
    body: bytes


def _resolve_all(hostname: str) -> list[str]:
    """Returns every IP address `hostname` resolves to. A hostname that
    resolves to more than one address only needs one private/loopback
    member to be rejected — DNS rebinding and multi-homed records are
    exactly the case a single `getaddrinfo` result would miss."""
    try:
        infos = socket.getaddrinfo(hostname, None)
    except socket.gaierror as exc:
        raise UnsafeBaseUrlError(f"could not resolve host {hostname!r}: {exc}") from exc
    return [info[4][0] for info in infos]


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
    """
    parsed = urllib.parse.urlsplit(raw_url)
    if parsed.scheme != "https":
        raise UnsafeBaseUrlError(f"base URL must use https, got {raw_url!r}")
    if not parsed.hostname:
        raise UnsafeBaseUrlError(f"base URL has no host: {raw_url!r}")
    if parsed.username or parsed.password:
        raise UnsafeBaseUrlError("base URL must not carry embedded credentials")

    if allow_private_network:
        return raw_url

    for address in _resolve_all(parsed.hostname):
        ip = ipaddress.ip_address(address)
        if ip.is_loopback or ip.is_link_local or ip.is_private or ip.is_reserved or ip.is_multicast:
            raise UnsafeBaseUrlError(
                f"base URL {raw_url!r} resolves to a non-public address ({address}); "
                "pass allow_private_network=True for a deliberately self-hosted instance"
            )
    return raw_url


def fetch_json(
    url: str,
    *,
    headers: dict[str, str],
    timeout_secs: float = DEFAULT_TIMEOUT_SECS,
    opener: urllib.request.OpenerDirector | None = None,
) -> dict:
    """GETs `url` and parses the body as JSON. `url` must already have
    passed `validate_https_base_url` for its scheme/host — this function
    does not re-validate, since by the time a full request URL is built
    the host component may include a path the validator never saw."""
    request = urllib.request.Request(url, headers=headers, method="GET")
    open_fn = opener.open if opener is not None else urllib.request.urlopen
    with open_fn(request, timeout=timeout_secs) as response:
        raw = response.read(MAX_RESPONSE_BYTES + 1)
        if len(raw) > MAX_RESPONSE_BYTES:
            raise ValueError(f"response from {url} exceeded {MAX_RESPONSE_BYTES} bytes, refusing to parse")
        return json.loads(raw)


def post_json(
    url: str,
    *,
    payload: dict,
    headers: dict[str, str],
    timeout_secs: float = DEFAULT_TIMEOUT_SECS,
    opener: urllib.request.OpenerDirector | None = None,
) -> HttpResponse:
    """POSTs `payload` as JSON to `url`. Used only by a provider's
    explicit, opt-in write-back path (see each provider's
    `--enable-write-back` flag) — never called from a read-only code
    path."""
    body = json.dumps(payload).encode("utf-8")
    full_headers = {**headers, "Content-Type": "application/json"}
    request = urllib.request.Request(url, data=body, headers=full_headers, method="POST")
    open_fn = opener.open if opener is not None else urllib.request.urlopen
    try:
        with open_fn(request, timeout=timeout_secs) as response:
            return HttpResponse(status=response.status, body=response.read(MAX_RESPONSE_BYTES))
    except urllib.error.HTTPError as exc:
        return HttpResponse(status=exc.code, body=exc.read(MAX_RESPONSE_BYTES))
