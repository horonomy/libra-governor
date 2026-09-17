//! `libra-governor-estimator` — a simple, native-Rust, self-calibrating
//! empirical-quantile estimator for preflight cost/time estimates.
//!
//! This is deliberately *not* the Python/sklearn conformal-GBRT estimator
//! evaluated in `experiments/phase0/` — that harness is a separate,
//! already-closed evidence artifact, out of scope here. This crate
//! computes P50/P80/P90 quantiles directly over locally accumulated
//! [`ExecutionReceipt`] history, with no model training and no external
//! dependency.
//!
//! # Bucketing
//!
//! [`estimate`] (MVP 1.0, kept unchanged) supports three coarse tiers,
//! most to least specific, purely as a function of the sample sets it is
//! handed: (a) a caller-supplied "class" bucket, if it has at least
//! [`MIN_CLASS_SAMPLES`] rows; (b) the full global local history; (c)
//! cold start. Nothing in `libra-governor-domain` classified tasks in
//! MVP 1.0, so `libra-governor-daemon` never actually called it with a
//! class bucket — every MVP 1.0 estimate came from tier (b) or (c).
//!
//! [`estimate_bucketed`] (HORO-1130) replaces that with a real
//! hierarchical backoff ladder over [`TaskFeatures`] — see
//! [`bucket_ladder`] — and is what `libra-governor-daemon` calls today.
//! Walks most-specific to least-specific
//! (`RepoTopologyModel -> RepoTopology -> Repo -> Topology`), then falls
//! back to the full global history (`Global`), then to
//! [`Estimate::cold_start`] (`ColdStart`) when there is no local history
//! at all. The first tier whose matching sample set meets
//! [`MIN_CLASS_SAMPLES`] wins.

use libra_governor_domain::{
    BucketTier, Confidence, Estimate, ExecutionReceipt, ResourceAmount, ResourceKind, TaskFeatures,
};

/// Minimum number of same-task-class samples required to prefer the
/// class-bucketed history over the full global history. Chosen as a
/// small, defensible threshold consistent with
/// [`Confidence::from_sample_count`]'s "5 samples is the low/medium
/// boundary" — below this, a class-specific bucket is not meaningfully
/// more informative than the global pool, so falling back to more data
/// beats a data-starved narrow slice.
pub const MIN_CLASS_SAMPLES: usize = 5;

/// Computes a preflight [`Estimate`] from local execution history.
///
/// `global_receipts` is the full local history (see
/// [`libra_governor_ledger::LedgerStore::receipts_for_estimation`]).
/// `class_receipts`, when `Some` and at least [`MIN_CLASS_SAMPLES`] long,
/// is preferred over `global_receipts` — see module docs on bucketing.
/// Falls back to [`Estimate::cold_start`] only when `global_receipts` is
/// also empty.
pub fn estimate(
    global_receipts: &[ExecutionReceipt],
    class_receipts: Option<&[ExecutionReceipt]>,
) -> Estimate {
    if let Some(class) = class_receipts {
        if class.len() >= MIN_CLASS_SAMPLES {
            return compute(class, BucketTier::Repo, libra_governor_domain::FEATURE_SCHEMA_VERSION);
        }
    }
    if !global_receipts.is_empty() {
        return compute(
            global_receipts,
            BucketTier::Global,
            libra_governor_domain::FEATURE_SCHEMA_VERSION,
        );
    }
    Estimate::cold_start()
}

/// Computes a preflight [`Estimate`] with real task-class bucketing
/// (HORO-1130): walks [`bucket_ladder`] most-specific to least-specific,
/// using the first tier whose matching sample set meets
/// [`MIN_CLASS_SAMPLES`], falling back to the full local history
/// ([`BucketTier::Global`]) and finally to [`Estimate::cold_start`]
/// ([`BucketTier::ColdStart`]) when there is no local history at all.
///
/// `history` is every locally recorded receipt paired with the
/// [`TaskFeatures`] its originating plan was estimated against, if any
/// (see
/// [`libra_governor_ledger::LedgerStore::receipts_for_estimation`]). A
/// pre-MVP-2 receipt with `None` features can never match a bucketed
/// tier (it carries no features to match against) but still contributes
/// to the global-tier sample set — see the migration/backward-compat
/// test in this module.
pub fn estimate_bucketed(
    history: &[(Option<TaskFeatures>, ExecutionReceipt)],
    current: &TaskFeatures,
) -> Estimate {
    for tier in bucket_ladder(current) {
        let bucketed: Vec<ExecutionReceipt> = history
            .iter()
            .filter(|(features, _)| {
                features
                    .as_ref()
                    .is_some_and(|f| matches(tier, f, current))
            })
            .map(|(_, receipt)| receipt.clone())
            .collect();
        if bucketed.len() >= MIN_CLASS_SAMPLES {
            return compute(&bucketed, tier, &current.feature_schema_version);
        }
    }

    let global: Vec<ExecutionReceipt> = history.iter().map(|(_, r)| r.clone()).collect();
    if !global.is_empty() {
        return compute(&global, BucketTier::Global, &current.feature_schema_version);
    }

    let mut cold = Estimate::cold_start();
    cold.feature_schema_version = current.feature_schema_version.clone();
    cold
}

/// The hierarchical backoff ladder, most specific to least specific.
/// [`BucketTier::Global`] and [`BucketTier::ColdStart`] are not part of
/// this ladder: they are the two fallbacks [`estimate_bucketed`] applies
/// after every ladder tier has been tried and none met
/// [`MIN_CLASS_SAMPLES`].
fn bucket_ladder(_current: &TaskFeatures) -> [BucketTier; 4] {
    [
        BucketTier::RepoTopologyModel,
        BucketTier::RepoTopology,
        BucketTier::Repo,
        BucketTier::Topology,
    ]
}

/// Whether `a` and `b` belong to the same bucket at `tier`.
fn matches(tier: BucketTier, a: &TaskFeatures, b: &TaskFeatures) -> bool {
    match tier {
        BucketTier::RepoTopologyModel => {
            a.repo_key == b.repo_key && a.topology == b.topology && a.model == b.model
        }
        BucketTier::RepoTopology => a.repo_key == b.repo_key && a.topology == b.topology,
        BucketTier::Repo => a.repo_key == b.repo_key,
        BucketTier::Topology => a.topology == b.topology,
        BucketTier::Global | BucketTier::ColdStart => true,
    }
}

/// Computes real quantiles over a non-empty sample set. Never called with
/// an empty slice — callers route the empty case to
/// [`Estimate::cold_start`] instead, so this function can assume at least
/// one sample.
fn compute(receipts: &[ExecutionReceipt], bucket_tier: BucketTier, feature_schema_version: &str) -> Estimate {
    debug_assert!(!receipts.is_empty());

    let mut durations: Vec<u64> = receipts.iter().map(|r| r.actual_duration_secs).collect();
    durations.sort_unstable();

    let (resource_p50, resource_p80, resource_p90) = resource_quantiles(receipts);

    Estimate {
        duration_p50_secs: Some(quantile_u64(&durations, 0.50)),
        duration_p80_secs: Some(quantile_u64(&durations, 0.80)),
        duration_p90_secs: Some(quantile_u64(&durations, 0.90)),
        resource_p50,
        resource_p80,
        resource_p90,
        confidence: Confidence::from_sample_count(receipts.len()),
        sample_count: receipts.len(),
        cold_start: false,
        estimator_version: libra_governor_domain::ESTIMATOR_VERSION.to_string(),
        reason: None,
        feature_schema_version: feature_schema_version.to_string(),
        bucket_tier,
    }
}

/// Nearest-rank quantile over an already-sorted, non-empty slice.
/// `p` is a fraction in `[0.0, 1.0]`.
fn quantile_u64(sorted: &[u64], p: f64) -> u64 {
    debug_assert!(!sorted.is_empty());
    let rank = (p * sorted.len() as f64).ceil() as usize;
    let index = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[index]
}

/// Selects the most-common [`ResourceKind`] across every
/// [`ExecutionReceipt::actual_usage`] entry in `receipts`, then computes
/// P50/P80/P90 over just the amounts of that kind.
///
/// [`ResourceAmount`] deliberately does not implement addition/averaging
/// across kinds (see its crate docs: mixing USD cents, raw tokens, and
/// quota percentages must never be silently summed). Quantiling over the
/// single most-observed kind and ignoring the others is this ticket's
/// chosen simplification — a future ticket could instead return one
/// `Estimate` per kind, but nothing in the current `PreflightResult`/
/// `Estimate` shape asks for that, and building it speculatively would be
/// exactly the unwarranted complexity this ticket's brief asks to avoid.
/// Returns `(None, None, None)` when no receipt carries any resource
/// usage at all (true of every MVP 1.0 receipt today — Claude Code's hook
/// payloads expose no cost/token data, see the PR description).
fn resource_quantiles(
    receipts: &[ExecutionReceipt],
) -> (
    Option<ResourceAmount>,
    Option<ResourceAmount>,
    Option<ResourceAmount>,
) {
    let amounts: Vec<&ResourceAmount> = receipts
        .iter()
        .flat_map(|r| r.actual_usage.iter())
        .collect();
    if amounts.is_empty() {
        return (None, None, None);
    }

    let most_common_kind = most_common_kind(&amounts);

    let mut values: Vec<f64> = amounts
        .iter()
        .filter(|a| a.kind() == most_common_kind)
        .map(|a| numeric_value(a))
        .collect();
    values.sort_by(f64::total_cmp);

    let to_amount = |v: f64| rebuild_amount(most_common_kind, v);
    (
        Some(to_amount(quantile_f64(&values, 0.50))),
        Some(to_amount(quantile_f64(&values, 0.80))),
        Some(to_amount(quantile_f64(&values, 0.90))),
    )
}

fn quantile_f64(sorted: &[f64], p: f64) -> f64 {
    debug_assert!(!sorted.is_empty());
    let rank = (p * sorted.len() as f64).ceil() as usize;
    let index = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[index]
}

/// The first-encountered [`ResourceKind`] with the highest occurrence
/// count. Deterministic tie-breaking (first-seen order) rather than
/// enum-declaration order, so the result does not silently change if
/// `ResourceKind`'s variant order is ever reshuffled.
fn most_common_kind(amounts: &[&ResourceAmount]) -> ResourceKind {
    let mut seen_order: Vec<ResourceKind> = Vec::new();
    let mut counts: Vec<(ResourceKind, usize)> = Vec::new();
    for amount in amounts {
        let kind = amount.kind();
        if let Some(entry) = counts.iter_mut().find(|(k, _)| *k == kind) {
            entry.1 += 1;
        } else {
            seen_order.push(kind);
            counts.push((kind, 1));
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(kind, _)| kind)
        .unwrap_or(seen_order[0])
}

fn numeric_value(amount: &ResourceAmount) -> f64 {
    match amount {
        ResourceAmount::UsdCents(v) => *v as f64,
        ResourceAmount::Tokens(v) => *v as f64,
        ResourceAmount::QuotaPercent(v) => *v as f64,
    }
}

fn rebuild_amount(kind: ResourceKind, value: f64) -> ResourceAmount {
    match kind {
        ResourceKind::Usd => ResourceAmount::UsdCents(value.round() as i64),
        ResourceKind::Tokens => ResourceAmount::Tokens(value.round() as u64),
        ResourceKind::QuotaPercent => ResourceAmount::QuotaPercent(value as f32),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libra_governor_domain::{BuildTopology, ExecutionOutcome, PlanId, TaskId};
    use time::OffsetDateTime;

    fn receipt(duration_secs: u64, usage: Vec<ResourceAmount>) -> ExecutionReceipt {
        ExecutionReceipt::new(
            TaskId::new(),
            1,
            PlanId::new(),
            duration_secs,
            usage,
            ExecutionOutcome::Unknown,
            OffsetDateTime::UNIX_EPOCH,
        )
    }

    #[test]
    fn cold_start_when_no_local_history_at_all() {
        let result = estimate(&[], None);
        assert!(result.cold_start);
        assert_eq!(result.sample_count, 0);
        assert_eq!(result.confidence, Confidence::Low);
        assert!(result.duration_p50_secs.is_none());
    }

    #[test]
    fn falls_back_to_global_history_when_no_class_bucket_supplied() {
        let receipts: Vec<_> = (1..=10).map(|n| receipt(n * 10, vec![])).collect();
        let result = estimate(&receipts, None);
        assert!(!result.cold_start);
        assert_eq!(result.sample_count, 10);
        assert_eq!(
            result.estimator_version,
            libra_governor_domain::ESTIMATOR_VERSION
        );
    }

    #[test]
    fn prefers_class_bucket_when_it_meets_the_minimum_sample_threshold() {
        let global: Vec<_> = (1..=50).map(|n| receipt(n * 100, vec![])).collect();
        let class: Vec<_> = (1..=5).map(|n| receipt(n, vec![])).collect();
        let result = estimate(&global, Some(&class));
        assert_eq!(
            result.sample_count, 5,
            "must use the smaller, more specific class bucket once it meets the threshold"
        );
    }

    #[test]
    fn falls_back_to_global_when_class_bucket_is_too_thin() {
        let global: Vec<_> = (1..=20).map(|n| receipt(n * 100, vec![])).collect();
        let class: Vec<_> = (1..=2).map(|n| receipt(n, vec![])).collect();
        let result = estimate(&global, Some(&class));
        assert_eq!(
            result.sample_count, 20,
            "a class bucket below MIN_CLASS_SAMPLES must not be used"
        );
    }

    #[test]
    fn quantile_u64_matches_hand_computed_values_for_ten_samples() {
        let sorted: Vec<u64> = (1..=10).map(|n| n * 10).collect(); // 10,20,...,100
        assert_eq!(quantile_u64(&sorted, 0.50), 50);
        assert_eq!(quantile_u64(&sorted, 0.80), 80);
        assert_eq!(quantile_u64(&sorted, 0.90), 90);
    }

    #[test]
    fn duration_confidence_escalates_with_sample_count() {
        let few: Vec<_> = (1..=3).map(|n| receipt(n, vec![])).collect();
        assert_eq!(estimate(&few, None).confidence, Confidence::Low);

        let medium: Vec<_> = (1..=10).map(|n| receipt(n, vec![])).collect();
        assert_eq!(estimate(&medium, None).confidence, Confidence::Medium);

        let many: Vec<_> = (1..=30).map(|n| receipt(n, vec![])).collect();
        assert_eq!(estimate(&many, None).confidence, Confidence::High);
    }

    #[test]
    fn resource_quantiles_are_none_when_no_receipt_carries_usage() {
        let receipts: Vec<_> = (1..=5).map(|n| receipt(n, vec![])).collect();
        let result = estimate(&receipts, None);
        assert!(result.resource_p50.is_none());
        assert!(result.resource_p80.is_none());
        assert!(result.resource_p90.is_none());
    }

    #[test]
    fn resource_quantiles_computed_over_tokens_when_present() {
        let receipts: Vec<_> = (1..=10)
            .map(|n| receipt(n, vec![ResourceAmount::Tokens(n * 1000)]))
            .collect();
        let result = estimate(&receipts, None);
        assert_eq!(result.resource_p50, Some(ResourceAmount::Tokens(5000)));
        assert_eq!(result.resource_p90, Some(ResourceAmount::Tokens(9000)));
    }

    #[test]
    fn resource_quantiles_pick_the_most_common_kind_and_ignore_the_rest() {
        let mut receipts: Vec<_> = (1..=8)
            .map(|n| receipt(n, vec![ResourceAmount::Tokens(n * 100)]))
            .collect();
        // A minority of receipts report a USD amount instead — must not
        // be mixed into the token quantiles.
        receipts.push(receipt(9, vec![ResourceAmount::UsdCents(999)]));

        let result = estimate(&receipts, None);
        match result.resource_p50 {
            Some(ResourceAmount::Tokens(_)) => {}
            other => panic!("expected the majority Tokens kind, got {other:?}"),
        }
    }

    #[test]
    fn every_estimate_is_tagged_with_the_estimator_version() {
        assert_eq!(
            Estimate::cold_start().estimator_version,
            libra_governor_domain::ESTIMATOR_VERSION
        );
        let receipts: Vec<_> = (1..=5).map(|n| receipt(n, vec![])).collect();
        assert_eq!(
            estimate(&receipts, None).estimator_version,
            libra_governor_domain::ESTIMATOR_VERSION
        );
    }

    fn features(repo_key: &str, topology: BuildTopology, model: Option<&str>) -> TaskFeatures {
        TaskFeatures {
            repo_key: repo_key.to_string(),
            topology,
            model: model.map(str::to_string),
            prompt_char_len: 10,
            prompt_line_count: 1,
            prompt_code_block_count: 0,
            prompt_has_traceback: false,
            prompt_filepath_token_count: 0,
            prompt_numeric_token_count: 0,
            likely_affected_path_count: 0,
            detected_test_command_count: 1,
            feature_schema_version: libra_governor_domain::FEATURE_SCHEMA_VERSION.to_string(),
        }
    }

    fn dated_receipt(
        duration_secs: u64,
        task_features: Option<TaskFeatures>,
    ) -> (Option<TaskFeatures>, ExecutionReceipt) {
        (task_features.clone(), receipt(duration_secs, vec![]).with_task_features(task_features))
    }

    #[test]
    fn estimate_bucketed_reaches_cold_start_with_no_history_at_all() {
        let current = features("repo-a", BuildTopology::Cargo, Some("claude-sonnet-5"));
        let result = estimate_bucketed(&[], &current);
        assert!(result.cold_start);
        assert_eq!(result.bucket_tier, BucketTier::ColdStart);
        assert_eq!(result.feature_schema_version, current.feature_schema_version);
    }

    #[test]
    fn estimate_bucketed_falls_back_to_global_when_no_bucket_meets_threshold() {
        let current = features("repo-a", BuildTopology::Cargo, Some("claude-sonnet-5"));
        // Unfeatured (pre-MVP-2) history: cannot match any bucketed tier,
        // but must still contribute to the global tier.
        let history: Vec<_> = (1..=6).map(|n| dated_receipt(n * 10, None)).collect();
        let result = estimate_bucketed(&history, &current);
        assert_eq!(result.bucket_tier, BucketTier::Global);
        assert_eq!(result.sample_count, 6);
        assert!(!result.cold_start);
    }

    #[test]
    fn estimate_bucketed_selects_repo_tier_once_threshold_is_met() {
        let current = features("repo-a", BuildTopology::Cargo, Some("claude-sonnet-5"));
        let mut history: Vec<_> = (1..=5)
            .map(|n| {
                dated_receipt(
                    n,
                    Some(features("repo-a", BuildTopology::Npm, Some("other-model"))),
                )
            })
            .collect();
        // A larger pool of unrelated global history, so the global tier
        // would report a different (larger) sample_count if it were
        // chosen instead.
        history.extend((1..=50).map(|n| dated_receipt(n * 100, None)));

        let result = estimate_bucketed(&history, &current);
        assert_eq!(
            result.bucket_tier,
            BucketTier::Repo,
            "same repo_key but different topology/model must land on the Repo tier, not narrower"
        );
        assert_eq!(result.sample_count, 5);
        assert!(
            result.sample_count < history.len(),
            "bucketed sample_count must be smaller than the global pool"
        );
    }

    #[test]
    fn estimate_bucketed_prefers_most_specific_tier_available() {
        let current = features("repo-a", BuildTopology::Cargo, Some("claude-sonnet-5"));
        let mut history: Vec<_> = (1..=5)
            .map(|n| dated_receipt(n, Some(current.clone())))
            .collect();
        history.extend((1..=5).map(|n| {
            dated_receipt(
                n * 10,
                Some(features("repo-a", BuildTopology::Cargo, Some("other-model"))),
            )
        }));

        let result = estimate_bucketed(&history, &current);
        assert_eq!(result.bucket_tier, BucketTier::RepoTopologyModel);
        assert_eq!(result.sample_count, 5);
    }

    #[test]
    fn estimate_bucketed_tags_every_estimate_with_v2_bucketed_quantile() {
        let current = features("repo-a", BuildTopology::Cargo, None);
        assert_eq!(
            estimate_bucketed(&[], &current).estimator_version,
            "v2-bucketed-quantile"
        );
        let history: Vec<_> = (1..=5).map(|n| dated_receipt(n, Some(current.clone()))).collect();
        assert_eq!(
            estimate_bucketed(&history, &current).estimator_version,
            "v2-bucketed-quantile"
        );
    }

    #[test]
    fn resource_amounts_remain_none_on_every_bucketed_estimate_from_real_local_data() {
        let current = features("repo-a", BuildTopology::Cargo, None);
        let history: Vec<_> = (1..=10).map(|n| dated_receipt(n, Some(current.clone()))).collect();
        let result = estimate_bucketed(&history, &current);
        assert!(result.resource_p50.is_none());
        assert!(result.resource_p80.is_none());
        assert!(result.resource_p90.is_none());
    }
}
