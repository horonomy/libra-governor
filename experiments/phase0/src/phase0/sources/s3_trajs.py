"""Lists and fetches SWE-agent trajectory files from the public S3 bucket.

The bucket `swe-bench-submissions` allows anonymous GET; trajectories live
under `verified/<submission>/trajs/<instance_id>.traj`. Full .traj files run
into the tens-of-GB range across all required submissions (~3.8 GB total for
the 6 required ones as of the design review's live probe), and 99% of that
is conversation transcript text we do not need and must never commit. So we
never cache whole raw blobs to disk: each file is streamed into memory,
parsed once, and only the small `info.model_stats` / `info.exit_status`
slice survives -- cached by the object's ETag under data/raw/distilled/ so a
second `fetch` run is a no-op unless S3 content actually changed.
"""

from __future__ import annotations

import json
import xml.etree.ElementTree as ET
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import requests

BUCKET_URL = "https://swe-bench-submissions.s3.amazonaws.com/"
S3_NS = "{http://s3.amazonaws.com/doc/2006-03-01/}"


@dataclass(frozen=True)
class S3Object:
    key: str
    etag: str
    size: int


def list_prefix(prefix: str) -> list[S3Object]:
    """List all objects under a prefix, following S3 ListObjectsV2 pagination."""
    objects: list[S3Object] = []
    continuation_token: str | None = None
    while True:
        params: dict[str, str] = {"list-type": "2", "prefix": prefix}
        if continuation_token:
            params["continuation-token"] = continuation_token
        resp = requests.get(BUCKET_URL, params=params, timeout=30)
        resp.raise_for_status()
        # Note: this XML is ListObjectsV2 output from AWS S3 itself (fixed
        # https://swe-bench-submissions.s3.amazonaws.com/ endpoint), not
        # attacker-controlled input, so stdlib ElementTree's XXE exposure is
        # not applicable here. Not swapping to defusedxml to avoid adding a
        # new dependency for a non-reachable threat model.
        root = ET.fromstring(resp.text)
        for content in root.findall(f"{S3_NS}Contents"):
            key = content.findtext(f"{S3_NS}Key")
            etag = (content.findtext(f"{S3_NS}ETag") or "").strip('"')
            size = int(content.findtext(f"{S3_NS}Size") or "0")
            if key:
                objects.append(S3Object(key=key, etag=etag, size=size))
        is_truncated = (root.findtext(f"{S3_NS}IsTruncated") or "false").lower() == "true"
        if not is_truncated:
            break
        continuation_token = root.findtext(f"{S3_NS}NextContinuationToken")
        if not continuation_token:
            break
    return objects


def instance_id_from_key(key: str) -> str:
    return Path(key).stem


def _distilled_cache_path(cache_dir: Path, submission: str, instance_id: str) -> Path:
    return cache_dir / "distilled" / submission / f"{instance_id}.json"


def fetch_distilled_traj(
    obj: S3Object, submission: str, cache_dir: Path
) -> dict[str, Any] | None:
    """Download one .traj object, extract cost/exit-status fields, discard the body.

    Returns None if the object has no info.model_stats (unusable for cost
    replay -- caller should skip it, not fabricate a record).
    """
    instance_id = instance_id_from_key(obj.key)
    cache_path = _distilled_cache_path(cache_dir, submission, instance_id)
    if cache_path.exists():
        cached = json.loads(cache_path.read_text(encoding="utf-8"))
        if cached.get("_etag") == obj.etag:
            return cached

    url = BUCKET_URL + obj.key
    resp = requests.get(url, timeout=60)
    resp.raise_for_status()
    try:
        traj = json.loads(resp.text)
    except json.JSONDecodeError:
        return None

    info = traj.get("info", {}) if isinstance(traj, dict) else {}
    model_stats = info.get("model_stats")
    if not model_stats:
        return None

    distilled = {
        "_etag": obj.etag,
        "instance_id": instance_id,
        "submission": submission,
        "exit_status": info.get("exit_status"),
        "model_stats": model_stats,
    }
    cache_path.parent.mkdir(parents=True, exist_ok=True)
    cache_path.write_text(json.dumps(distilled), encoding="utf-8")
    return distilled


def load_cached_distilled(cache_dir: Path, submission: str) -> list[dict[str, Any]]:
    sub_dir = cache_dir / "distilled" / submission
    if not sub_dir.exists():
        return []
    return [json.loads(p.read_text(encoding="utf-8")) for p in sorted(sub_dir.glob("*.json"))]
