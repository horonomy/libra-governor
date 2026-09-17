"""Freeze/load GroupKFold(5) splits, content-hashed for "frozen before tuning".

Two named split configs, both GroupKFold(n_splits=5):
- cold_start: grouped on `repo` -- a repo's rows never straddle train/test.
- history_assisted: grouped on `instance_id` -- the same task never
  straddles train/test, but different models' runs of the same repo's
  tasks CAN appear on both sides, which is what gives repo_prior_* history
  features any signal.

`run` must hard-error, not warn, if a split file is missing or its
live-computed hash doesn't match the sidecar hash -- this is what "frozen
before tuning" actually enforces.
"""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
from typing import Any

import numpy as np
import pandas as pd
from sklearn.model_selection import GroupKFold

SPLIT_CONFIGS = {
    "cold_start": "repo",
    "history_assisted": "instance_id",
}


def _content_hash(payload: dict[str, Any]) -> str:
    blob = json.dumps(payload, sort_keys=True).encode("utf-8")
    return hashlib.sha256(blob).hexdigest()


def _build_split_payload(df: pd.DataFrame, group_col: str, n_splits: int = 5) -> dict[str, Any]:
    groups = df[group_col].to_numpy()
    row_ids = df["row_id"].tolist()
    gkf = GroupKFold(n_splits=n_splits)
    folds = []
    for train_idx, test_idx in gkf.split(df, groups=groups):
        folds.append(
            {
                "train_row_ids": [row_ids[i] for i in train_idx],
                "test_row_ids": [row_ids[i] for i in test_idx],
            }
        )
    return {"group_col": group_col, "n_splits": n_splits, "folds": folds}


def freeze_split(df: pd.DataFrame, name: str, out_dir: Path) -> Path:
    """Compute and write data/splits/<name>.json with a sidecar content hash."""
    if name not in SPLIT_CONFIGS:
        raise ValueError(f"Unknown split config '{name}'. Known: {sorted(SPLIT_CONFIGS)}")
    group_col = SPLIT_CONFIGS[name]
    if "row_id" not in df.columns:
        df = df.copy()
        df["row_id"] = df.index.astype(str)

    payload = _build_split_payload(df, group_col)
    digest = _content_hash(payload)
    payload["content_sha256"] = digest

    out_dir.mkdir(parents=True, exist_ok=True)
    out_path = out_dir / f"{name}.json"
    out_path.write_text(json.dumps(payload, indent=2, sort_keys=True), encoding="utf-8")
    return out_path


def load_frozen_split(name: str, splits_dir: Path) -> dict[str, Any]:
    """Load a frozen split file, hard-erroring if missing or hash mismatched."""
    path = splits_dir / f"{name}.json"
    if not path.exists():
        raise FileNotFoundError(
            f"Frozen split file {path} does not exist. Run "
            f"`phase0 freeze-splits` before `phase0 run` -- splits must be "
            f"frozen before tuning, this is not optional."
        )
    payload = json.loads(path.read_text(encoding="utf-8"))
    recorded_hash = payload.pop("content_sha256", None)
    live_hash = _content_hash(payload)
    if recorded_hash != live_hash:
        raise ValueError(
            f"Frozen split file {path} content hash mismatch "
            f"(recorded={recorded_hash}, live={live_hash}). The split file "
            f"was modified after freezing -- re-run `phase0 freeze-splits` "
            f"deliberately if this is intended; do not hand-edit split files."
        )
    payload["content_sha256"] = recorded_hash
    return payload


def carve_calibration_slice(
    train_row_ids: list[str],
    df: pd.DataFrame,
    group_col: str,
    calibration_fraction: float,
    seed: int,
) -> tuple[list[str], list[str]]:
    """Split a training fold's row ids into (fit_row_ids, calibration_row_ids).

    Carved by the same grouping logic as the outer split (whole groups go to
    one side or the other), so calibration data never leaks the same group
    into both fit and calibration.
    """
    train_df = df[df["row_id"].isin(train_row_ids)]
    groups = train_df[group_col].unique()
    rng = np.random.default_rng(seed)
    rng.shuffle(groups)
    n_cal_groups = max(1, int(round(len(groups) * calibration_fraction)))
    cal_groups = set(groups[:n_cal_groups])

    cal_mask = train_df[group_col].isin(cal_groups)
    calibration_row_ids = train_df.loc[cal_mask, "row_id"].tolist()
    fit_row_ids = train_df.loc[~cal_mask, "row_id"].tolist()
    return fit_row_ids, calibration_row_ids
