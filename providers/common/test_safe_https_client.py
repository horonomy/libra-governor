#!/usr/bin/env python3
"""Tests for `safe_https_client.py` (HORO-1173). No real network call is
made anywhere in this file — every `fetch_json`/`post_json` call passes a
fake opener.

Run with: python3 -m unittest discover -s providers -p 'test_*.py'
"""

from __future__ import annotations

import io
import json
import unittest
from unittest.mock import MagicMock, patch

import safe_https_client as shc


class _FakeResponse(io.BytesIO):
    def __init__(self, body: bytes, status: int = 200):
        super().__init__(body)
        self.status = status

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return False


class ValidateHttpsBaseUrlTest(unittest.TestCase):
    def test_rejects_non_https_scheme(self):
        with self.assertRaises(shc.UnsafeBaseUrlError):
            shc.validate_https_base_url("http://example.atlassian.net")

    def test_rejects_embedded_credentials(self):
        with self.assertRaises(shc.UnsafeBaseUrlError):
            shc.validate_https_base_url("https://user:pass@example.atlassian.net")

    def test_rejects_url_with_no_host(self):
        with self.assertRaises(shc.UnsafeBaseUrlError):
            shc.validate_https_base_url("https:///path-only")

    def test_rejects_loopback_target_by_default(self):
        with self.assertRaises(shc.UnsafeBaseUrlError):
            shc.validate_https_base_url("https://127.0.0.1")

    def test_rejects_private_network_target_by_default(self):
        with self.assertRaises(shc.UnsafeBaseUrlError):
            shc.validate_https_base_url("https://10.0.0.5")

    def test_allows_private_network_target_when_explicitly_opted_in(self):
        validated = shc.validate_https_base_url("https://10.0.0.5", allow_private_network=True)
        self.assertEqual(validated, "https://10.0.0.5")

    def test_allows_a_real_public_looking_host(self):
        # api.github.com resolves publicly in any real environment; this
        # exercises the real resolution path end to end without faking
        # DNS, matching this repo's convention of preferring real
        # behavior over a mock where one is cheaply available.
        validated = shc.validate_https_base_url("https://api.github.com")
        self.assertEqual(validated, "https://api.github.com")

    def test_returns_the_url_unchanged(self):
        url = "https://example.atlassian.net/some/path"
        self.assertEqual(shc.validate_https_base_url(url), url)


class FetchJsonTest(unittest.TestCase):
    def test_parses_a_json_response_via_injected_opener(self):
        opener = MagicMock()
        opener.open.return_value = _FakeResponse(json.dumps({"ok": True}).encode("utf-8"))
        result = shc.fetch_json("https://example.atlassian.net/x", headers={}, opener=opener)
        self.assertEqual(result, {"ok": True})

    def test_refuses_an_oversized_response_without_parsing_it(self):
        opener = MagicMock()
        oversized = b"x" * (shc.MAX_RESPONSE_BYTES + 1)
        opener.open.return_value = _FakeResponse(oversized)
        with self.assertRaises(ValueError):
            shc.fetch_json("https://example.atlassian.net/x", headers={}, opener=opener)

    def test_never_calls_real_urlopen_when_opener_is_injected(self):
        opener = MagicMock()
        opener.open.return_value = _FakeResponse(b"{}")
        shc.fetch_json("https://example.atlassian.net/x", headers={}, opener=opener)
        opener.open.assert_called_once()


class PostJsonTest(unittest.TestCase):
    def test_returns_status_and_body_on_success(self):
        opener = MagicMock()
        opener.open.return_value = _FakeResponse(b"created", status=201)
        response = shc.post_json("https://example.atlassian.net/x", payload={"a": 1}, headers={}, opener=opener)
        self.assertEqual(response.status, 201)
        self.assertEqual(response.body, b"created")


class ResolveValidatedIpTest(unittest.TestCase):
    def test_rejects_a_loopback_resolved_address_by_default(self):
        with self.assertRaises(shc.UnsafeBaseUrlError):
            shc._resolve_validated_ip("127.0.0.1", allow_private_network=False)

    def test_allows_loopback_when_explicitly_opted_in(self):
        ip = shc._resolve_validated_ip("127.0.0.1", allow_private_network=True)
        self.assertEqual(ip, "127.0.0.1")

    def test_resolves_a_real_public_host_exactly_once(self):
        # DNS-rebinding/TOCTOU protection depends on this being the ONE
        # resolution the connection is pinned to — a caller that resolved
        # again later would reopen the exact gap this function exists to
        # close.
        with patch("safe_https_client.socket.getaddrinfo", wraps=shc.socket.getaddrinfo) as spy:
            shc._resolve_validated_ip("api.github.com", allow_private_network=False)
            spy.assert_called_once()


class _FakeHTTPResponse:
    def __init__(self, status: int, headers: dict, body: bytes):
        self.status = status
        self._headers = headers
        self.body = body

    def getheader(self, name):
        return self._headers.get(name)

    def read(self, _n=-1):
        return self.body


class _FakePinnedConnection:
    """Replaces `_PinnedHTTPSConnection` so `_request_once`'s own
    validate-then-pin logic runs for real while no real socket or TLS
    handshake happens. `responses` maps `(hostname, path)` to a canned
    `_FakeHTTPResponse`; `calls` records every `(hostname, pinned_ip,
    path)` this fixture was asked to serve, for assertions."""

    responses: dict[tuple[str, str], "_FakeHTTPResponse"] = {}
    calls: list[tuple[str, str, str]] = []

    def __init__(self, hostname: str, pinned_ip: str, port: int, *, timeout: float):
        self._hostname = hostname
        self._pinned_ip = pinned_ip
        self._path = None

    def putrequest(self, _method, path):
        self._path = path

    def putheader(self, _key, _value):
        pass

    def endheaders(self, _body=None):
        _FakePinnedConnection.calls.append((self._hostname, self._pinned_ip, self._path))

    def getresponse(self):
        return _FakePinnedConnection.responses[(self._hostname, self._path)]

    def close(self):
        pass


class _PinnedTransportTest(unittest.TestCase):
    """Base class wiring `_PinnedHTTPSConnection` and DNS resolution to the
    fake fixture above for every test in this group, and restoring the
    real implementations afterward."""

    def setUp(self):
        _FakePinnedConnection.responses = {}
        _FakePinnedConnection.calls = []
        self._real_connection_cls = shc._PinnedHTTPSConnection
        self._real_resolve_all = shc._resolve_all
        shc._PinnedHTTPSConnection = _FakePinnedConnection
        shc._resolve_all = self._fake_resolve_all

    def tearDown(self):
        shc._PinnedHTTPSConnection = self._real_connection_cls
        shc._resolve_all = self._real_resolve_all

    # Default DNS fixture: every hostname this test group uses resolves
    # to a single public-looking address, except one reserved for the
    # "redirect targets a private address" case.
    _DNS = {
        "api.example.invalid": ["93.184.216.34"],
        "redirect-target.example.invalid": ["93.184.216.34"],
        "internal.example.invalid": ["10.0.0.5"],
    }

    def _fake_resolve_all(self, hostname):
        return self._DNS[hostname]


class RequestOnceTest(_PinnedTransportTest):
    def test_connects_with_host_header_identity_not_the_pinned_ip(self):
        _FakePinnedConnection.responses[("api.example.invalid", "/x")] = _FakeHTTPResponse(200, {}, b'{"ok":true}')
        status, location, raw = shc._request_once(
            "GET",
            "https://api.example.invalid/x",
            headers={},
            body=None,
            timeout_secs=1.0,
            allow_private_network=False,
        )
        self.assertEqual(status, 200)
        self.assertIsNone(location)
        self.assertEqual(raw, b'{"ok":true}')
        self.assertEqual(_FakePinnedConnection.calls, [("api.example.invalid", "93.184.216.34", "/x")])

    def test_refuses_a_target_that_resolves_private_before_ever_connecting(self):
        with self.assertRaises(shc.UnsafeBaseUrlError):
            shc._request_once(
                "GET",
                "https://internal.example.invalid/x",
                headers={},
                body=None,
                timeout_secs=1.0,
                allow_private_network=False,
            )
        self.assertEqual(_FakePinnedConnection.calls, [])

    def test_oversized_response_is_rejected(self):
        oversized = b"x" * (shc.MAX_RESPONSE_BYTES + 1)
        _FakePinnedConnection.responses[("api.example.invalid", "/x")] = _FakeHTTPResponse(200, {}, oversized)
        with self.assertRaises(ValueError):
            shc._request_once(
                "GET",
                "https://api.example.invalid/x",
                headers={},
                body=None,
                timeout_secs=1.0,
                allow_private_network=False,
            )


class RequestWithValidatedRedirectsTest(_PinnedTransportTest):
    def test_follows_a_redirect_to_an_allowed_target(self):
        _FakePinnedConnection.responses[("api.example.invalid", "/x")] = _FakeHTTPResponse(
            302, {"Location": "https://redirect-target.example.invalid/y"}, b""
        )
        _FakePinnedConnection.responses[("redirect-target.example.invalid", "/y")] = _FakeHTTPResponse(
            200, {}, b'{"ok":true}'
        )
        status, raw = shc._request_with_validated_redirects(
            "GET", "https://api.example.invalid/x", headers={}, body=None, timeout_secs=1.0, allow_private_network=False
        )
        self.assertEqual(status, 200)
        self.assertEqual(raw, b'{"ok":true}')
        self.assertEqual(
            _FakePinnedConnection.calls,
            [
                ("api.example.invalid", "93.184.216.34", "/x"),
                ("redirect-target.example.invalid", "93.184.216.34", "/y"),
            ],
        )

    def test_refuses_to_follow_a_redirect_whose_target_resolves_private(self):
        # This is the redirect-bypass class: the INITIAL URL is fine, but
        # its Location header points somewhere unsafe. The loop must
        # re-run full validation on that hop before connecting to it,
        # never trust the first URL's validation to cover it.
        _FakePinnedConnection.responses[("api.example.invalid", "/x")] = _FakeHTTPResponse(
            302, {"Location": "https://internal.example.invalid/y"}, b""
        )
        with self.assertRaises(shc.UnsafeBaseUrlError):
            shc._request_with_validated_redirects(
                "GET",
                "https://api.example.invalid/x",
                headers={},
                body=None,
                timeout_secs=1.0,
                allow_private_network=False,
            )
        # Only the first, legitimate hop was ever actually connected to.
        self.assertEqual(_FakePinnedConnection.calls, [("api.example.invalid", "93.184.216.34", "/x")])

    def test_a_303_response_switches_a_post_to_a_get_with_no_body(self):
        _FakePinnedConnection.responses[("api.example.invalid", "/x")] = _FakeHTTPResponse(
            303, {"Location": "https://redirect-target.example.invalid/y"}, b""
        )
        _FakePinnedConnection.responses[("redirect-target.example.invalid", "/y")] = _FakeHTTPResponse(200, {}, b"ok")
        status, raw = shc._request_with_validated_redirects(
            "POST",
            "https://api.example.invalid/x",
            headers={},
            body=b'{"a":1}',
            timeout_secs=1.0,
            allow_private_network=False,
        )
        self.assertEqual((status, raw), (200, b"ok"))

    def test_a_307_redirect_preserves_the_original_method(self):
        _FakePinnedConnection.responses[("api.example.invalid", "/x")] = _FakeHTTPResponse(
            307, {"Location": "https://redirect-target.example.invalid/y"}, b""
        )
        _FakePinnedConnection.responses[("redirect-target.example.invalid", "/y")] = _FakeHTTPResponse(
            200, {}, b"created"
        )
        status, raw = shc._request_with_validated_redirects(
            "POST",
            "https://api.example.invalid/x",
            headers={},
            body=b'{"a":1}',
            timeout_secs=1.0,
            allow_private_network=False,
        )
        self.assertEqual((status, raw), (200, b"created"))

    def test_too_many_redirects_raises_rather_than_looping_forever(self):
        _FakePinnedConnection.responses[("api.example.invalid", "/x")] = _FakeHTTPResponse(
            302, {"Location": "https://api.example.invalid/x"}, b""
        )
        with self.assertRaises(shc.UnsafeBaseUrlError):
            shc._request_with_validated_redirects(
                "GET",
                "https://api.example.invalid/x",
                headers={},
                body=None,
                timeout_secs=1.0,
                allow_private_network=False,
                max_redirects=2,
            )


if __name__ == "__main__":
    unittest.main()
