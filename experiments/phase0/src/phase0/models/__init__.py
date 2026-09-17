"""Shared model interface for all baselines + the candidate estimator."""

from __future__ import annotations

from typing import Protocol

import numpy as np
import pandas as pd


class QuantileEstimator(Protocol):
    name: str

    def fit(self, X: pd.DataFrame, y: np.ndarray) -> None: ...

    def predict_quantiles(self, X: pd.DataFrame, qs: list[float]) -> np.ndarray:
        """Return an (n, len(qs)) array of predicted quantiles."""
        ...
