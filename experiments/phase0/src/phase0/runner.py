"""Orchestrates `phase0 run`: fits all models across both frozen split
configs and assembles the metrics/stratified/decision_utility/compute_cost
rows for results/phase0_results.json.

Network-free: operates only on the already-extracted DataFrame and the
already-frozen split files. Loading either is the caller's job
(see __main__.py) so this module stays easy to unit test against a small
in-memory DataFrame.
"""

from __future__ import annotations

import time
from pathlib import Path
from typing import Any

import numpy as np
import pandas as pd

from phase0 import features, splits
from phase0.config import Config
from phase0.metrics.coverage import (
    empirical_coverage,
    mean_interval_width,
    overrun_severity_p95,
    pinball_loss,
)
from phase0.metrics.decision import admission_regret
from phase0.models.baselines import (
    GlobalMedianEstimator,
    KnnHistoryEstimator,
    LLMSelfEstimateEstimator,
    make_taskclass_difficulty,
    make_taskclass_repo,
)
from phase0.models.candidate import CandidateEstimator

ORACLE_PROBE_MODELS = {"taskclass_difficulty"}


def _fit_predict_baselines(
    train_df: pd.DataFrame,
    test_df: pd.DataFrame,
    y_train_log: np.ndarray,
    qs: list[float],
    cache_dir: Path,
) -> dict[str, dict[str, Any]]:
    """Fits every baseline; returns {model_name: {"pred_log": arr, "oracle_probe": bool,
    "status": "ok"|"unavailable", "unavailable_mask": arr|None, "fit_seconds": float,
    "predict_seconds": float}}.
    """
    results: dict[str, dict[str, Any]] = {}

    def _time_fit_predict(model, whitelist_cols, oracle_probe=False):
        t0 = time.perf_counter()
        model.fit(train_df[whitelist_cols], y_train_log)
        fit_seconds = time.perf_counter() - t0
        t0 = time.perf_counter()
        pred = model.predict_quantiles(test_df[whitelist_cols], qs)
        predict_seconds = time.perf_counter() - t0
        return pred, fit_seconds, predict_seconds

    gm = GlobalMedianEstimator()
    pred, fit_s, pred_s = _time_fit_predict(gm, features.FEATURE_WHITELIST)
    results["global_median"] = {
        "pred_log": pred,
        "oracle_probe": False,
        "status": "ok",
        "unavailable_mask": None,
        "fit_seconds": fit_s,
        "predict_seconds": pred_s,
    }

    tc_repo = make_taskclass_repo()
    pred, fit_s, pred_s = _time_fit_predict(tc_repo, features.FEATURE_WHITELIST)
    results["taskclass_repo"] = {
        "pred_log": pred,
        "oracle_probe": False,
        "status": "ok",
        "unavailable_mask": None,
        "fit_seconds": fit_s,
        "predict_seconds": pred_s,
    }

    tc_diff = make_taskclass_difficulty()
    diff_cols = features.FEATURE_WHITELIST + (
        ["difficulty"] if "difficulty" not in features.FEATURE_WHITELIST else []
    )
    pred, fit_s, pred_s = _time_fit_predict(tc_diff, diff_cols, oracle_probe=True)
    results["taskclass_difficulty"] = {
        "pred_log": pred,
        "oracle_probe": True,
        "status": "ok",
        "unavailable_mask": None,
        "fit_seconds": fit_s,
        "predict_seconds": pred_s,
    }

    # llm_self_estimate: single global cache, model_id varies per row so we
    # group by model_id and predict per-group, merging masks back in order.
    llm_pred = np.full((len(test_df), len(qs)), np.nan)
    llm_unavailable = np.zeros(len(test_df), dtype=bool)
    t0 = time.perf_counter()
    for model_id, group in test_df.groupby("model_id"):
        est = LLMSelfEstimateEstimator(
            cache_path=cache_dir / "llm_selfestimate.json", model_id=model_id
        )
        est.fit(train_df[features.FEATURE_WHITELIST], y_train_log)
        group_cols = features.FEATURE_WHITELIST + ["instance_id"]
        group_pred = est.predict_quantiles(group[group_cols], qs)
        positions = test_df.index.get_indexer(group.index)
        llm_pred[positions] = group_pred
        llm_unavailable[positions] = est.unavailable_mask
    llm_seconds = time.perf_counter() - t0
    status = "unavailable" if llm_unavailable.all() and len(test_df) else "ok"
    results["llm_self_estimate"] = {
        "pred_log": llm_pred,
        "oracle_probe": False,
        "status": status,
        "unavailable_mask": llm_unavailable,
        "fit_seconds": llm_seconds,
        "predict_seconds": 0.0,
    }

    knn_encoder = features.FeatureEncoder().fit(train_df)
    X_train_enc = knn_encoder.transform(train_df)
    X_test_enc = knn_encoder.transform(test_df)
    X_train_enc["model_id"] = train_df["model_id"].to_numpy()
    X_test_enc["model_id"] = test_df["model_id"].to_numpy()

    knn = KnnHistoryEstimator(numeric_cols=knn_encoder.output_columns, k=5)
    t0 = time.perf_counter()
    knn.fit(X_train_enc, y_train_log)
    fit_s = time.perf_counter() - t0
    t0 = time.perf_counter()
    pred = knn.predict_quantiles(X_test_enc, qs)
    pred_s = time.perf_counter() - t0
    results["knn_history"] = {
        "pred_log": pred,
        "oracle_probe": False,
        "status": "ok",
        "unavailable_mask": None,
        "fit_seconds": fit_s,
        "predict_seconds": pred_s,
    }

    return results


def _fit_predict_candidate(
    train_df: pd.DataFrame,
    test_df: pd.DataFrame,
    fit_row_ids: list[str],
    cal_row_ids: list[str],
    y_col_log: pd.Series,
    qs: list[float],
    n_estimators_grid: list[int],
    max_depth: int,
    seed: int,
) -> dict[str, Any]:
    """Fits the candidate on a pure numeric encoding (FeatureEncoder output
    only -- no model_id string column, unlike the encoding knn_history uses).
    Re-fits its own encoder on the fit slice only, so calibration-fold and
    test-fold rows never influence the one-hot vocabulary.
    """
    fit_mask = train_df["row_id"].isin(fit_row_ids)
    cal_mask = train_df["row_id"].isin(cal_row_ids)

    candidate_encoder = features.FeatureEncoder().fit(train_df[fit_mask])
    X_fit_enc = candidate_encoder.transform(train_df[fit_mask])
    X_cal_enc = candidate_encoder.transform(train_df[cal_mask])
    X_test_enc = candidate_encoder.transform(test_df)
    y_fit = y_col_log[fit_mask].to_numpy()
    y_cal = y_col_log[cal_mask].to_numpy()

    cand = CandidateEstimator(
        n_estimators_grid=n_estimators_grid, max_depth=max_depth, random_state=seed
    )
    t0 = time.perf_counter()
    cand.fit_with_calibration(X_fit_enc, y_fit, X_cal_enc, y_cal, qs)
    fit_seconds = time.perf_counter() - t0
    t0 = time.perf_counter()
    pred = cand.predict_quantiles(X_test_enc, qs)
    predict_seconds = time.perf_counter() - t0
    return {
        "pred_log": pred,
        "oracle_probe": False,
        "status": "ok",
        "unavailable_mask": None,
        "fit_seconds": fit_seconds,
        "predict_seconds": predict_seconds,
        "chosen_n_estimators": cand.chosen_n_estimators,
    }


def run_all(cfg: Config, df: pd.DataFrame, splits_dir: Path, cache_dir: Path) -> dict[str, Any]:
    qs = list(cfg["quantiles"])
    budgets = list(cfg["budget_grid_usd"])
    penalty = float(cfg["forgone_success_penalty_usd"])
    cal_fraction = float(cfg["calibration_fraction"])
    seed = int(cfg["seed"])
    n_estimators_grid = list(cfg["candidate_n_estimators_grid"])
    max_depth = int(cfg["candidate_max_depth"])

    metrics_rows: list[dict[str, Any]] = []
    stratified_rows: list[dict[str, Any]] = []
    decision_rows: list[dict[str, Any]] = []
    compute_cost_accum: dict[str, dict[str, list[float]]] = {}

    for split_name, group_col in splits.SPLIT_CONFIGS.items():
        payload = splits.load_frozen_split(split_name, splits_dir)
        folds = payload["folds"]

        oof: dict[str, dict[str, list]] = {}

        for fold in folds:
            train_ids = set(fold["train_row_ids"])
            test_ids = set(fold["test_row_ids"])
            train_mask = df["row_id"].isin(train_ids).to_numpy()
            test_mask = df["row_id"].isin(test_ids).to_numpy()
            if not train_mask.any() or not test_mask.any():
                continue

            fold_df = features.add_repo_prior_features(df, train_mask)
            train_df = fold_df[train_mask].reset_index(drop=True)
            test_df = fold_df[test_mask].reset_index(drop=True)

            y_train_usd = train_df["instance_cost_usd"].to_numpy()
            y_train_log = np.log1p(y_train_usd)
            y_test_usd = test_df["instance_cost_usd"].to_numpy()

            baseline_results = _fit_predict_baselines(train_df, test_df, y_train_log, qs, cache_dir)

            fit_row_ids, cal_row_ids = splits.carve_calibration_slice(
                fold["train_row_ids"], train_df, group_col, cal_fraction, seed
            )
            candidate_result = _fit_predict_candidate(
                train_df,
                test_df,
                fit_row_ids,
                cal_row_ids,
                pd.Series(y_train_log, index=train_df.index),
                qs,
                n_estimators_grid,
                max_depth,
                seed,
            )

            all_results = {**baseline_results, "candidate": candidate_result}

            for model_name, result in all_results.items():
                pred_usd = np.expm1(result["pred_log"])
                bucket = oof.setdefault(
                    model_name,
                    {
                        "pred_usd": [],
                        "y_true_usd": [],
                        "resolved": [],
                        "censored": [],
                        "unavailable_mask": [],
                        "oracle_probe": result["oracle_probe"],
                        "status_all_unavailable": result["status"] == "unavailable",
                        "fit_seconds": [],
                        "predict_seconds": [],
                    },
                )
                bucket["pred_usd"].append(pred_usd)
                bucket["y_true_usd"].append(y_test_usd)
                bucket["resolved"].append(test_df["resolved"].to_numpy())
                bucket["censored"].append(test_df["censored"].to_numpy())
                mask = (
                    result["unavailable_mask"]
                    if result["unavailable_mask"] is not None
                    else np.zeros(len(test_df), dtype=bool)
                )
                bucket["unavailable_mask"].append(mask)
                bucket["fit_seconds"].append(result["fit_seconds"])
                bucket["predict_seconds"].append(result["predict_seconds"])

                key = model_name
                cost_bucket = compute_cost_accum.setdefault(
                    key, {"fit_seconds": [], "predict_seconds": [], "n_predict_rows": []}
                )
                cost_bucket["fit_seconds"].append(result["fit_seconds"])
                cost_bucket["predict_seconds"].append(result["predict_seconds"])
                cost_bucket["n_predict_rows"].append(len(test_df))

        # aggregate out-of-fold predictions for this split_config
        for model_name, bucket in oof.items():
            pred_usd = np.concatenate(bucket["pred_usd"], axis=0)
            y_true_usd = np.concatenate(bucket["y_true_usd"], axis=0)
            resolved = np.concatenate(bucket["resolved"], axis=0)
            censored = np.concatenate(bucket["censored"], axis=0)
            unavailable = np.concatenate(bucket["unavailable_mask"], axis=0)
            available = ~unavailable
            oracle_probe = bucket["oracle_probe"]
            n_total = len(pred_usd)

            for qi, q in enumerate(qs):
                pred_q = pred_usd[:, qi]
                if available.any():
                    cov = empirical_coverage(y_true_usd[available], pred_q[available])
                    cov_uncensored = empirical_coverage(
                        y_true_usd[available & ~censored], pred_q[available & ~censored]
                    )
                    cov_censored = (
                        empirical_coverage(
                            y_true_usd[available & censored], pred_q[available & censored]
                        )
                        if (available & censored).any()
                        else float("nan")
                    )
                    pin = pinball_loss(y_true_usd[available], pred_q[available], q)
                else:
                    cov = cov_uncensored = cov_censored = float("nan")
                    pin = float("nan")

                status = (
                    "unavailable"
                    if bucket["status_all_unavailable"] and not available.any()
                    else "ok"
                )

                width = float("nan")
                if q == 0.9 and available.any():
                    p50_idx = qs.index(0.5) if 0.5 in qs else None
                    if p50_idx is not None:
                        width = mean_interval_width(pred_usd[available, p50_idx], pred_q[available])

                overrun = float("nan")
                if q == 0.9 and available.any():
                    overrun = overrun_severity_p95(y_true_usd[available], pred_q[available])

                metrics_rows.append(
                    {
                        "split_config": split_name,
                        "model_name": model_name,
                        "target": "instance_cost_usd",
                        "quantile": q,
                        "empirical_coverage": cov,
                        "coverage_uncensored": cov_uncensored,
                        "coverage_censored": cov_censored,
                        "pinball_loss": pin,
                        "mean_interval_width_usd": width,
                        "overrun_severity_p95_usd": overrun,
                        "n": n_total,
                        "oracle_probe": oracle_probe,
                        "status": status,
                    }
                )

                for stratum_name in ("resolved", "unresolved", "exit_cost"):
                    if stratum_name == "exit_cost":
                        strat_mask = censored
                    elif stratum_name == "resolved":
                        strat_mask = resolved & ~censored
                    else:
                        strat_mask = ~resolved & ~censored
                    strat_avail = strat_mask & available
                    if strat_avail.any():
                        strat_cov = empirical_coverage(y_true_usd[strat_avail], pred_q[strat_avail])
                        strat_pin = pinball_loss(y_true_usd[strat_avail], pred_q[strat_avail], q)
                    else:
                        strat_cov = float("nan")
                        strat_pin = float("nan")
                    stratified_rows.append(
                        {
                            "split_config": split_name,
                            "model_name": model_name,
                            "target": "instance_cost_usd",
                            "quantile": q,
                            "stratum": stratum_name,
                            "empirical_coverage": strat_cov,
                            "pinball_loss": strat_pin,
                            "n": int(strat_avail.sum()),
                            "oracle_probe": oracle_probe,
                            "status": status,
                        }
                    )

            if 0.8 in qs and available.any():
                q80_idx = qs.index(0.8)
                regret_rows = admission_regret(
                    pred_usd[available, q80_idx],
                    y_true_usd[available],
                    budgets,
                    censored[available],
                    resolved[available],
                    penalty,
                )
                for row in regret_rows:
                    decision_rows.append(
                        {"split_config": split_name, "model_name": model_name, **row}
                    )

    compute_cost_rows = []
    for model_name, bucket in compute_cost_accum.items():
        total_predict_rows = sum(bucket["n_predict_rows"]) or 1
        total_predict_seconds = sum(bucket["predict_seconds"])
        compute_cost_rows.append(
            {
                "model_name": model_name,
                "fit_seconds": float(np.mean(bucket["fit_seconds"]))
                if bucket["fit_seconds"]
                else 0.0,
                "predict_seconds_per_1k": total_predict_seconds / total_predict_rows * 1000.0,
                "peak_rss_mb": _peak_rss_mb(),
            }
        )

    return {
        "metrics": metrics_rows,
        "stratified": stratified_rows,
        "decision_utility": decision_rows,
        "compute_cost": compute_cost_rows,
    }


def _peak_rss_mb() -> float:
    """Simple proxy for peak RSS: resource.getrusage, unit-normalized to MB.

    ru_maxrss is KB on Linux, bytes on macOS -- 10M is a safe threshold
    between "a few hundred MB in KB" and "a few hundred MB in bytes" for any
    process this harness would plausibly run as.
    """
    try:
        import resource

        raw = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
        return raw / (1024.0 * 1024.0) if raw > 10_000_000 else raw / 1024.0
    except Exception:
        return float("nan")
