#!/usr/bin/env python3
"""Tests for `libra_provider_runtime.py` (HORO-1173). Exercises the real
HMAC/replay/loopback logic end to end against a real `ThreadingHTTPServer`
bound to 127.0.0.1 on an OS-assigned port — no network call leaves the
machine, matching this repo's "prefer real behavior over a mock" convention
already used by `libra_example_provider.py`'s own test suite.
"""

from __future__ import annotations

import hashlib
import hmac
import http.client
import json
import threading
import time
import unittest
import uuid

import libra_provider_runtime as runtime

SECRET = b"test-secret-not-a-real-credential"


def _sign(secret: bytes, timestamp: int, nonce: str, body: bytes) -> str:
    payload = f"{runtime.SIGNATURE_VERSION}.{timestamp}.{nonce}.".encode("utf-8") + body
    mac = hmac.new(secret, payload, hashlib.sha256).hexdigest()
    return f"{runtime.SIGNATURE_VERSION}={mac}"


class _EchoHandler(runtime.BaseProviderHandler):
    secret = SECRET
    replay_guard = runtime.ReplayGuard(skew_secs=120.0)
    max_clock_skew_secs = 120.0
    provider_id = "test"

    def route_table(self):
        return {"/echo": self._handle_echo}

    def _handle_echo(self, request: dict) -> None:
        self._respond_json(200, {"echo": request})


class RequireLoopbackHostTest(unittest.TestCase):
    def test_accepts_ipv4_loopback_literal(self):
        runtime.require_loopback_host("127.0.0.1", program_name="test")

    def test_accepts_ipv6_loopback_literal(self):
        runtime.require_loopback_host("::1", program_name="test")

    def test_rejects_a_wildcard_bind_address(self):
        with self.assertRaises(SystemExit):
            runtime.require_loopback_host("0.0.0.0", program_name="test")

    def test_rejects_a_hostname(self):
        with self.assertRaises(SystemExit):
            runtime.require_loopback_host("localhost", program_name="test")


class RequireReadableFileTest(unittest.TestCase):
    def test_rejects_a_nonexistent_path(self):
        with self.assertRaises(SystemExit):
            runtime.require_readable_file("/definitely/does/not/exist", "secret-file", program_name="test")

    def test_accepts_this_test_file_itself(self):
        resolved = runtime.require_readable_file(__file__, "secret-file", program_name="test")
        self.assertTrue(resolved.is_file())


class ReplayGuardTest(unittest.TestCase):
    def test_first_use_of_a_nonce_is_accepted(self):
        guard = runtime.ReplayGuard(skew_secs=60.0)
        self.assertTrue(guard.check_and_record_nonce("n1"))

    def test_second_use_of_the_same_nonce_is_rejected(self):
        guard = runtime.ReplayGuard(skew_secs=60.0)
        guard.check_and_record_nonce("n1")
        self.assertFalse(guard.check_and_record_nonce("n1"))

    def test_expired_nonce_entries_are_pruned_and_reusable(self):
        guard = runtime.ReplayGuard(skew_secs=0.01)
        guard.check_and_record_nonce("n1")
        time.sleep(0.05)
        self.assertTrue(guard.check_and_record_nonce("n1"))

    def test_event_id_dedup_is_independent_of_nonce_dedup(self):
        guard = runtime.ReplayGuard(skew_secs=60.0)
        self.assertTrue(guard.check_and_record_event_id("e1"))
        self.assertFalse(guard.check_and_record_event_id("e1"))


class _ServerFixture:
    def __enter__(self):
        self.server = runtime.ThreadingHTTPServer(("127.0.0.1", 0), _EchoHandler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        return self.server.server_address

    def __exit__(self, *exc):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)
        return False


class HandlerHttpTest(unittest.TestCase):
    def _post(self, address, path, body: bytes, headers: dict) -> http.client.HTTPResponse:
        conn = http.client.HTTPConnection(address[0], address[1], timeout=5)
        conn.request("POST", path, body=body, headers=headers)
        response = conn.getresponse()
        response.read_body = response.read()
        return response

    def _signed_headers(self, body: bytes, *, timestamp=None, nonce=None, schema_version=None, secret=SECRET):
        timestamp = int(time.time()) if timestamp is None else timestamp
        nonce = nonce or str(uuid.uuid4())
        schema_version = schema_version or runtime.SCHEMA_VERSION
        return {
            "Content-Length": str(len(body)),
            "x-libra-schema-version": schema_version,
            "x-libra-request-id": str(uuid.uuid4()),
            "x-libra-timestamp": str(timestamp),
            "x-libra-nonce": nonce,
            "x-libra-signature": _sign(secret, timestamp, nonce, body),
        }

    def test_correctly_signed_request_is_accepted_and_routed(self):
        with _ServerFixture() as address:
            body = json.dumps({"hello": "world"}).encode("utf-8")
            response = self._post(address, "/echo", body, self._signed_headers(body))
            self.assertEqual(response.status, 200)
            self.assertEqual(json.loads(response.read_body), {"echo": {"hello": "world"}})

    def test_wrong_secret_is_rejected_with_401(self):
        with _ServerFixture() as address:
            body = b"{}"
            headers = self._signed_headers(body, secret=b"wrong-secret")
            response = self._post(address, "/echo", body, headers)
            self.assertEqual(response.status, 401)

    def test_replayed_nonce_is_rejected_with_409(self):
        with _ServerFixture() as address:
            body = b"{}"
            nonce = str(uuid.uuid4())
            headers = self._signed_headers(body, nonce=nonce)
            first = self._post(address, "/echo", body, headers)
            self.assertEqual(first.status, 200)
            second = self._post(address, "/echo", body, self._signed_headers(body, nonce=nonce))
            self.assertEqual(second.status, 409)

    def test_stale_timestamp_outside_skew_window_is_rejected_with_401(self):
        with _ServerFixture() as address:
            body = b"{}"
            headers = self._signed_headers(body, timestamp=int(time.time()) - 10_000)
            response = self._post(address, "/echo", body, headers)
            self.assertEqual(response.status, 401)

    def test_unsupported_schema_version_is_rejected_with_400(self):
        with _ServerFixture() as address:
            body = b"{}"
            headers = self._signed_headers(body, schema_version="libra.extension.v99")
            response = self._post(address, "/echo", body, headers)
            self.assertEqual(response.status, 400)

    def test_unknown_route_is_404(self):
        with _ServerFixture() as address:
            body = b"{}"
            response = self._post(address, "/not-a-real-route", body, self._signed_headers(body))
            self.assertEqual(response.status, 404)

    def test_malformed_json_body_is_rejected_with_400_after_signature_passes(self):
        with _ServerFixture() as address:
            body = b"not json"
            response = self._post(address, "/echo", body, self._signed_headers(body))
            self.assertEqual(response.status, 400)


if __name__ == "__main__":
    unittest.main()
