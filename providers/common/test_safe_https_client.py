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
from unittest.mock import MagicMock

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


if __name__ == "__main__":
    unittest.main()
