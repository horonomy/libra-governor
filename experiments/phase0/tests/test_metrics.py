"""Unit tests for coverage/pinball/decision metrics."""

from __future__ import annotations

import numpy as np

from phase0.metrics.coverage import (
    empirical_coverage,
    mean_interval_width,
    overrun_severity_p95,
    pinball_loss,
)
from phase0.metrics.decision import admission_regret


def test_pinball_loss_zero_for_perfect_prediction():
    y = np.array([1.0, 2.0, 3.0])
    assert pinball_loss(y, y, 0.5) == 0.0


def test_pinball_loss_asymmetric_for_extreme_quantile():
    y_true = np.array([10.0])
    over_pred = np.array([20.0])
    under_pred = np.array([5.0])
    # At q=0.9, underpredicting (missing the high quantile) should cost more
    # than overpredicting by the same margin.
    loss_over = pinball_loss(y_true, over_pred, 0.9)
    loss_under = pinball_loss(y_true, under_pred, 0.9)
    assert loss_under > loss_over


def test_empirical_coverage_basic():
    y_true = np.array([1.0, 2.0, 3.0, 4.0])
    pred_q = np.array([2.0, 2.0, 2.0, 2.0])
    assert empirical_coverage(y_true, pred_q) == 0.5


def test_empirical_coverage_empty_is_nan():
    assert np.isnan(empirical_coverage(np.array([]), np.array([])))


def test_mean_interval_width():
    low = np.array([1.0, 2.0])
    high = np.array([3.0, 5.0])
    assert mean_interval_width(low, high) == 2.5


def test_overrun_severity_p95_no_overrun_is_zero():
    y_true = np.array([1.0, 2.0, 3.0])
    pred_p90 = np.array([10.0, 10.0, 10.0])
    assert overrun_severity_p95(y_true, pred_p90) == 0.0


def test_overrun_severity_p95_positive_overrun():
    y_true = np.array([100.0])
    pred_p90 = np.array([10.0])
    assert overrun_severity_p95(y_true, pred_p90) == 90.0


def test_admission_regret_perfect_predictions_have_zero_false_admit_excess():
    actual = np.array([1.0, 2.0, 3.0, 10.0])
    pred_q80 = actual.copy()  # perfectly matches actual
    censored = np.zeros(4, dtype=bool)
    resolved = np.array([True, True, False, False])
    rows = admission_regret(pred_q80, actual, budgets=[5.0], censored=censored, resolved=resolved)
    row = rows[0]
    # admit iff pred_q80 <= 5.0 -> rows 0,1,2 admitted (<=5), row 3 rejected (10>5)
    assert row["admit_rate"] == 0.75
    # No false admits: every admitted row's actual also <= budget.
    assert row["false_admit_count"] == 0
    assert row["false_admit_excess_usd"] == 0.0
    # Row 3 rejected and actual(10) > budget(5), so not a false reject.
    assert row["false_reject_count"] == 0
    assert row["forgone_success_count"] == 0
    assert row["regret_usd"] == 0.0


def test_admission_regret_false_admit_excess_and_forgone_success():
    pred_q80 = np.array([1.0, 1.0])
    actual = np.array([5.0, 2.0])  # row 0 overran budget; row 1 within budget
    censored = np.array([False, False])
    resolved = np.array([True, True])
    rows = admission_regret(
        pred_q80,
        actual,
        budgets=[3.0],
        censored=censored,
        resolved=resolved,
        forgone_success_penalty_usd=5.0,
    )
    row = rows[0]
    # admit iff pred_q80(1.0) <= 3.0 -> both admitted.
    assert row["admit_rate"] == 1.0
    # Row 0: admitted, actual(5) > budget(3) -> false admit, excess = 2.0
    assert row["false_admit_count"] == 1
    assert row["false_admit_excess_usd"] == 2.0
    assert row["false_reject_count"] == 0
    assert row["forgone_success_count"] == 0
    assert row["regret_usd"] == 2.0


def test_admission_regret_censored_row_still_counts_as_false_admit():
    """A censored row's recorded cost is a lower bound; if even that lower
    bound exceeds budget while admitted, it must still count as a false
    admit -- true cost can only be larger.
    """
    pred_q80 = np.array([1.0])
    actual = np.array([10.0])  # lower bound only, due to censoring
    censored = np.array([True])
    resolved = np.array([False])
    rows = admission_regret(pred_q80, actual, budgets=[3.0], censored=censored, resolved=resolved)
    assert rows[0]["false_admit_count"] == 1
    assert rows[0]["false_admit_excess_usd"] == 7.0
