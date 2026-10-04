#!/usr/bin/env python3
"""Build Libra's static product surface from its canonical repository docs."""

from __future__ import annotations

import base64
import hashlib
import html
import re
import shutil
import subprocess
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

INLINE_LINK = re.compile(r"(?P<prefix>!?)\[(?P<label>[^]]+)\]\((?P<target>[^\s)]+)(?P<title>\s+[^)]*)?\)")
REFERENCE_LINK = re.compile(r"^(?P<prefix>\s*\[[^]]+\]:\s*)(?P<target>\S+)(?P<suffix>.*)$", re.MULTILINE)
SCRIPT_BODY = re.compile(r"<script(?:\s[^>]*)?>(.*?)</script>", re.IGNORECASE | re.DOTALL)


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
    def inline(match: re.Match[str]) -> str:
        target = repository_target(source_path, match.group("target"))
        title = match.group("title") or ""
        return f'{match.group("prefix")}[{match.group("label")}]({target}{title})'

    def reference(match: re.Match[str]) -> str:
        target = repository_target(source_path, match.group("target"))
        return f'{match.group("prefix")}{target}{match.group("suffix")}'

    return REFERENCE_LINK.sub(reference, INLINE_LINK.sub(inline, markdown))


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
        for body in SCRIPT_BODY.findall(page.read_text(encoding="utf-8")):
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
