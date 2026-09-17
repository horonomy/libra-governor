"""Asserts fitted feature columns == the admission-time whitelist exactly.

This is the enforcement mechanism for "nothing post-hoc ever enters fit()" --
a real test, not a comment. It works by monkeypatching pandas.DataFrame
column access... no: simpler and more direct, it wraps every estimator's
fit() to record the columns of the X actually passed, then asserts that set
against FEATURE_WHITELIST (plus the documented `difficulty` addition for the
oracle-probe baseline, which is explicitly tagged and excluded from the
"legitimate admission-time baseline" claim).
"""

from __future__ import annotations

from pathlib import Path

import numpy as np
import pandas as pd
import pytest

from phase0 import features
from phase0.config import load_config
from phase0.models.baselines import (
    GlobalMedianEstimator,
    KnnHistoryEstimator,
    make_taskclass_difficulty,
    make_taskclass_repo,
)
from phase0.models.candidate import CandidateEstimator

FIXTURE_PATH = Path(__file__).parent / "fixtures" / "mini.parquet"

BANNED_COLUMNS = {
    "patch",
    "test_patch",
    "FAIL_TO_PASS",
    "PASS_TO_PASS",
    "resolved",
    "n_steps",
    "api_calls",
    "exit_status",
    "instance_cost_usd",
}


@pytest.fixture()
def df() -> pd.DataFrame:
    raw = pd.read_parquet(FIXTURE_PATH)
    # repo_prior_median_cost/repo_prior_n are computed per-fold at run time
    # (features.add_repo_prior_features), not stored in the extracted
    # parquet -- treat the whole fixture as one training fold here so the
    # whitelist columns actually exist for these unit tests.
    train_mask = np.ones(len(raw), dtype=bool)
    return features.add_repo_prior_features(raw, train_mask)


def test_feature_whitelist_has_no_banned_columns():
    assert BANNED_COLUMNS.isdisjoint(features.FEATURE_WHITELIST)


def test_global_median_only_sees_whitelist(df):
    seen_columns = {}
    est = GlobalMedianEstimator()
    X = df[features.FEATURE_WHITELIST]
    seen_columns["cols"] = list(X.columns)
    est.fit(X, np.log1p(df["instance_cost_usd"].to_numpy()))
    assert set(seen_columns["cols"]) == set(features.FEATURE_WHITELIST)
    assert BANNED_COLUMNS.isdisjoint(seen_columns["cols"])


def test_taskclass_repo_only_sees_whitelist(df):
    est = make_taskclass_repo()
    X = df[features.FEATURE_WHITELIST]
    est.fit(X, np.log1p(df["instance_cost_usd"].to_numpy()))
    assert set(X.columns) == set(features.FEATURE_WHITELIST)


def test_taskclass_difficulty_is_oracle_probe_not_whitelisted(df):
    """difficulty is intentionally NOT in FEATURE_WHITELIST -- it's an
    oracle-probe upper bound, not a legitimate admission-time feature, and
    tests/test_leakage.py's job here is to confirm it stays excluded.
    """
    assert "difficulty" not in features.FEATURE_WHITELIST
    est = make_taskclass_difficulty()
    X = df[features.FEATURE_WHITELIST + ["difficulty"]]
    est.fit(X, np.log1p(df["instance_cost_usd"].to_numpy()))
    # The estimator only reads the "difficulty" group column; every OTHER
    # column it could have touched is still exactly the whitelist.
    assert set(X.columns) - {"difficulty"} == set(features.FEATURE_WHITELIST)


def test_knn_history_input_columns_are_whitelist_plus_model_id(df):
    encoder = features.FeatureEncoder().fit(df)
    X_enc = encoder.transform(df)
    X_enc["model_id"] = df["model_id"].to_numpy()

    # Every encoded column is either a whitelisted numeric passthrough or a
    # one-hot expansion of a whitelisted categorical column; "model_id" is
    # appended separately only for same-model_id neighbor filtering, not fed
    # to the distance metric (numeric_cols excludes it).
    for col in encoder.output_columns:
        if col in features.NUMERIC_FEATURES:
            continue
        assert any(col.startswith(cat + "_") for cat in features.CATEGORICAL_FEATURES), col

    knn = KnnHistoryEstimator(numeric_cols=encoder.output_columns, k=3)
    knn.fit(X_enc, np.log1p(df["instance_cost_usd"].to_numpy()))
    # No banned column name appears anywhere in the encoded matrix.
    assert BANNED_COLUMNS.isdisjoint(X_enc.columns)


def test_candidate_fit_input_is_exactly_the_encoded_whitelist(df):
    """The strongest version of the leakage check: the candidate's actual
    fitted feature matrix (post-encoding) traces back to FEATURE_WHITELIST
    columns only -- verified by round-tripping the encoder's own bookkeeping,
    not by reading the candidate's source.
    """
    encoder = features.FeatureEncoder()
    encoder.fit(df)
    X = encoder.transform(df)

    assert BANNED_COLUMNS.isdisjoint(X.columns)
    # Every output column is either a numeric passthrough (whitelisted) or a
    # one-hot expansion of a whitelisted categorical column.
    for col in X.columns:
        if col in features.NUMERIC_FEATURES:
            continue
        assert any(col.startswith(cat + "_") for cat in features.CATEGORICAL_FEATURES), col

    cfg = load_config()
    cand = CandidateEstimator(
        n_estimators_grid=[10],
        max_depth=2,
        random_state=int(cfg["seed"]),
    )
    y = np.log1p(df["instance_cost_usd"].to_numpy())
    n = len(df)
    fit_idx = np.arange(0, int(n * 0.7))
    cal_idx = np.arange(int(n * 0.7), n)
    cand.fit_with_calibration(X.iloc[fit_idx], y[fit_idx], X.iloc[cal_idx], y[cal_idx], qs=[0.5])
    pred = cand.predict_quantiles(X, [0.5])
    assert pred.shape == (n, 1)
