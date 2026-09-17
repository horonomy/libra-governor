"""Turns phase0_results.json into phase0_report.md."""

from __future__ import annotations

from typing import Any

FALSIFICATION_RULE = (
    "If `candidate` does not beat `taskclass_repo` on `regret_usd` at matched "
    "admit-rate, the hypothesis is not supported by this evidence. "
    "Coverage/pinball/sharpness numbers are supporting evidence, not the "
    "headline finding."
)


def _matched_admit_rate_comparison(decision_rows: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """For each split_config/budget, compare candidate vs taskclass_repo regret_usd."""
    by_key: dict[tuple[str, float], dict[str, dict[str, Any]]] = {}
    for row in decision_rows:
        key = (row["split_config"], row["budget_usd"])
        by_key.setdefault(key, {})[row["model_name"]] = row

    comparisons = []
    for (split_config, budget), models in sorted(
        by_key.items(), key=lambda kv: (kv[0][0], kv[0][1])
    ):
        cand = models.get("candidate")
        repo_baseline = models.get("taskclass_repo")
        if cand is None or repo_baseline is None:
            continue
        comparisons.append(
            {
                "split_config": split_config,
                "budget_usd": budget,
                "candidate_regret_usd": cand["regret_usd"],
                "taskclass_repo_regret_usd": repo_baseline["regret_usd"],
                "candidate_admit_rate": cand["admit_rate"],
                "taskclass_repo_admit_rate": repo_baseline["admit_rate"],
                "candidate_beats_repo": cand["regret_usd"] < repo_baseline["regret_usd"],
            }
        )
    return comparisons


def render_report(results: dict[str, Any]) -> str:
    run_meta = results.get("run", {})
    dataset = results.get("dataset", {})
    decision_rows = results.get("decision_utility", [])
    metrics_rows = results.get("metrics", [])

    comparisons = _matched_admit_rate_comparison(decision_rows)
    n_wins = sum(1 for c in comparisons if c["candidate_beats_repo"])
    n_total = len(comparisons)
    overall_verdict = (
        "SUPPORTED" if n_total and n_wins == n_total else ("MIXED" if n_wins else "NOT SUPPORTED")
    )
    if n_total == 0:
        overall_verdict = "NO EVIDENCE (no matched candidate/taskclass_repo rows)"

    lines: list[str] = []
    lines.append("# Phase 0 Estimator Feasibility Report")
    lines.append("")
    lines.append("## Falsification rule")
    lines.append("")
    lines.append(f"> {FALSIFICATION_RULE}")
    lines.append("")
    lines.append("## Headline finding")
    lines.append("")
    lines.append(
        f"**Verdict: {overall_verdict}** -- candidate beat taskclass_repo on regret_usd "
        f"in {n_wins}/{n_total} matched (split_config, budget) comparisons."
    )
    lines.append("")
    if comparisons:
        lines.append(
            "| split_config | budget_usd | candidate regret_usd | taskclass_repo regret_usd | candidate wins |"  # noqa: E501
        )
        lines.append("|---|---|---|---|---|")
        for c in comparisons:
            lines.append(
                f"| {c['split_config']} | {c['budget_usd']} | "  # noqa: E501
                f"{c['candidate_regret_usd']:.2f} | {c['taskclass_repo_regret_usd']:.2f} | "
                f"{'yes' if c['candidate_beats_repo'] else 'no'} |"
            )
        lines.append("")

    lines.append("## Dataset")
    lines.append("")
    lines.append(f"- n_rows: {dataset.get('n_rows')}")
    lines.append(f"- n_instances: {dataset.get('n_instances')}")
    lines.append(f"- n_submissions: {dataset.get('n_submissions')}")
    lines.append(f"- censoring_rate: {dataset.get('censoring_rate')}")
    lines.append(f"- resolved_rate: {dataset.get('resolved_rate')}")
    lines.append(f"- n_uncensored_rows: {dataset.get('n_uncensored_rows')}")
    lines.append(f"- escalation_triggered: {dataset.get('escalation_triggered')}")
    if dataset.get("escalation_triggered"):
        lines.append("")
        lines.append(
            "> **ESCALATION**: uncensored row count is below the configured "
            "min_uncensored_rows threshold. Per HORO-1120, this is a "
            "dataset-adequacy judgment for HORO-1123 to decide, not something "
            "this harness routes around."
        )
    lines.append("")

    lines.append("## Supporting evidence: coverage / pinball / sharpness")
    lines.append("")
    lines.append("(Supporting evidence only -- see falsification rule above.)")
    lines.append("")
    lines.append(
        "| split_config | model_name | target | quantile | empirical_coverage | pinball_loss | n | oracle_probe | status |"  # noqa: E501
    )
    lines.append("|---|---|---|---|---|---|---|---|---|")
    for row in metrics_rows:
        cov = row["empirical_coverage"]
        pin = row["pinball_loss"]
        cov_str = f"{cov:.3f}" if cov is not None else "n/a"
        pin_str = f"{pin:.4f}" if pin is not None else "n/a"
        lines.append(
            f"| {row['split_config']} | {row['model_name']} | {row.get('target', '')} | "
            f"{row['quantile']} | {cov_str} | {pin_str} | {row['n']} | "
            f"{row['oracle_probe']} | {row['status']} |"
        )
    lines.append("")

    tuning_rows = results.get("candidate_tuning", [])
    if tuning_rows:
        lines.append("## Candidate n_estimators choice (per split_config / target / fold)")
        lines.append("")
        lines.append("| split_config | target | chosen_n_estimators (by quantile) |")
        lines.append("|---|---|---|")
        for row in tuning_rows:
            lines.append(
                f"| {row['split_config']} | {row['target']} | {row['chosen_n_estimators']} |"
            )
        lines.append("")

    lines.append("## Run metadata")
    lines.append("")
    lines.append(f"- seed: {run_meta.get('seed')}")
    lines.append(f"- git_sha: {run_meta.get('git_sha')}")
    lines.append(f"- config_sha256: {run_meta.get('config_sha256')}")
    lines.append(f"- dataset_fingerprint: {run_meta.get('dataset_fingerprint')}")
    lines.append(f"- generated_at: {run_meta.get('generated_at')}")
    lines.append("")
    lines.append("## Known limitations")
    lines.append("")
    lines.append(
        "See PROVENANCE.md for the full list (sample size, harness diversity, "
        "proxy time target, censoring bias, Python-only generalization)."
    )
    lines.append("")

    return "\n".join(lines)
