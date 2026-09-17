"""Extract pipeline: distilled trajectory records + SWE-bench metadata -> DataFrame.

Builds the row schema committed to data/interim/trajectories.parquet. This
module must be import-safe with no network calls at import time (CI imports
it against a committed fixture parquet only) -- network calls only happen
inside function bodies invoked by `phase0 fetch` / `phase0 extract`.
"""

from __future__ import annotations

from pathlib import Path
from typing import Any

import pandas as pd

from phase0 import features
from phase0.adapters import get_adapter
from phase0.config import Config, all_submissions
from phase0.sources import s3_trajs, swebench

# Columns that MUST NEVER enter fit() -- kept in the committed parquet only
# as reference/reporting data (metrics, censoring, oracle-probe baseline).
BANNED_FROM_FIT = {
    "patch",
    "test_patch",
    "FAIL_TO_PASS",
    "PASS_TO_PASS",
    "difficulty",
    "resolved",
    "n_steps",
    "api_calls",
    "exit_status",
}


def is_censored(exit_status: str) -> bool:
    exit_status = exit_status or ""
    return "exit_cost" in exit_status or "exit_context" in exit_status


def proxy_wall_clock_seconds(
    api_calls: int, tokens_sent: int, tokens_received: int, cfg: Config
) -> float:
    """Documented, non-measured proxy: base_seconds_per_call * api_calls
    + seconds_per_1k_tokens * (tokens_sent + tokens_received) / 1000.
    See configs/phase0.yaml `proxy_time` and PROVENANCE.md.
    """
    proxy_cfg = cfg["proxy_time"]
    base = proxy_cfg["base_seconds_per_call"]
    per_1k = proxy_cfg["seconds_per_1k_tokens"]
    return base * api_calls + per_1k * (tokens_sent + tokens_received) / 1000.0


def fetch_all(cfg: Config, raw_dir: Path, include_optional: bool = False) -> None:
    """Network step: list S3 prefixes, download+distill trajs, fetch results.json + HF metadata."""
    for sub in all_submissions(cfg, include_optional=include_optional):
        name = sub["name"]
        objects = s3_trajs.list_prefix(f"verified/{name}/trajs/")
        for obj in objects:
            s3_trajs.fetch_distilled_traj(obj, name, raw_dir)
        swebench.fetch_results_json(name, raw_dir / "results")
    swebench.fetch_task_metadata(raw_dir / "metadata")


def extract_dataframe(cfg: Config, raw_dir: Path, include_optional: bool = False) -> pd.DataFrame:
    """Network-free: distill data/raw/ into the committed trajectories DataFrame.

    No raw problem_statement text is retained -- only derived numeric/boolean
    counts (features.derive_text_features).
    """
    task_metadata = swebench.fetch_task_metadata(raw_dir / "metadata")
    meta_by_id: dict[str, dict[str, Any]] = {row["instance_id"]: row for row in task_metadata}

    rows: list[dict[str, Any]] = []
    for sub in all_submissions(cfg, include_optional=include_optional):
        name = sub["name"]
        adapter = get_adapter(sub["adapter"])
        results = swebench.fetch_results_json(name, raw_dir / "results")
        distilled_records = s3_trajs.load_cached_distilled(raw_dir, name)

        for record in distilled_records:
            normalized = adapter(record)
            if normalized is None:
                continue
            instance_id = normalized["instance_id"]
            meta = meta_by_id.get(instance_id)
            if meta is None:
                continue

            outcome = swebench.outcome_label(results, instance_id)
            text_feats = features.derive_text_features(meta.get("problem_statement", ""))
            exit_status = normalized["exit_status"] or ""

            row = {
                "instance_id": instance_id,
                "submission": name,
                "repo": meta.get("repo", "unknown"),
                "difficulty": meta.get("difficulty"),
                "created_at_year_month": features.created_at_year_month(meta.get("created_at", "")),
                "model_id": sub["model_id"],
                "harness_id": sub["harness_id"],
                **text_feats,
                "instance_cost_usd": normalized["instance_cost_usd"],
                "target_kind_cost": "measured",
                "api_calls": normalized["api_calls"],
                "tokens_sent": normalized["tokens_sent"],
                "tokens_received": normalized["tokens_received"],
                "wall_clock_seconds": proxy_wall_clock_seconds(
                    normalized["api_calls"],
                    normalized["tokens_sent"],
                    normalized["tokens_received"],
                    cfg,
                ),
                "target_kind_wall_clock": "proxy",
                "exit_status": exit_status,
                "censored": is_censored(exit_status),
                "resolved": bool(outcome["resolved"]),
                "has_generation": bool(outcome["has_generation"]),
                "has_logs": bool(outcome["has_logs"]),
            }
            rows.append(row)

    df = pd.DataFrame(rows)
    if not df.empty:
        df["row_id"] = df["submission"].astype(str) + "::" + df["instance_id"].astype(str)
    return df
