#!/usr/bin/env python3
"""Build Libra's static product surface from its canonical repository docs."""

from __future__ import annotations

import base64
import hashlib
import html
import re
import shutil
import subprocess
from collections.abc import Callable, Iterable, Iterator
from html.parser import HTMLParser
from pathlib import Path, PurePosixPath
from urllib.parse import urljoin, urlsplit, urlunsplit


ROOT = Path(__file__).resolve().parents[1]
SITE = ROOT / "site"
BUILD = ROOT / "build"
BOOK_SOURCE = BUILD / "book-src"
OUTPUT = BUILD / "site"
BASE_URL = "https://libra.horonom.com/"
REPOSITORY_BLOB = "https://github.com/horonomy/libra-governor/blob/main/"
MDBOOK_VERSION = "mdbook v0.5.2"

CHAPTERS: tuple[tuple[str, str, str], ...] = (
    ("Developer Preview guide", "README.md", "developer-preview.md"),
    ("Product North Star", "PRODUCT.md", "north-star.md"),
    ("Architecture", "ARCHITECTURE.md", "architecture.md"),
    ("Claude Code integration", "integrations/claude-code/README.md", "claude-code.md"),
    ("Codex CLI integration", "integrations/codex/README.md", "codex.md"),
    ("Composable statusline", "docs/statusline.md", "statusline.md"),
    ("Security and supported versions", "SECURITY.md", "security.md"),
)


class _Next:
    """First index >= p where `find(text, p)` hits, remembered between queries.

    Queries arrive in non-decreasing order, so a cached hit (or a cached miss) answers
    every later query without rescanning; that is what keeps the scanners below linear.
    """

    def __init__(self, text: str, find: Callable[[str, int], int]) -> None:
        self._text = text
        self._find = find
        self._from = -1
        self._hit = -1

    def __call__(self, p: int) -> int:
        if self._from < 0 or p < self._from or (self._hit >= 0 and p > self._hit):
            self._from = p
            self._hit = self._find(self._text, p)
        return self._hit


_STOP = re.compile(r"[\s)]")  # whitespace or ")"
_NON_SPACE = re.compile(r"\S")


def _search(pattern: re.Pattern[str]) -> Callable[[str, int], int]:
    def find(text: str, p: int) -> int:
        match = pattern.search(text, p)
        return match.start() if match else -1

    return find


def inline_links(text: str) -> Iterator[tuple[int, int, str, str, str, str]]:
    """Yield (start, end, prefix, label, target, title) for `!?[label](target title?)`.

    Single left-to-right pass with the exact semantics of the former regex
    `(!?)\\[([^]]+)\\]\\(([^\\s)]+)(\\s[^)]*)?\\)` under `finditer`, but each failed
    candidate reuses the cached next-"]" / next-stop / next-")" lookups instead of
    rescanning to the end of the input (the regex was quadratic: python:S8786).
    """

    next_close = _Next(text, lambda t, p: t.find("]", p))
    next_paren = _Next(text, lambda t, p: t.find(")", p))
    next_stop = _Next(text, _search(_STOP))
    tails: dict[int, tuple[int, str, str] | None] = {}
    size = len(text)
    pos = 0
    while True:
        j = text.find("[", pos)
        if j < 0:
            return
        k = next_close(j + 1)
        if k < 0:
            return  # no "]" anywhere later: no further candidate can match
        if k == j + 1 or text[k + 1 : k + 2] != "(":
            pos = j + 1
            continue
        if k not in tails:
            tails.clear()
            e = next_stop(k + 2)
            if e < 0 or e == k + 2:
                tails[k] = None  # empty target, or target runs to the end of the input
            elif text[e] == ")":
                tails[k] = (e + 1, text[k + 2 : e], "")
            else:  # whitespace after the target: title runs to the next ")"
                q = next_paren(e)
                tails[k] = None if q < 0 else (q + 1, text[k + 2 : e], text[e:q])
        tail = tails[k]
        if tail is None:
            pos = j + 1
            continue
        end, target, title = tail
        start = j - 1 if j > pos and text[j - 1] == "!" else j
        yield start, end, text[start:j], text[j + 1 : k], target, title
        pos = end
        if pos >= size:
            return


def reference_links(text: str) -> Iterator[tuple[int, int, str, str, str]]:
    """Yield (start, end, prefix, target, suffix) for a `[label]: target rest` line.

    Exact semantics of the former `^(\\s*\\[[^]]+\\]:\\s*)(\\S+)(.*)$` under
    re.MULTILINE: the prefix may begin at an earlier blank line and the target may sit
    on a following line. Line starts inside one whitespace run all reach the same
    "[" and so succeed or fail together; a failure skips the whole run.
    """

    next_non_space = _Next(text, _search(_NON_SPACE))
    next_close = _Next(text, lambda t, p: t.find("]", p))
    size = len(text)
    p = 0
    while p <= size:
        w = next_non_space(p)
        if w < 0 or text[w] != "[":
            if w < 0:
                return
            nl = text.find("\n", w)
            if nl < 0:
                return
            p = nl + 1
            continue
        k = next_close(w + 1)
        if k < 0:
            return
        u = -1
        if k > w + 1 and text[k + 1 : k + 2] == ":":
            u = next_non_space(k + 2)
        if u < 0:
            nl = text.find("\n", w)
            if nl < 0:
                return
            p = nl + 1
            continue
        v = u
        while v < size and not text[v].isspace():
            v += 1
        nl = text.find("\n", v)
        end = size if nl < 0 else nl
        yield p, end, text[p:u], text[u:v], text[v:end]
        if nl < 0:
            return
        p = nl + 1


def _splice(text: str, replacements: Iterable[tuple[int, int, str]]) -> str:
    out: list[str] = []
    last = 0
    for start, end, replacement in replacements:
        out.append(text[last:start])
        out.append(replacement)
        last = end
    out.append(text[last:])
    return "".join(out)


class InlineScriptParser(HTMLParser):
    """Collect exact inline script bodies for the deployed CSP."""

    def __init__(self) -> None:
        super().__init__(convert_charrefs=False)
        self.bodies: list[str] = []
        self._body: list[str] | None = None

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        if tag.lower() == "script" and not any(name.lower() == "src" for name, _ in attrs):
            self._body = []

    def handle_data(self, data: str) -> None:
        if self._body is not None:
            self._body.append(data)

    def handle_endtag(self, tag: str) -> None:
        if tag.lower() == "script" and self._body is not None:
            self.bodies.append("".join(self._body))
            self._body = None

    @property
    def has_unclosed_script(self) -> bool:
        return self._body is not None


def run(*command: str) -> None:
    subprocess.run(command, cwd=ROOT, check=True)


def verify_tools() -> None:
    version = subprocess.run(
        ["mdbook", "--version"], cwd=ROOT, check=True, capture_output=True, text=True
    ).stdout.strip()
    if version != MDBOOK_VERSION:
        raise RuntimeError(f"expected {MDBOOK_VERSION}, found {version}")

    expected = (ROOT / "tools/public_surface.sha256").read_text(encoding="utf-8").split()[0]
    actual = hashlib.sha256((ROOT / "tools/public_surface.py").read_bytes()).hexdigest()
    if actual != expected:
        raise RuntimeError("public surface checker does not match its reviewed checksum")


def repository_target(source_path: str, raw_target: str) -> str:
    target = html.unescape(raw_target)
    parsed = urlsplit(target)
    if parsed.scheme or parsed.netloc or target.startswith(("#", "/")):
        return raw_target

    source_parent = PurePosixPath(source_path).parent
    resolved = PurePosixPath(source_parent, parsed.path)
    parts: list[str] = []
    for part in resolved.parts:
        if part in ("", "."):
            continue
        if part == "..":
            if parts:
                parts.pop()
            continue
        parts.append(part)
    normalized = "/".join(parts)

    local_chapters = {canonical: generated for _, canonical, generated in CHAPTERS}
    if normalized in local_chapters:
        destination = local_chapters[normalized]
    else:
        destination = urljoin(REPOSITORY_BLOB, normalized)
    return urlunsplit(("", "", destination, parsed.query, parsed.fragment))


def rewrite_links(markdown: str, source_path: str) -> str:
    inline = (
        (start, end, f"{prefix}[{label}]({repository_target(source_path, target)}{title})")
        for start, end, prefix, label, target, title in inline_links(markdown)
    )
    rewritten = _splice(markdown, inline)
    reference = (
        (start, end, f"{prefix}{repository_target(source_path, target)}{suffix}")
        for start, end, prefix, target, suffix in reference_links(rewritten)
    )
    return _splice(rewritten, reference)


def prepare_book() -> None:
    if BUILD.exists():
        shutil.rmtree(BUILD)
    BOOK_SOURCE.mkdir(parents=True)
    shutil.copy2(SITE / "guide.md", BOOK_SOURCE / "index.md")

    summary = ["# Summary", "", "- [Start here](index.md)"]
    for title, canonical, generated in CHAPTERS:
        source = ROOT / canonical
        if not source.is_file():
            raise FileNotFoundError(f"canonical documentation is missing: {canonical}")
        rendered = rewrite_links(source.read_text(encoding="utf-8"), canonical)
        (BOOK_SOURCE / generated).write_text(rendered, encoding="utf-8")
        summary.append(f"- [{title}]({generated})")
    (BOOK_SOURCE / "SUMMARY.md").write_text("\n".join(summary) + "\n", encoding="utf-8")


def canonical_for(page: Path) -> str:
    relative = page.relative_to(OUTPUT).as_posix()
    if relative == "index.html":
        return BASE_URL
    if relative == "docs/index.html":
        return urljoin(BASE_URL, "docs/")
    return urljoin(BASE_URL, relative)


def inject_metadata() -> list[str]:
    locations: list[str] = []
    for page in sorted(OUTPUT.rglob("*.html")):
        source = page.read_text(encoding="utf-8")
        if page.name == "404.html":
            metadata = '<meta name="robots" content="noindex">'
        else:
            canonical = canonical_for(page)
            locations.append(canonical)
            metadata = f'<link rel="canonical" href="{canonical}">'
        if "rel=\"canonical\"" not in source and "name=\"robots\" content=\"noindex\"" not in source:
            source = source.replace("</head>", f"    {metadata}\n</head>", 1)
            page.write_text(source, encoding="utf-8")
    return locations


def script_hashes() -> list[str]:
    hashes: set[str] = set()
    for page in OUTPUT.rglob("*.html"):
        parser = InlineScriptParser()
        parser.feed(page.read_text(encoding="utf-8"))
        parser.close()
        if parser.has_unclosed_script:
            raise RuntimeError(f"unclosed inline script in {page.relative_to(OUTPUT)}")
        for body in parser.bodies:
            if not body.strip():
                continue
            digest = base64.b64encode(hashlib.sha256(body.encode("utf-8")).digest()).decode("ascii")
            hashes.add(f"'sha256-{digest}'")
    return sorted(hashes)


def write_deployment_files(locations: list[str]) -> None:
    hashes = " ".join(script_hashes())
    csp = (
        "default-src 'self'; "
        f"script-src 'self' {hashes}; "
        "style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self'; "
        "connect-src 'self'; object-src 'none'; base-uri 'none'; "
        "frame-ancestors 'none'; form-action 'none'"
    )
    headers = (
        "/*\n"
        f"  Content-Security-Policy: {csp}\n"
        "  Referrer-Policy: no-referrer\n"
        "  X-Content-Type-Options: nosniff\n"
        "  Permissions-Policy: camera=(), microphone=(), geolocation=()\n"
    )
    (OUTPUT / "_headers").write_text(headers, encoding="utf-8")
    (OUTPUT / "robots.txt").write_text(
        f"User-agent: *\nAllow: /\nSitemap: {urljoin(BASE_URL, 'sitemap.xml')}\n",
        encoding="utf-8",
    )
    entries = "".join(f"  <url><loc>{location}</loc></url>\n" for location in locations)
    sitemap = f'<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n{entries}</urlset>\n'
    (OUTPUT / "sitemap.xml").write_text(sitemap, encoding="utf-8")


def build() -> None:
    verify_tools()
    prepare_book()
    run("mdbook", "build", "site")
    shutil.copy2(SITE / "index.html", OUTPUT / "index.html")
    shutil.copy2(SITE / "style.css", OUTPUT / "style.css")
    locations = inject_metadata()
    write_deployment_files(locations)
    run("python3", "tools/public_surface.py", "site/public-surface.json", "build/site")


if __name__ == "__main__":
    build()
