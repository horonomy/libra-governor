"""Baseline QuantileEstimator implementations.

- global_median: constant quantiles regardless of X.
- taskclass_repo: quantiles per-`repo` group -- the primary bar the
  candidate must beat.
- taskclass_difficulty: quantiles keyed on the human-annotated `difficulty`
  field. Tagged oracle_probe=True by callers -- it's a post-hoc field, not a
  legitimate admission-time baseline, kept only as an upper-bound reference.
- llm_self_estimate: reads a cached (instance_id, prompt_sha256, model_id)
  fixture; any cache miss makes that row `status: "unavailable"`, never a
  fabricated or silently-skipped prediction.
- knn_history: k=5 nearest neighbors on scaled whitelisted numeric features,
  restricted to the same model_id, empirical quantiles of the 5 neighbors.
"""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
from typing import Any

import numpy as np
import pandas as pd
from sklearn.neighbors import NearestNeighbors
from sklearn.preprocessing import StandardScaler


class GlobalMedianEstimator:
    name = "global_median"

    def __init__(self) -> None:
        self._quantile_fn: dict[float, float] = {}
        self._y: np.ndarray | None = None

    def fit(self, X: pd.DataFrame, y: np.ndarray) -> None:
        self._y = np.asarray(y, dtype=float)

    def predict_quantiles(self, X: pd.DataFrame, qs: list[float]) -> np.ndarray:
        assert self._y is not None, "call fit() first"
        values = [np.quantile(self._y, q) for q in qs]
        return np.tile(np.array(values), (len(X), 1))


class _GroupedQuantileEstimator:
    """Shared implementation for quantile-per-category baselines."""

    def __init__(self, group_col: str, name: str) -> None:
        self.name = name
        self._group_col = group_col
        self._group_values: dict[Any, np.ndarray] = {}
        self._global_values: np.ndarray = np.array([])

    def fit(self, X: pd.DataFrame, y: np.ndarray) -> None:
        y = np.asarray(y, dtype=float)
        self._global_values = y
        groups = X[self._group_col]
        self._group_values = {}
        for group, idx in groups.groupby(groups).groups.items():
            positions = [groups.index.get_loc(i) for i in idx]
            self._group_values[group] = y[positions]

    def predict_quantiles(self, X: pd.DataFrame, qs: list[float]) -> np.ndarray:
        out = np.zeros((len(X), len(qs)))
        groups = X[self._group_col].to_numpy()
        for i, group in enumerate(groups):
            values = self._group_values.get(group)
            if values is None or len(values) == 0:
                values = self._global_values
            out[i, :] = [np.quantile(values, q) for q in qs]
        return out


def make_taskclass_repo() -> _GroupedQuantileEstimator:
    return _GroupedQuantileEstimator(group_col="repo", name="taskclass_repo")


def make_taskclass_difficulty() -> _GroupedQuantileEstimator:
    # oracle_probe=True is applied by the caller when recording metrics rows;
    # the estimator itself doesn't know that -- it's just grouped quantiles
    # keyed on whatever column it's given.
    return _GroupedQuantileEstimator(group_col="difficulty", name="taskclass_difficulty")


class LLMSelfEstimateEstimator:
    """Reads a cached fixture keyed by (instance_id, prompt_sha256, model_id).

    Not a fitted model in the usual sense: fit() just remembers the fixture
    path; predict_quantiles() looks up each row and raises AvailabilityError
    per-row via the `unavailable_mask` output attribute the caller reads
    after predicting, rather than silently guessing.
    """

    name = "llm_self_estimate"

    def __init__(self, cache_path: Path, model_id: str) -> None:
        self._cache_path = cache_path
        self._model_id = model_id
        self._cache: dict[str, dict[str, Any]] = {}
        self.unavailable_mask: np.ndarray = np.array([])

    def fit(self, X: pd.DataFrame, y: np.ndarray) -> None:
        if self._cache_path.exists():
            self._cache = json.loads(self._cache_path.read_text(encoding="utf-8"))
        else:
            self._cache = {}

    @staticmethod
    def cache_key(instance_id: str, prompt_sha256: str, model_id: str) -> str:
        return f"{instance_id}|{prompt_sha256}|{model_id}"

    def predict_quantiles(self, X: pd.DataFrame, qs: list[float]) -> np.ndarray:
        out = np.full((len(X), len(qs)), np.nan)
        unavailable = np.zeros(len(X), dtype=bool)
        for i, (_, row) in enumerate(X.iterrows()):
            instance_id = row.get("instance_id", "")
            prompt_sha256 = hashlib.sha256(
                str(row.get("problem_statement_char_len", "")).encode("utf-8")
            ).hexdigest()
            key = self.cache_key(instance_id, prompt_sha256, self._model_id)
            entry = self._cache.get(key) or self._cache.get(instance_id)
            if entry is None:
                unavailable[i] = True
                continue
            out[i, :] = [entry.get(f"q{q}", np.nan) for q in qs]
        self.unavailable_mask = unavailable
        return out


class KnnHistoryEstimator:
    """k-NN (k=5) on scaled whitelisted numeric features, same model_id only."""

    name = "knn_history"

    def __init__(self, numeric_cols: list[str], k: int = 5) -> None:
        self._numeric_cols = numeric_cols
        self._k = k
        self._scaler = StandardScaler()
        self._nn: NearestNeighbors | None = None
        self._y: np.ndarray | None = None
        self._model_ids: np.ndarray | None = None

    def fit(self, X: pd.DataFrame, y: np.ndarray) -> None:
        Xn = X[self._numeric_cols].fillna(0.0).to_numpy(dtype=float)
        Xs = self._scaler.fit_transform(Xn)
        k = min(self._k, len(X)) if len(X) else 1
        self._nn = NearestNeighbors(n_neighbors=max(k, 1)).fit(Xs)
        self._y = np.asarray(y, dtype=float)
        self._model_ids = X["model_id"].to_numpy() if "model_id" in X.columns else None

    def predict_quantiles(self, X: pd.DataFrame, qs: list[float]) -> np.ndarray:
        assert self._nn is not None and self._y is not None
        Xn = X[self._numeric_cols].fillna(0.0).to_numpy(dtype=float)
        Xs = self._scaler.transform(Xn)
        _, idx = self._nn.kneighbors(Xs)
        out = np.zeros((len(X), len(qs)))
        for i in range(len(X)):
            neighbor_idx = idx[i]
            neighbor_y = self._y[neighbor_idx]
            out[i, :] = [np.quantile(neighbor_y, q) for q in qs]
        return out
