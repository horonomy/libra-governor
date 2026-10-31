#!/usr/bin/env python3
"""Regression tests for the link-rewriting patterns in `build_site.py` (Sonar python:S8786).

The previous patterns are kept below as literals so the rewrite stays provably
behaviour-preserving, and a bounded-time check keeps the quadratic backtracking
from coming back. Stdlib only.

Run with: python3 tools/test_build_site.py -v
"""

from __future__ import annotations

import re
import sys
import time
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))

import build_site

OLD_INLINE_LINK = re.compile(
    r"(?P<prefix>!?)\[(?P<label>[^]]+)\]\((?P<target>[^\s)]+)(?P<title>\s+[^)]*)?\)"
)
OLD_REFERENCE_LINK = re.compile(
    r"^(?P<prefix>\s*\[[^]]+\]:\s*)(?P<target>\S+)(?P<suffix>.*)$", re.MULTILINE
)

INLINE_CASES = [
    "[a](b)",
    "![img](pic.png)",
    '[a](b "title")',
    "[a](b   'two  spaces')",
    "[a](b\t\"tab\")",
    "[a](b\n\"newline title\")",
    "[a](b )",
    "[a](b \n )",
    "[a](b(c))",
    "[a](b (c))",
    "[a](b \"t (x)\")",
    "[a](#frag) [b](../x.md#y 'z') ![c](d)",
    "[a]()",
    "[a]( b)",
    "[a](b",
    "[a](b ",
    "[[nested]](x)",
    "[a](b)\r\n[c](d \"e\")\r\n",
    "[a](b \"unicode space\")",
]

REFERENCE_CASES = [
    "[a]: https://example.com",
    "[a]: https://example.com \"Title\"",
    "  [a]: ../x.md",
    "\t[a]: ../x.md",
    "[a]:\n   ../x.md",
    "[a]:\n\n\n  ../x.md  trailing",
    "[a]:",
    "[a]: ",
    "text\n[a]: x\n[b]: y z\n",
    "text\n\n\n[a]: x\n",
    "text   \n  \n\t\n  [a]: x\n",
    "[a]: x\r\n[b]: y\r\n",
    "\r\n\r\n  [a]: x\r\n",
    "[a]:\n[b]: y",
    "    [a]: indented code-ish\n",
    "[a b]: spaced label",
    "[a\nb]: multiline label",
    "\n\n",
    "   ",
]


def _repository_markdown() -> list[Path]:
    skipped = {"build", "target", ".git", ".claude", "node_modules"}
    return sorted(
        path
        for path in build_site.ROOT.rglob("*.md")
        if not skipped.intersection(path.relative_to(build_site.ROOT).parts)
    )


def _blank_prefix_ok(text: str, old: re.Match[str], new: re.Match[str]) -> bool:
    """The old prefix may only add whole leading blank lines to the new one."""
    extra = text[old.start() : new.start()]
    return (
        old.start() <= new.start()
        and extra.strip() == ""
        and (extra == "" or extra.endswith("\n"))
        and old.group("prefix")[len(extra) :] == new.group("prefix")
    )


class InlineLinkEquivalence(unittest.TestCase):
    def assert_same(self, text: str) -> None:
        old = [(m.span(), m.groupdict()) for m in OLD_INLINE_LINK.finditer(text)]
        new = [(m.span(), m.groupdict()) for m in build_site.INLINE_LINK.finditer(text)]
        self.assertEqual(old, new, repr(text[:200]))

    def test_hand_written_cases(self) -> None:
        for text in INLINE_CASES:
            with self.subTest(text=text):
                self.assert_same(text)

    def test_repository_markdown(self) -> None:
        files = _repository_markdown()
        self.assertTrue(files)
        for path in files:
            with self.subTest(path=path.name):
                self.assert_same(path.read_text(encoding="utf-8"))

    def test_every_prefix_of_cases_agrees(self) -> None:
        for text in INLINE_CASES:
            for end in range(len(text) + 1):
                self.assert_same(text[:end])


class ReferenceLinkEquivalence(unittest.TestCase):
    def assert_same(self, text: str) -> None:
        old = list(OLD_REFERENCE_LINK.finditer(text))
        new = list(build_site.REFERENCE_LINK.finditer(text))
        self.assertEqual(len(old), len(new), repr(text[:200]))
        for o, n in zip(old, new):
            self.assertEqual(o.end(), n.end())
            self.assertEqual(o.span("target"), n.span("target"))
            self.assertEqual(o.span("suffix"), n.span("suffix"))
            self.assertTrue(_blank_prefix_ok(text, o, n), repr(text[:200]))

    def test_hand_written_cases(self) -> None:
        for text in REFERENCE_CASES:
            with self.subTest(text=text):
                self.assert_same(text)

    def test_repository_markdown(self) -> None:
        for path in _repository_markdown():
            with self.subTest(path=path.name):
                self.assert_same(path.read_text(encoding="utf-8"))

    def test_every_prefix_of_cases_agrees(self) -> None:
        for text in REFERENCE_CASES:
            for end in range(len(text) + 1):
                self.assert_same(text[:end])


class RewriteLinksOutput(unittest.TestCase):
    """The rewritten Markdown (what the site is built from) must not change."""

    def old_rewrite(self, text: str, source: str) -> str:
        with mock.patch.object(build_site, "INLINE_LINK", OLD_INLINE_LINK), mock.patch.object(
            build_site, "REFERENCE_LINK", OLD_REFERENCE_LINK
        ):
            return build_site.rewrite_links(text, source)

    def test_hand_written_cases(self) -> None:
        for text in INLINE_CASES + REFERENCE_CASES:
            with self.subTest(text=text):
                self.assertEqual(
                    self.old_rewrite(text, "docs/x.md"),
                    build_site.rewrite_links(text, "docs/x.md"),
                )

    def test_chapters(self) -> None:
        for _, canonical, _ in build_site.CHAPTERS:
            text = (build_site.ROOT / canonical).read_text(encoding="utf-8")
            with self.subTest(chapter=canonical):
                self.assertEqual(
                    self.old_rewrite(text, canonical),
                    build_site.rewrite_links(text, canonical),
                )


class PathologicalInputsStayFast(unittest.TestCase):
    """Each input took minutes with the old patterns (quadratic); now ~10 ms.

    The 3 s cap is a very generous multiple so a slow CI runner cannot flake.
    """

    SIZE = 200_000
    CAP_SECONDS = 3.0

    def assert_fast(self, pattern: re.Pattern[str], text: str) -> None:
        started = time.perf_counter()
        list(pattern.finditer(text))
        self.assertLess(time.perf_counter() - started, self.CAP_SECONDS)

    def test_inline_link_whitespace_run_in_title(self) -> None:
        self.assert_fast(build_site.INLINE_LINK, "[a](b" + " " * self.SIZE)

    def test_reference_link_blank_lines(self) -> None:
        self.assert_fast(build_site.REFERENCE_LINK, "\n" * self.SIZE + "x")

    def test_reference_link_crlf_blank_lines(self) -> None:
        self.assert_fast(build_site.REFERENCE_LINK, "\r\n" * self.SIZE + "x")

    def test_reference_link_whitespace_only_lines(self) -> None:
        self.assert_fast(build_site.REFERENCE_LINK, "  \n" * self.SIZE + "x")


if __name__ == "__main__":
    unittest.main()
