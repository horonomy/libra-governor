#!/usr/bin/env python3
"""Check shared safety and discoverability invariants in a static site build."""

from __future__ import annotations

import argparse
import html
import ipaddress
import json
import re
import sys
import xml.etree.ElementTree as ET
from dataclasses import dataclass
from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import ParseResult, parse_qsl, unquote, urljoin, urlparse

SCANNED_SUFFIXES = {".css", ".html", ".js", ".json", ".map", ".txt", ".xml"}
SECRET = re.compile(
    r"(?:ghp_[A-Za-z0-9_-]{20,}|sk-[A-Za-z0-9]{20,}|AKIA[0-9A-Z]{16}|"
    r"-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----)"
)
EXECUTABLE_URL = re.compile(
    r"(?ix)(?:"
    r"(?:fetch|WebSocket|EventSource)\s*\(\s*[\"']((?:https?:)?//[^\"']+)|"
    r"(?:origin|base[_-]?url|api[_-]?url|endpoint)\s*[:=]\s*[\"']((?:https?:)?//[^\"']+)|"
    r"url\(\s*[\"']?((?:https?:)?//[^\"')]+)"
    r")"
)
SENSITIVE_QUERY_KEYS = {"access_token", "api_key", "apikey", "email", "password", "secret", "token"}
TRAVERSAL_ERROR = "URL contains path traversal"
MAX_SITEMAP_CHARS = 1_000_000
UNSAFE_XML_DECLARATION = re.compile(r"<!\s*(?:DOCTYPE|ENTITY)\b", re.IGNORECASE)
FORBIDDEN_ANALYTICS_TERMS = {
    "authenticated",
    "code",
    "email",
    "evidence",
    "prompt",
    "repo",
    "repository",
    "secret",
    "security",
    "tenant",
    "token",
}
SENSITIVE_ASSIGNMENT = re.compile(
    r"(?i)\b(?:[a-z0-9]+[_-])*(?:prompt|email|tenant|authenticated_content|"
    r"api[_-]?key|access[_-]?token)(?:[_-][a-z0-9]+)*\s*[:=]"
)
INDEX_NAME = "index.html"
INDEX_HTML = Path(INDEX_NAME)


@dataclass(frozen=True)
class Finding:
    rule: str
    detail: str


@dataclass
class HtmlFacts:
    canonicals: list[str]
    links: list[tuple[str, str, str]]
    anchors: list[str]
    ids: set[str]
    noindex: bool = False


class _HtmlFactsParser(HTMLParser):
    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.facts = HtmlFacts([], [], [], set())

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        values = {key.lower(): value for key, value in attrs if value is not None}
        if "id" in values:
            self.facts.ids.add(values["id"])
        if tag.lower() == "a" and "name" in values:
            self.facts.ids.add(values["name"])
        normalized_tag = tag.lower()
        for attribute in ("href", "src"):
            if attribute in values:
                self.facts.links.append((values[attribute], normalized_tag, attribute))
        if normalized_tag == "a" and "href" in values:
            self.facts.anchors.append(values["href"])
        rel = {item.lower() for item in values.get("rel", "").split()}
        if tag.lower() == "link" and "canonical" in rel and "href" in values:
            self.facts.canonicals.append(values["href"])
        if normalized_tag == "meta" and values.get("name", "").lower() == "robots":
            directives = re.split(r"[\s,]+", values.get("content", "").lower())
            self.facts.noindex = self.facts.noindex or "noindex" in directives


def _decode_url(value: str) -> str:
    decoded = html.unescape(value.strip())
    for _ in range(4):
        expanded = unquote(decoded)
        if expanded == decoded:
            break
        decoded = expanded
    return decoded


def _has_parent_segment(path: str) -> bool:
    return any(part == ".." for part in path.split("/"))


def _input_url_error(raw_value: str, parsed_input: ParseResult) -> str:
    if parsed_input.scheme and parsed_input.scheme.lower() != "https":
        return "network URLs must use HTTPS"
    if parsed_input.scheme and not parsed_input.netloc:
        return "absolute URL is missing a host"
    if parsed_input.username or parsed_input.password:
        return "credential-bearing URL"
    if "\\" in parsed_input.path:
        return TRAVERSAL_ERROR
    raw_path = urlparse(html.unescape(raw_value.strip())).path
    if _has_parent_segment(parsed_input.path) and (
        parsed_input.netloc or not _has_parent_segment(raw_path)
    ):
        return TRAVERSAL_ERROR
    return ""


def _host_is_public(hostname: str | None) -> bool:
    if not hostname:
        return False
    host = hostname.rstrip(".").lower()
    if host == "localhost" or host.endswith((".localhost", ".internal", ".local")):
        return False
    if host == "run.app" or host.endswith(".run.app"):
        return False
    try:
        return ipaddress.ip_address(host).is_global
    except ValueError:
        try:
            ascii_host = host.encode("idna").decode("ascii")
        except UnicodeError:
            return False
        labels = ascii_host.split(".")
        return (
            len(ascii_host) <= 253
            and len(labels) >= 2
            and all(
                re.fullmatch(r"[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?", label) for label in labels
            )
        )


def _network_url(value: object, base_url: str) -> tuple[str | None, str]:
    """Return a normalized public HTTP(S) URL without echoing rejected input."""
    if not isinstance(value, str) or not value.strip():
        return None, "URL must be a non-empty string"
    decoded = _decode_url(value)
    if decoded.startswith("//") or any(ord(character) < 32 for character in decoded):
        return None, "URL uses an ambiguous or invalid form"
    try:
        parsed_input = urlparse(decoded)
        input_error = _input_url_error(value, parsed_input)
        if input_error:
            return None, input_error
        resolved = urlparse(urljoin(base_url, decoded))
        _ = resolved.port
    except ValueError:
        return None, "malformed URL"
    if resolved.scheme != "https" or not _host_is_public(resolved.hostname):
        return None, "URL exposes a non-public origin"
    if "\\" in resolved.path or _has_parent_segment(resolved.path):
        return None, TRAVERSAL_ERROR
    query_keys = {
        key.lower().replace("-", "_")
        for key, _ in parse_qsl(resolved.query, keep_blank_values=True)
    }
    if query_keys.intersection(SENSITIVE_QUERY_KEYS):
        return None, "URL contains a sensitive query parameter"
    return resolved.geturl(), ""


def _same_identity(url: str, base_url: str) -> bool:
    candidate, base = urlparse(url), urlparse(base_url)
    return (
        candidate.scheme == base.scheme
        and candidate.hostname == base.hostname
        and candidate.port == base.port
    )


def _inside_base_path(url: str, base_url: str) -> bool:
    path = urlparse(url).path
    prefix = urlparse(base_url).path.rstrip("/") + "/"
    return path == prefix.rstrip("/") or path.startswith(prefix)


def _parse_html(text: str) -> HtmlFacts:
    parser = _HtmlFactsParser()
    parser.feed(text)
    parser.close()
    return parser.facts


def _artifact_files(root: Path, findings: list[Finding]) -> tuple[list[Path], dict[Path, str]]:
    paths: list[Path] = []
    texts: dict[Path, str] = {}
    resolved_root = root.resolve()
    for path in sorted(root.rglob("*")):
        if not path.is_file() or path.suffix.lower() not in SCANNED_SUFFIXES:
            continue
        try:
            path.resolve(strict=True).relative_to(resolved_root)
        except (OSError, ValueError):
            findings.append(
                Finding("artifact-boundary", f"{path.relative_to(root)} escapes the output root")
            )
            continue
        paths.append(path)
        texts[path] = path.read_text(encoding="utf-8", errors="replace")
    return paths, texts


def _relative_public_path(url: str, base_url: str) -> str | None:
    if not _same_identity(url, base_url) or not _inside_base_path(url, base_url):
        return None
    base_path = urlparse(base_url).path.rstrip("/") + "/"
    return unquote(urlparse(url).path)[len(base_path) :].lstrip("/")


def _target_file(root: Path, url: str, base_url: str) -> Path | None:
    relative = _relative_public_path(url, base_url)
    if relative is None:
        return None
    requested = root / relative
    candidates = [requested]
    if not relative or relative.endswith("/"):
        candidates.append(requested / INDEX_NAME)
    elif not Path(relative).suffix:
        candidates.extend((root / f"{relative}.html", requested / INDEX_NAME))
    for candidate in candidates:
        try:
            candidate.resolve(strict=True).relative_to(root.resolve())
        except (OSError, ValueError):
            continue
        if candidate.is_file():
            return candidate
    return None


def _page_url(path: Path, root: Path, base_url: str) -> str:
    relative = path.relative_to(root).as_posix()
    if relative == INDEX_NAME:
        return base_url
    if relative.endswith(f"/{INDEX_NAME}"):
        relative = relative[: -len(INDEX_NAME)]
    return urljoin(base_url, relative)


def _validate_internal_target(
    raw: str,
    tag: str,
    attribute: str,
    document_url: str,
    base_url: str,
    root: Path,
    html_facts: dict[Path, HtmlFacts],
) -> str | None:
    scheme = urlparse(html.unescape(raw.strip())).scheme.lower()
    if (
        scheme == "data"
        and tag == "img"
        and attribute == "src"
        and raw.strip().lower().startswith("data:image/")
    ):
        return None
    if scheme in {"mailto", "tel"}:
        return None
    if scheme and scheme not in {"http", "https"}:
        return "link uses an unsupported URL scheme"
    normalized, reason = _network_url(raw, document_url)
    if normalized is None:
        return reason
    if not _same_identity(normalized, base_url):
        return None
    target = _target_file(root, normalized, base_url)
    if target is None:
        return "same-site target is absent from the artifact"
    fragment = unquote(urlparse(normalized).fragment)
    raw_fragment = urlparse(urljoin(document_url, html.unescape(raw.strip()))).fragment
    target_ids = html_facts.get(target, HtmlFacts([], [], [], set())).ids
    if fragment and fragment not in target_ids and raw_fragment not in target_ids:
        return "same-site fragment target is absent"
    return None


def _validate_base(manifest: dict, findings: list[Finding]) -> str | None:
    raw = manifest.get("base_url")
    normalized, reason = _network_url(raw, "https://invalid.example/")
    if normalized is None:
        findings.append(Finding("canonical-identity", f"base_url rejected: {reason}"))
        return None
    parsed = urlparse(normalized)
    if (
        not isinstance(raw, str)
        or not raw.lower().startswith("https://")
        or parsed.query
        or parsed.fragment
    ):
        findings.append(
            Finding(
                "canonical-identity",
                "base_url must be an absolute HTTPS URL without query or fragment",
            )
        )
        return None
    return normalized.rstrip("/") + "/"


def _validate_docs(manifest: dict, base_url: str, root: Path, findings: list[Finding]) -> None:
    if "docs_url" not in manifest or manifest["docs_url"] is None:
        return
    normalized, reason = _network_url(manifest["docs_url"], base_url)
    if normalized is None:
        findings.append(Finding("docs-link", f"docs_url rejected: {reason}"))
    elif _same_identity(normalized, base_url) and _target_file(root, normalized, base_url) is None:
        findings.append(Finding("docs-link", "same-site docs_url is absent from the artifact"))


def _scan_artifacts(
    paths: list[Path], texts: dict[Path, str], root: Path, base_url: str, findings: list[Finding]
) -> dict[Path, HtmlFacts]:
    facts: dict[Path, HtmlFacts] = {}
    for path in paths:
        rel, text = path.relative_to(root), texts[path]
        if SECRET.search(text):
            findings.append(Finding("secret-scan", str(rel)))
        executable_urls = (item for match in EXECUTABLE_URL.findall(text) for item in match if item)
        for candidate in executable_urls:
            _, reason = _network_url(candidate, base_url)
            if reason:
                rule = (
                    "private-origin"
                    if reason == "URL exposes a non-public origin"
                    else "executable-url"
                )
                findings.append(Finding(rule, f"{rel}: {reason}"))
        if path.suffix.lower() == ".html":
            facts[path] = _parse_html(text)
    return facts


def _canonical_finding(
    page: HtmlFacts, document_url: str, base_url: str, rel: Path
) -> Finding | None:
    if page.noindex and not page.canonicals:
        return None
    normalized = [_network_url(item, document_url)[0] for item in page.canonicals]
    valid = [
        item
        for item in normalized
        if item and _same_identity(item, base_url) and _inside_base_path(item, base_url)
    ]
    if len(page.canonicals) != 1 or len(valid) != 1:
        return Finding("canonical-identity", f"{rel} must have one same-site canonical link")
    if valid[0] != document_url:
        return Finding("canonical-identity", f"{rel} canonical does not match its public URL")
    return None


def _validate_html(
    root: Path, base_url: str, facts: dict[Path, HtmlFacts], findings: list[Finding]
) -> None:
    if root / INDEX_HTML not in facts:
        findings.append(Finding("artifact", "index.html is missing"))
    for path, page in facts.items():
        rel = path.relative_to(root)
        document_url = _page_url(path, root, base_url)
        canonical_finding = _canonical_finding(page, document_url, base_url, rel)
        if canonical_finding:
            findings.append(canonical_finding)
        for raw, tag, attribute in page.links:
            reason = _validate_internal_target(
                raw, tag, attribute, document_url, base_url, root, facts
            )
            if reason:
                findings.append(Finding("link-integrity", f"{rel}: {reason}"))


def _sitemap_location_is_invalid(location: str, base_url: str) -> tuple[bool, str | None]:
    normalized, _ = _network_url(location, base_url)
    if not normalized:
        return True, None
    parsed = urlparse(normalized)
    invalid = (
        not _same_identity(normalized, base_url)
        or not _inside_base_path(normalized, base_url)
        or bool(parsed.query or parsed.fragment)
    )
    return invalid, normalized


def _parse_sitemap(text: str, findings: list[Finding]) -> ET.Element | None:
    if len(text) > MAX_SITEMAP_CHARS:
        findings.append(Finding("robots-sitemap", "sitemap.xml exceeds the bounded parse limit"))
        return None
    if UNSAFE_XML_DECLARATION.search(text):
        findings.append(
            Finding("robots-sitemap", "sitemap.xml contains a forbidden XML declaration")
        )
        return None
    try:
        # S314 is safe here: size and all DTD/entity declarations are rejected above.
        return ET.fromstring(text)  # noqa: S314
    except ET.ParseError:
        findings.append(Finding("robots-sitemap", "sitemap.xml is malformed XML"))
        return None


def _validate_discovery(
    root: Path, base_url: str, texts: dict[Path, str], findings: list[Finding]
) -> None:
    robots, sitemap = root / "robots.txt", root / "sitemap.xml"
    if robots not in texts or sitemap not in texts:
        findings.append(Finding("robots-sitemap", "robots.txt and sitemap.xml are mandatory"))
        return
    expected = urljoin(base_url, "sitemap.xml")
    declarations = [
        line.split(":", 1)[1].strip()
        for line in texts[robots].splitlines()
        if line.lower().startswith("sitemap:")
    ]
    if len(declarations) != 1 or _network_url(declarations[0], base_url)[0] != expected:
        findings.append(
            Finding(
                "robots-sitemap", "robots.txt must declare the deployed sitemap URL exactly once"
            )
        )
    tree = _parse_sitemap(texts[sitemap], findings)
    if tree is None:
        return
    locations = [
        node.text.strip()
        for node in tree.iter()
        if node.tag.rsplit("}", 1)[-1] == "loc" and node.text
    ]
    if not locations:
        findings.append(Finding("robots-sitemap", "sitemap.xml has no loc entries"))
    for location in locations:
        invalid, normalized = _sitemap_location_is_invalid(location, base_url)
        if invalid:
            findings.append(
                Finding("robots-sitemap", "sitemap.xml contains a foreign or invalid URL")
            )
        elif normalized and _target_file(root, normalized, base_url) is None:
            findings.append(
                Finding("robots-sitemap", "sitemap.xml references an absent artifact page")
            )


def _validate_navigation(
    manifest: dict, base_url: str, root: Path, facts: dict[Path, HtmlFacts], findings: list[Finding]
) -> None:
    required = manifest.get("required_navigation", [])
    if not isinstance(required, list) or not all(isinstance(item, str) for item in required):
        findings.append(Finding("navigation", "required_navigation must be a list of URL strings"))
        return
    index = facts.get(root / INDEX_HTML, HtmlFacts([], [], [], set()))
    anchors = {_network_url(item, base_url)[0] for item in index.anchors}
    for item in required:
        normalized, reason = _network_url(item, base_url)
        if normalized is None:
            findings.append(Finding("navigation", f"required target rejected: {reason}"))
        elif normalized not in anchors:
            findings.append(
                Finding("navigation", "required target is absent from index.html anchors")
            )


def _validate_analytics(manifest: dict, texts: dict[Path, str], findings: list[Finding]) -> None:
    analytics = manifest.get("analytics")
    if not isinstance(analytics, dict) or not isinstance(analytics.get("enabled"), bool):
        findings.append(
            Finding("privacy-analytics", "analytics must declare a boolean enabled value")
        )
        return
    allowed_keys = {"enabled", "payload_fields"}
    if set(analytics) - allowed_keys:
        findings.append(
            Finding("privacy-analytics", "analytics contains unsupported manifest fields")
        )
    if not analytics["enabled"]:
        return
    fields = analytics.get("payload_fields")
    if (
        not isinstance(fields, list)
        or not fields
        or not all(isinstance(item, str) for item in fields)
    ):
        findings.append(Finding("privacy-analytics", "enabled analytics must list payload_fields"))
        return
    tokens = {token for field in fields for token in re.split(r"[^a-z]+", field.lower()) if token}
    if tokens.intersection(FORBIDDEN_ANALYTICS_TERMS):
        findings.append(
            Finding("privacy-analytics", "analytics declares a forbidden payload field")
        )
    if any(SENSITIVE_ASSIGNMENT.search(text) for text in texts.values()):
        findings.append(
            Finding("privacy-analytics", "artifact contains a sensitive analytics field assignment")
        )


def validate(manifest: dict, root: Path) -> list[Finding]:
    """Return deterministic findings; an empty list means the artifact passes."""
    findings: list[Finding] = []
    if not isinstance(manifest, dict):
        return [Finding("manifest", "manifest root must be an object")]
    allowed_manifest_keys = {"analytics", "base_url", "docs_url", "required_navigation"}
    if set(manifest) - allowed_manifest_keys:
        findings.append(Finding("manifest", "manifest contains unsupported fields"))
    base_url = _validate_base(manifest, findings)
    paths, texts = _artifact_files(root, findings)
    if not paths:
        findings.append(Finding("artifact", "no supported artifacts found"))
    if base_url is None:
        return findings
    _validate_docs(manifest, base_url, root, findings)
    facts = _scan_artifacts(paths, texts, root, base_url, findings)
    _validate_html(root, base_url, facts, findings)
    _validate_discovery(root, base_url, texts, findings)
    _validate_navigation(manifest, base_url, root, facts, findings)
    _validate_analytics(manifest, texts, findings)
    return findings


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("manifest", type=Path)
    parser.add_argument("artifact_root", type=Path)
    args = parser.parse_args(argv)
    manifest_path = args.manifest.resolve(strict=True)
    artifact_root = args.artifact_root.resolve(strict=True)
    if not manifest_path.is_file() or not artifact_root.is_dir():
        parser.error("manifest must be a file and artifact_root must be a directory")
    findings = validate(json.loads(manifest_path.read_text(encoding="utf-8")), artifact_root)
    for finding in findings:
        print(f"FAIL [{finding.rule}] {finding.detail}")
    if not findings:
        print("PASS public surface invariants")
    return 1 if findings else 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
