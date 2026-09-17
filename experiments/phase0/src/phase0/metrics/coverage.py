"""Coverage, sharpness, and calibration metrics for quantile predictions."""

from __future__ import annotations

import numpy as np


def pinball_loss(y_true: np.ndarray, y_pred: np.ndarray, quantile: float) -> float:
    """Mean pinball (quantile) loss for one quantile level."""
    y_true = np.asarray(y_true, dtype=float)
    y_pred = np.asarray(y_pred, dtype=float)
    diff = y_true - y_pred
    return float(np.mean(np.maximum(quantile * diff, (quantile - 1) * diff)))


def empirical_coverage(y_true: np.ndarray, y_pred_quantile: np.ndarray) -> float:
    """Fraction of true y <= predicted quantile."""
    y_true = np.asarray(y_true, dtype=float)
    y_pred_quantile = np.asarray(y_pred_quantile, dtype=float)
    if len(y_true) == 0:
        return float("nan")
    return float(np.mean(y_true <= y_pred_quantile))


def mean_interval_width(pred_low: np.ndarray, pred_high: np.ndarray) -> float:
    """Mean width of a (low, high) quantile pair, e.g. (p10, p90) or (p50-ish, p50+ish)."""
    pred_low = np.asarray(pred_low, dtype=float)
    pred_high = np.asarray(pred_high, dtype=float)
    if len(pred_low) == 0:
        return float("nan")
    return float(np.mean(pred_high - pred_low))


def overrun_severity_p95(y_true: np.ndarray, pred_p90: np.ndarray) -> float:
    """p95 of (actual - predicted_p90), restricted to positive overruns.

    If no row overran, returns 0.0 (no overrun severity to report), not NaN.
    """
    y_true = np.asarray(y_true, dtype=float)
    pred_p90 = np.asarray(pred_p90, dtype=float)
    overrun = y_true - pred_p90
    positive = overrun[overrun > 0]
    if len(positive) == 0:
        return 0.0
    return float(np.quantile(positive, 0.95))
