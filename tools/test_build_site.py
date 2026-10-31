#!/usr/bin/env python3
"""Regression tests for the link scanners in `build_site.py` (Sonar python:S8786).

The original regexes are kept below as literals (the oracle): the linear scanners that
replaced them must return exactly the same spans and groups for every input, and a
bounded-time check keeps super-linear rescanning from coming back. Stdlib only.

Run with: python3 tools/test_build_site.py -v
"""

from __future__ import annotations

import random
import re
import sys
import time
import unittest
from pathlib import Path
from typing import ClassVar

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


def old_inline(text: str) -> list[tuple[int, int, str, str, str, str]]:
    return [
        (m.start(), m.end(), m["prefix"], m["label"], m["target"], m["title"] or "")
        for m in OLD_INLINE_LINK.finditer(text)
    ]


def old_reference(text: str) -> list[tuple[int, int, str, str, str]]:
    return [
        (m.start(), m.end(), m["prefix"], m["target"], m["suffix"])
        for m in OLD_REFERENCE_LINK.finditer(text)
    ]


FUZZ_ALPHABET = ["[", "]", "(", ")", ":", " ", "\n", "\r", "\t", "!", "a", "x", '"', "\u2003", "\x85", "\x1f", "](", "]:", "[a", "(b", "\n\n", "[a]: ", "![", "[a](b", "[a](b ", ")"]


def fuzz_corpus(count: int = 60_000) -> list[str]:
    rng = random.Random(8786)  # fixed seed: deterministic
    return ["".join(rng.choices(FUZZ_ALPHABET, k=rng.randint(0, 16))) for _ in range(count)]


class LinkScannerEquivalence(unittest.TestCase):
    """Scanners vs the original regexes: identical spans and groups, no restrictions."""

    def check(self, text: str) -> None:
        self.assertEqual(old_inline(text), list(build_site.inline_links(text)), repr(text))
        self.assertEqual(old_reference(text), list(build_site.reference_links(text)), repr(text))

    def test_hand_written_cases(self) -> None:
        for text in INLINE_CASES + REFERENCE_CASES:
            with self.subTest(text=text):
                self.check(text)

    def test_every_prefix_of_cases(self) -> None:
        for text in INLINE_CASES + REFERENCE_CASES:
            for end in range(len(text) + 1):
                self.check(text[:end])

    def test_multiline_labels_titles_and_next_line_targets_are_kept(self) -> None:
        self.assertEqual(len(list(build_site.inline_links("[a\nb](c)"))), 1)
        self.assertEqual(len(list(build_site.inline_links('[a](c "t\nu")'))), 1)
        self.assertEqual(len(list(build_site.reference_links("[a]:\n\n  c"))), 1)
        self.assertEqual(len(list(build_site.reference_links("\n\n[a\nb]: c"))), 1)

    def test_random_inputs(self) -> None:
        for text in fuzz_corpus():
            self.check(text)

    def test_repository_markdown(self) -> None:
        files = _repository_markdown()
        self.assertTrue(files)
        for path in files:
            with self.subTest(path=path.name):
                self.check(path.read_text(encoding="utf-8"))


class RewriteLinksOutput(unittest.TestCase):
    """The rewritten Markdown (what the site is built from) must not change."""

    @staticmethod
    def old_rewrite(text: str, source: str) -> str:
        def inline(m: re.Match[str]) -> str:
            target = build_site.repository_target(source, m["target"])
            return f'{m["prefix"]}[{m["label"]}]({target}{m["title"] or ""})'

        def reference(m: re.Match[str]) -> str:
            target = build_site.repository_target(source, m["target"])
            return f'{m["prefix"]}{target}{m["suffix"]}'

        return OLD_REFERENCE_LINK.sub(reference, OLD_INLINE_LINK.sub(inline, text))

    def test_hand_written_and_random_cases(self) -> None:
        for text in INLINE_CASES + REFERENCE_CASES + fuzz_corpus(5_000):
            self.assertEqual(
                self.old_rewrite(text, "docs/x.md"), build_site.rewrite_links(text, "docs/x.md"), repr(text)
            )

    def test_chapters(self) -> None:
        for _, canonical, _ in build_site.CHAPTERS:
            text = (build_site.ROOT / canonical).read_text(encoding="utf-8")
            with self.subTest(chapter=canonical):
                self.assertEqual(
                    self.old_rewrite(text, canonical), build_site.rewrite_links(text, canonical)
                )


class PathologicalInputsStayFast(unittest.TestCase):
    """Each input took seconds to minutes with the old regexes (quadratic); now ~10-100 ms.

    The 3 s cap is a very generous multiple so a slow CI runner cannot flake.
    """

    SIZE = 200_000
    CAP_SECONDS = 3.0
    CASES: ClassVar[dict[str, str]] = {
        "whitespace run in a title": "[a](b" + " " * SIZE,
        "blank lines": "\n" * SIZE + "x",
        "CRLF blank lines": "\r\n" * SIZE + "x",
        "whitespace-only lines": "  \n" * SIZE + "x",
        "repeated unclosed inline links": "[a](b " * SIZE,
        "repeated unclosed multi-line labels": "[a\n" * SIZE,
        "repeated unclosed one-line labels": "[a" * SIZE,
        "label start run then one target": "[" * SIZE + "](" + "b" * SIZE,
        "reference marker then whitespace": "[a]:" + " " * SIZE,
        "reference marker then blank lines": "[a]:" + "\n" * SIZE,
        "repeated reference markers": "[a]: " * SIZE,
        "repeated reference lines without target": "[a]:\n" * SIZE,
        "repeated reference lines with unclosed labels": "[a\n" * SIZE + "]: x",
    }

    def test_each_scanner_is_fast(self) -> None:
        for name, text in self.CASES.items():
            for scanner in (build_site.inline_links, build_site.reference_links):
                with self.subTest(case=name, scanner=scanner.__name__):
                    started = time.perf_counter()
                    list(scanner(text))
                    self.assertLess(time.perf_counter() - started, self.CAP_SECONDS)


if __name__ == "__main__":
    unittest.main()
