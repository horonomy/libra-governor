"""Loads SWE-bench Verified outcome labels and task metadata.

Two network sources, both public and requiring no auth token:
- SWE-bench/experiments GitHub repo: per-submission results.json under
  evaluation/verified/<submission>/results/results.json, giving the
  resolved / no_generation / no_logs instance-id lists.
- princeton-nlp/SWE-bench_Verified on HuggingFace: task metadata
  (repo, problem_statement, created_at, difficulty, ...) via the
  `datasets` library, no auth required for this public dataset.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any

import requests

RESULTS_JSON_URL = (
    "https://raw.githubusercontent.com/SWE-bench/experiments/main/"
    "evaluation/verified/{submission}/results/results.json"
)


def fetch_results_json(submission: str, cache_dir: Path) -> dict[str, list[str]]:
    """Fetch and cache a submission's results.json (resolved/no_generation/no_logs)."""
    cache_dir.mkdir(parents=True, exist_ok=True)
    cache_path = cache_dir / f"{submission}.results.json"
    if cache_path.exists():
        return json.loads(cache_path.read_text(encoding="utf-8"))

    url = RESULTS_JSON_URL.format(submission=submission)
    resp = requests.get(url, timeout=30)
    resp.raise_for_status()
    data = resp.json()
    cache_path.write_text(json.dumps(data), encoding="utf-8")
    return data


def outcome_label(results: dict[str, list[str]], instance_id: str) -> dict[str, Any]:
    """Return {"resolved": bool, "has_generation": bool, "has_logs": bool} for one instance."""
    resolved_set = set(results.get("resolved", []))
    no_generation_set = set(results.get("no_generation", []))
    no_logs_set = set(results.get("no_logs", []))
    return {
        "resolved": instance_id in resolved_set,
        "has_generation": instance_id not in no_generation_set,
        "has_logs": instance_id not in no_logs_set,
    }


def fetch_task_metadata(cache_dir: Path) -> "list[dict[str, Any]]":
    """Fetch princeton-nlp/SWE-bench_Verified task metadata via the datasets library.

    Cached to cache_dir/swebench_verified_metadata.json after first fetch.
    """
    cache_dir.mkdir(parents=True, exist_ok=True)
    cache_path = cache_dir / "swebench_verified_metadata.json"
    if cache_path.exists():
        return json.loads(cache_path.read_text(encoding="utf-8"))

    from datasets import load_dataset

    ds = load_dataset("princeton-nlp/SWE-bench_Verified", split="test")
    records = [dict(row) for row in ds]
    cache_path.write_text(json.dumps(records), encoding="utf-8")
    return records
