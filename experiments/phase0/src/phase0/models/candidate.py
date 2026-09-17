"""Candidate estimator: sklearn GradientBoostingRegressor(loss="quantile")
wrapped in one-sided split-conformal (CQR-style) calibration.

Per spec: sklearn only, no LightGBM, no neural network ("interpretable
first"). One fit per quantile in {0.5, 0.8, 0.9}, max_depth=3, n_estimators
chosen from a fixed 3-point grid via calibration-fold pinball loss.

Conformal adjustment is one-sided by construction: the downstream decision
metric (admission_regret) only ever thresholds on pred_q80 via
`admit iff pred_q80 <= budget`, so a symmetric interval-widening calibration
would be incoherent with how the prediction is actually used. Instead we
compute the conformity score s_i = y_i - qhat(x_i) on a held-out calibration
slice and add its ceil((n+1)(1-alpha))-th order statistic to every future
prediction for that quantile -- this raises (or lowers) the quantile
prediction just enough to hit nominal coverage on the calibration slice,
one-sidedly, matching the "predicted value must not be exceeded" semantics
used everywhere else in this harness.
"""

from __future__ import annotations

import math

import numpy as np
import pandas as pd
from sklearn.ensemble import GradientBoostingRegressor

from phase0.metrics.coverage import pinball_loss


def _one_sided_conformal_offset(residuals: np.ndarray, quantile: float) -> float:
    """ceil((n+1)*quantile)-th order statistic of residuals (y - qhat)."""
    n = len(residuals)
    if n == 0:
        return 0.0
    rank = min(n, max(1, math.ceil((n + 1) * quantile)))
    sorted_res = np.sort(residuals)
    return float(sorted_res[rank - 1])


class CandidateEstimator:
    """GBRT quantile regressor with split-conformal calibration, per-quantile."""

    name = "candidate"

    def __init__(
        self,
        n_estimators_grid: list[int],
        max_depth: int = 3,
        random_state: int = 0,
    ) -> None:
        self._n_estimators_grid = n_estimators_grid
        self._max_depth = max_depth
        self._random_state = random_state
        self._models: dict[float, GradientBoostingRegressor] = {}
        self._offsets: dict[float, float] = {}
        self.chosen_n_estimators: dict[float, int] = {}

    def fit_with_calibration(
        self,
        X_fit: pd.DataFrame,
        y_fit: np.ndarray,
        X_cal: pd.DataFrame,
        y_cal: np.ndarray,
        qs: list[float],
    ) -> None:
        """Fit on (X_fit, y_fit); calibrate the conformal offset on (X_cal, y_cal).

        X_fit / X_cal must already be numeric-encoded (see report.py / run
        pipeline for the encoding step) and contain ONLY whitelisted columns.
        """
        for q in qs:
            best_n, best_model, best_loss = None, None, math.inf
            for n_estimators in self._n_estimators_grid:
                model = GradientBoostingRegressor(
                    loss="quantile",
                    alpha=q,
                    max_depth=self._max_depth,
                    n_estimators=n_estimators,
                    random_state=self._random_state,
                )
                model.fit(X_fit, y_fit)
                cal_pred = model.predict(X_cal)
                loss = pinball_loss(y_cal, cal_pred, q)
                if loss < best_loss:
                    best_n, best_model, best_loss = n_estimators, model, loss
            assert best_model is not None and best_n is not None
            self._models[q] = best_model
            self.chosen_n_estimators[q] = best_n

            cal_pred = best_model.predict(X_cal)
            residuals = np.asarray(y_cal, dtype=float) - cal_pred
            self._offsets[q] = _one_sided_conformal_offset(residuals, q)

    def predict_quantiles(self, X: pd.DataFrame, qs: list[float]) -> np.ndarray:
        out = np.zeros((len(X), len(qs)))
        for j, q in enumerate(qs):
            model = self._models[q]
            offset = self._offsets.get(q, 0.0)
            out[:, j] = model.predict(X) + offset
        return out
