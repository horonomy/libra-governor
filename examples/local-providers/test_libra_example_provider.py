#!/usr/bin/env python3
"""Regression test for the loopback-only boundary that is this
provider's entire justification for speaking plain HTTP without TLS
(HORO-1174, python:S5332 accepted exception — see README.md and the
Jira evidence comment). `require_loopback_host` is the one gate between
`--host` and `ThreadingHTTPServer` actually binding a socket; this test
is the mechanical proof that gate does what the exception claims.

Stdlib only (`unittest`), matching this file's own dependency-free
design. Run directly:

    python3 -m unittest examples/local-providers/test_libra_example_provider.py
"""

from __future__ import annotations

import unittest

from libra_example_provider import LOOPBACK_LITERALS, require_loopback_host


class RequireLoopbackHostTests(unittest.TestCase):
    def test_accepts_every_loopback_literal(self):
        for host in LOOPBACK_LITERALS:
            with self.subTest(host=host):
                require_loopback_host(host)  # must not raise

    def test_rejects_all_interfaces(self):
        with self.assertRaises(SystemExit):
            require_loopback_host("0.0.0.0")

    def test_rejects_localhost_hostname(self):
        # "localhost" is deliberately not accepted: it can resolve
        # differently across hosts/resolvers, unlike a loopback literal.
        with self.assertRaises(SystemExit):
            require_loopback_host("localhost")

    def test_rejects_a_lan_address(self):
        with self.assertRaises(SystemExit):
            require_loopback_host("192.168.1.10")

    def test_rejects_empty_string(self):
        with self.assertRaises(SystemExit):
            require_loopback_host("")


if __name__ == "__main__":
    unittest.main()
