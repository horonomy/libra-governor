"""Admission-time feature whitelist -- derivation and enforcement.

Every feature here must be knowable BEFORE the agent runs. Nothing derived
from the outcome (patch, test_patch, FAIL_TO_PASS, PASS_TO_PASS, difficulty,
resolved, n_steps, api_calls, exit_status, or any other post-hoc field) may
ever reach `fit()` -- tests/test_leakage.py asserts this by comparing the
fitted model's actual feature-matrix columns against FEATURE_WHITELIST.
"""

from __future__ import annotations

import re

import numpy as np
import pandas as pd

FEATURE_WHITELIST: list[str] = [
    "repo",
    "problem_statement_char_len",
    "problem_statement_line_count",
    "problem_statement_code_block_count",
    "problem_statement_has_traceback",
    "problem_statement_filepath_token_count",
    "problem_statement_numeric_token_count",
    "created_at_year_month",
    "model_id",
    "harness_id",
    "repo_prior_median_cost",
    "repo_prior_n",
]

_CODE_FENCE_RE = re.compile(r"```")
_TRACEBACK_RE = re.compile(r"Traceback \(most recent call last\)")
_FILEPATH_TOKEN_RE = re.compile(r"\b[\w./-]+/[\w./-]+\.\w+\b")
_NUMERIC_TOKEN_RE = re.compile(r"\b\d+\b")


def derive_text_features(problem_statement: str) -> dict[str, int | bool]:
    """Simple regex/count-based stats over problem_statement text.

    No embeddings, no semantic NLP -- per spec, out of scope for Phase 0.
    """
    text = problem_statement or ""
    lines = text.splitlines()
    return {
        "problem_statement_char_len": len(text),
        "problem_statement_line_count": len(lines),
        "problem_statement_code_block_count": len(_CODE_FENCE_RE.findall(text)) // 2,
        "problem_statement_has_traceback": bool(_TRACEBACK_RE.search(text)),
        "problem_statement_filepath_token_count": len(_FILEPATH_TOKEN_RE.findall(text)),
        "problem_statement_numeric_token_count": len(_NUMERIC_TOKEN_RE.findall(text)),
    }


def created_at_year_month(created_at: str) -> str:
    """Truncate an ISO-ish created_at timestamp to 'YYYY-MM'."""
    if not created_at:
        return "unknown"
    return str(created_at)[:7]


def add_repo_prior_features(
    df: pd.DataFrame,
    train_mask: np.ndarray,
    target_col: str = "instance_cost_usd",
) -> pd.DataFrame:
    """Compute repo_prior_median_cost / repo_prior_n FROM TRAINING ROWS ONLY.

    Must be called per-fold: `train_mask` selects the rows whose target may
    be aggregated; every row in df (train and test) then gets the resulting
    per-repo median/count looked up, so test rows never contribute to their
    own (or any other repo's) prior. A repo with zero training rows (e.g.
    under `cold_start` splits, where a whole repo is held out) falls back to
    the global training-fold median with repo_prior_n = 0 -- this is the
    documented cold-start degradation, not a bug: it must never emit NaN
    into the candidate/knn models.
    """
    out = df.copy()
    train_df = df.loc[train_mask]
    per_repo = train_df.groupby("repo")[target_col].agg(["median", "count"])
    global_median = train_df[target_col].median() if len(train_df) else 0.0

    medians = out["repo"].map(per_repo["median"]).fillna(global_median)
    counts = out["repo"].map(per_repo["count"]).fillna(0).astype(int)

    out["repo_prior_median_cost"] = medians
    out["repo_prior_n"] = counts
    return out
