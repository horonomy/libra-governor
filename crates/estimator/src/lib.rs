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
    BucketTier, Confidence, Estimate, ExecutionReceipt, Feasibility, FeasibilityBound,
    ProgressEvidence, RegimeBasis, RegimeKey, RegimeProvenance, RemainingDuration,
    RemainingResource, RemainingWorkEstimate, ResourceAmount, ResourceKind, SpendSoFar,
    TaskFeatures, TruthStrength, MIN_CONDITIONAL_SAMPLES,
};

pub mod calibration;
pub use calibration::{
    admission_replay, duration_coverage, AdmissionOutcome, AdmissionPolicy, AdmissionStats,
    CostCoverage, CoverageReport, QuantileCoverage, Stratum, CALIBRATION_QUANTILES,
    REQUIRED_CALIBRATION_PAIRS,
};

pub mod regime;
pub use regime::{
    active_regime_status, build_report, confidence_basis, detect_drift, epochs_from_pairs,
    ActiveRegimeStatus, CalibrationEpoch, ConfidenceBasis, ConfidenceChangeReason, DriftVerdict,
    OutOfRegimeContribution, RegimeCalibrationReport, SupersededEpoch, DRIFT_EXCEEDANCE_THRESHOLD,
    DRIFT_WINDOW, OUT_OF_REGIME_BOUNDS_WEIGHT, OUT_OF_REGIME_CONFIDENCE_WEIGHT,
};

/// Minimum number of same-task-class samples required to prefer the
/// class-bucketed history over the full global history. Chosen as a
/// small, defensible threshold — below this, a class-specific bucket is
/// not meaningfully more informative than the global pool, so falling
/// back to more data beats a data-starved narrow slice.
///
/// Re-exported from `libra-governor-domain` (rather than redeclared
/// here) since HORO-1132: [`Confidence::from_evidence`] applies this same
/// threshold as its narrow-tier low/medium boundary, and the two must
/// never silently drift apart.
pub use libra_governor_domain::MIN_CLASS_SAMPLES;

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
    // The legacy entry point has no regime input of its own — pass the
    // honest "pre-regime" provenance, which the positive-evidence rule
    // treats as comparable to everything, preserving this function's
    // pre-HORO-1671 confidence behavior exactly (see the regression test
    // `todays_real_data_shape_produces_identical_confidence_to_pre_1671`
    // in `regime.rs`).
    let regime = RegimeProvenance::pre_regime_record();
    if let Some(class) = class_receipts {
        if class.len() >= MIN_CLASS_SAMPLES {
            return compute(
                class,
                BucketTier::Repo,
                libra_governor_domain::FEATURE_SCHEMA_VERSION,
                &regime,
            );
        }
    }
    if !global_receipts.is_empty() {
        return compute(
            global_receipts,
            BucketTier::Global,
            libra_governor_domain::FEATURE_SCHEMA_VERSION,
            &regime,
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
    current_regime: &RegimeProvenance,
) -> Estimate {
    for tier in bucket_ladder(current) {
        let bucketed: Vec<ExecutionReceipt> = history
            .iter()
            .filter(|(features, _)| features.as_ref().is_some_and(|f| matches(tier, f, current)))
            .map(|(_, receipt)| receipt.clone())
            .collect();
        if bucketed.len() >= MIN_CLASS_SAMPLES {
            return compute(
                &bucketed,
                tier,
                &current.feature_schema_version,
                current_regime,
            );
        }
    }

    let global: Vec<ExecutionReceipt> = history.iter().map(|(_, r)| r.clone()).collect();
    if !global.is_empty() {
        return compute(
            &global,
            BucketTier::Global,
            &current.feature_schema_version,
            current_regime,
        );
    }

    let mut cold = Estimate::cold_start();
    cold.feature_schema_version = current.feature_schema_version.clone();
    cold.regime = RegimeBasis {
        provenance: current_regime.clone(),
        in_regime_sample_count: 0,
        out_of_regime_sample_count: 0,
    };
    cold
}

/// Computes a progressive "remaining work" estimate (HORO-1669):
/// conditions the same bucketed history [`estimate_bucketed`] would use
/// on the runtime evidence in `progress`, via conditional empirical
/// quantiles over truncated history — see
/// `libra_governor_domain::progressive` module docs for why this is not
/// `total_pX - elapsed`.
///
/// Reuses [`bucket_ladder`]/[`matches`] to select the same sample set
/// `estimate_bucketed` would use for `current` — "this task's bucket" is
/// never a second, silently-drifting notion.
pub fn remaining_bucketed(
    history: &[(Option<TaskFeatures>, ExecutionReceipt)],
    current: &TaskFeatures,
    current_regime: &RegimeProvenance,
    progress: &ProgressEvidence,
) -> RemainingWorkEstimate {
    let base = estimate_bucketed(history, current, current_regime);

    let bucketed: Vec<ExecutionReceipt> = bucket_ladder(current)
        .into_iter()
        .find_map(|tier| {
            let matching: Vec<ExecutionReceipt> = history
                .iter()
                .filter(|(features, _)| {
                    features.as_ref().is_some_and(|f| matches(tier, f, current))
                })
                .map(|(_, r)| r.clone())
                .collect();
            (matching.len() >= MIN_CLASS_SAMPLES).then_some(matching)
        })
        .unwrap_or_else(|| history.iter().map(|(_, r)| r.clone()).collect());

    let duration = remaining_duration(&bucketed, progress.elapsed_secs);
    let resource = remaining_resource(&bucketed, &progress.spend_so_far);
    let feasibility = remaining_feasibility(&bucketed, progress.elapsed_secs);

    RemainingWorkEstimate::assemble(&base, duration, resource, feasibility, progress.clone())
}

fn remaining_duration(receipts: &[ExecutionReceipt], elapsed_secs: u64) -> RemainingDuration {
    let mut remaining: Vec<u64> = receipts
        .iter()
        .map(|r| r.actual_duration_secs)
        .filter(|d| *d > elapsed_secs)
        .map(|d| d - elapsed_secs)
        .collect();
    let conditional_n = remaining.len();
    if conditional_n < MIN_CONDITIONAL_SAMPLES {
        return RemainingDuration::Insufficient {
            conditional_n,
            required: MIN_CONDITIONAL_SAMPLES,
            elapsed_secs,
        };
    }
    remaining.sort_unstable();
    RemainingDuration::Quantiles {
        p50_secs: quantile_u64(&remaining, 0.50),
        p80_secs: quantile_u64(&remaining, 0.80),
        p90_secs: quantile_u64(&remaining, 0.90),
        conditional_n,
    }
}

fn remaining_resource(
    receipts: &[ExecutionReceipt],
    spend_so_far: &SpendSoFar,
) -> RemainingResource {
    let amounts: Vec<&ResourceAmount> = receipts
        .iter()
        .flat_map(|r| r.actual_usage.iter())
        .collect();
    if amounts.is_empty() {
        return RemainingResource::Unavailable {
            reason: "no receipt carries resource usage at the current enforcement tier".to_string(),
        };
    }

    let most_common_kind = most_common_kind(&amounts);
    // Absent a known spend-so-far, condition on zero progress — a real,
    // if coarse, "remaining from the start" figure, never fabricated: it
    // is still computed from real historical quantiles, just without
    // narrowing by elapsed spend.
    let spent = match spend_so_far {
        SpendSoFar::Known { kind, settled, .. } if *kind == most_common_kind => *settled,
        _ => 0.0,
    };

    let mut remaining: Vec<f64> = amounts
        .iter()
        .filter(|a| a.kind() == most_common_kind)
        .map(|a| numeric_value(a))
        .filter(|v| *v > spent)
        .map(|v| v - spent)
        .collect();
    let conditional_n = remaining.len();
    if conditional_n < MIN_CONDITIONAL_SAMPLES {
        return RemainingResource::Insufficient {
            conditional_n,
            required: MIN_CONDITIONAL_SAMPLES,
        };
    }
    remaining.sort_by(f64::total_cmp);
    let to_amount = |v: f64| rebuild_amount(most_common_kind, v);
    RemainingResource::Quantiles {
        kind: most_common_kind,
        p50: to_amount(quantile_f64(&remaining, 0.50)),
        p80: to_amount(quantile_f64(&remaining, 0.80)),
        p90: to_amount(quantile_f64(&remaining, 0.90)),
        conditional_n,
        // No receipt-level provenance tag exists yet for historical
        // resource usage (that's HORO-1667's gateway_requests territory,
        // not receipts) -- `Estimated` is the honest, conservative
        // floor until a stronger per-receipt provenance exists.
        weakest_truth: TruthStrength::Estimated,
    }
}

fn remaining_feasibility(receipts: &[ExecutionReceipt], elapsed_secs: u64) -> Feasibility {
    let conditional_n = receipts
        .iter()
        .filter(|r| r.actual_duration_secs > elapsed_secs)
        .count();
    if receipts.len() < MIN_CONDITIONAL_SAMPLES {
        return Feasibility::Insufficient {
            conditional_n: receipts.len(),
            required: MIN_CONDITIONAL_SAMPLES,
        };
    }
    let fraction = conditional_n as f64 / receipts.len() as f64;
    Feasibility::ObservedFrequency {
        conditional_n: receipts.len(),
        fitting_n: conditional_n,
        fraction,
        against: FeasibilityBound {
            kind: ResourceKind::Usd,
            remaining_headroom: 0.0,
        },
    }
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

/// The historically typical (median) `tool_call_count` for the bucket
/// tier `current` would land in, if enough same-bucket history exists to
/// trust a median (HORO-1139). Reuses the exact same
/// [`bucket_ladder`]/[`matches`] walk [`estimate_bucketed`] uses, so
/// "typical tool-call count" and "typical duration/resource" are always
/// computed over the identical sample set for a given task — never two
/// different, silently-drifting notions of "this task's bucket."
///
/// Returns `None` when no tier in the ladder has at least
/// [`MIN_CLASS_SAMPLES`] matching receipts — callers (see
/// `libra_governor_domain::tool_call_count_is_material`) fall back to a
/// fixed absolute threshold in that case, exactly like
/// [`estimate_bucketed`] falls back to [`BucketTier::Global`]/cold-start
/// when no ladder tier meets the threshold. Deliberately does NOT fall
/// back to a global-tier median: a tool-call count "typical" of the
/// entire unrelated local history is not a meaningful comparison point
/// the way a global-tier *duration/resource* quantile still is (that
/// asymmetry is intentional, not an oversight — a materially different
/// task class can have a wildly different normal tool-call count where
/// duration still clusters more consistently).
pub fn typical_tool_call_count_bucketed(
    history: &[(Option<TaskFeatures>, ExecutionReceipt)],
    current: &TaskFeatures,
) -> Option<u64> {
    for tier in bucket_ladder(current) {
        let mut counts: Vec<u64> = history
            .iter()
            .filter(|(features, _)| features.as_ref().is_some_and(|f| matches(tier, f, current)))
            .map(|(_, receipt)| receipt.tool_call_count)
            .collect();
        if counts.len() >= MIN_CLASS_SAMPLES {
            counts.sort_unstable();
            return Some(quantile_u64(&counts, 0.50));
        }
    }
    None
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
/// Computes real quantiles over a non-empty sample set, and (HORO-1671)
/// partitions it into in-/out-of-regime by [`RegimeKey::comparison`]
/// against `current_regime`. Quantile bounds still pool every receipt
/// (out-of-regime weight 1.0 — a bound exists at all rather than
/// cold-starting on every regime change); `confidence` is computed via
/// the UNCHANGED [`Confidence::from_evidence`], but now over the
/// in-regime count only (out-of-regime weight 0.0 for confidence) — see
/// `regime.rs` module docs for the full rationale.
fn compute(
    receipts: &[ExecutionReceipt],
    bucket_tier: BucketTier,
    feature_schema_version: &str,
    current_regime: &RegimeProvenance,
) -> Estimate {
    debug_assert!(!receipts.is_empty());

    let mut durations: Vec<u64> = receipts.iter().map(|r| r.actual_duration_secs).collect();
    durations.sort_unstable();

    let (resource_p50, resource_p80, resource_p90) = resource_quantiles(receipts);

    let pre_regime = RegimeKey::pre_regime_record();
    let in_regime_sample_count = receipts
        .iter()
        .filter(|r| {
            let key = r.regime.as_ref().map(|p| &p.key).unwrap_or(&pre_regime);
            key.comparison(&current_regime.key).is_comparable()
        })
        .count();
    let out_of_regime_sample_count = receipts.len() - in_regime_sample_count;

    Estimate {
        duration_p50_secs: Some(quantile_u64(&durations, 0.50)),
        duration_p80_secs: Some(quantile_u64(&durations, 0.80)),
        duration_p90_secs: Some(quantile_u64(&durations, 0.90)),
        resource_p50,
        resource_p80,
        resource_p90,
        confidence: Confidence::from_evidence(bucket_tier, in_regime_sample_count),
        sample_count: receipts.len(),
        cold_start: false,
        estimator_version: libra_governor_domain::ESTIMATOR_VERSION.to_string(),
        reason: None,
        feature_schema_version: feature_schema_version.to_string(),
        bucket_tier,
        regime: RegimeBasis {
            provenance: current_regime.clone(),
            in_regime_sample_count,
            out_of_regime_sample_count,
        },
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
/// usage at all. That used to be true of every receipt, on the stated
/// premise that Claude Code's hook payloads expose no cost/token data —
/// a premise HORO-1725 disproved (the `Stop` payload carries
/// `transcript_path`, and the host records per-turn token counts there).
/// Receipts written since then carry a measured `Tokens` amount whenever
/// the host exposed a readable transcript, so an all-`None` return now
/// means what it says: no receipt in this window has an observed amount,
/// not that observation is impossible.
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
    fn duration_confidence_via_global_tier_is_capped_at_medium() {
        // `estimate()`'s class-bucket branch tags its result
        // `BucketTier::Repo` (see its own docs), so a caller-supplied
        // class bucket can still reach High via the narrow-tier rule —
        // only the *global-fallback* branch (no class bucket supplied,
        // or one too thin to use) is subject to the Global cap tested
        // here (HORO-1132: `Confidence::from_evidence`).
        let few: Vec<_> = (1..=3).map(|n| receipt(n, vec![])).collect();
        assert_eq!(estimate(&few, None).confidence, Confidence::Medium);

        let many: Vec<_> = (1..=30).map(|n| receipt(n, vec![])).collect();
        assert_eq!(
            estimate(&many, None).confidence,
            Confidence::Medium,
            "Global tier confidence is capped at Medium regardless of n (HORO-1132)"
        );
    }

    #[test]
    fn duration_confidence_via_class_bucket_still_escalates_to_high() {
        let global: Vec<_> = (1..=50).map(|n| receipt(n * 100, vec![])).collect();
        let class: Vec<_> = (1..=20).map(|n| receipt(n, vec![])).collect();
        let result = estimate(&global, Some(&class));
        assert_eq!(result.bucket_tier, BucketTier::Repo);
        assert_eq!(
            result.confidence,
            Confidence::High,
            "a narrow (non-Global) tier with n>=20 must still reach High"
        );
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
        (
            task_features.clone(),
            receipt(duration_secs, vec![]).with_task_features(task_features),
        )
    }

    #[test]
    fn estimate_bucketed_reaches_cold_start_with_no_history_at_all() {
        let current = features("repo-a", BuildTopology::Cargo, Some("claude-sonnet-5"));
        let result = estimate_bucketed(&[], &current, &RegimeProvenance::pre_regime_record());
        assert!(result.cold_start);
        assert_eq!(result.bucket_tier, BucketTier::ColdStart);
        assert_eq!(
            result.feature_schema_version,
            current.feature_schema_version
        );
    }

    #[test]
    fn estimate_bucketed_falls_back_to_global_when_no_bucket_meets_threshold() {
        let current = features("repo-a", BuildTopology::Cargo, Some("claude-sonnet-5"));
        // Unfeatured (pre-MVP-2) history: cannot match any bucketed tier,
        // but must still contribute to the global tier.
        let history: Vec<_> = (1..=6).map(|n| dated_receipt(n * 10, None)).collect();
        let result = estimate_bucketed(&history, &current, &RegimeProvenance::pre_regime_record());
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

        let result = estimate_bucketed(&history, &current, &RegimeProvenance::pre_regime_record());
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
                Some(features(
                    "repo-a",
                    BuildTopology::Cargo,
                    Some("other-model"),
                )),
            )
        }));

        let result = estimate_bucketed(&history, &current, &RegimeProvenance::pre_regime_record());
        assert_eq!(result.bucket_tier, BucketTier::RepoTopologyModel);
        assert_eq!(result.sample_count, 5);
    }

    #[test]
    fn estimate_bucketed_tags_every_estimate_with_the_current_estimator_version() {
        let current = features("repo-a", BuildTopology::Cargo, None);
        assert_eq!(
            estimate_bucketed(&[], &current, &RegimeProvenance::pre_regime_record())
                .estimator_version,
            "v4-regime-aware"
        );
        let history: Vec<_> = (1..=5)
            .map(|n| dated_receipt(n, Some(current.clone())))
            .collect();
        assert_eq!(
            estimate_bucketed(&history, &current, &RegimeProvenance::pre_regime_record())
                .estimator_version,
            "v4-regime-aware"
        );
    }

    fn dated_receipt_with_tool_calls(
        duration_secs: u64,
        tool_call_count: u64,
        task_features: Option<TaskFeatures>,
    ) -> (Option<TaskFeatures>, ExecutionReceipt) {
        (
            task_features.clone(),
            receipt(duration_secs, vec![])
                .with_task_features(task_features)
                .with_tool_call_count(tool_call_count),
        )
    }

    #[test]
    fn typical_tool_call_count_bucketed_is_none_with_no_matching_history() {
        let current = features("repo-a", BuildTopology::Cargo, Some("claude-sonnet-5"));
        assert_eq!(typical_tool_call_count_bucketed(&[], &current), None);
    }

    #[test]
    fn typical_tool_call_count_bucketed_is_none_when_no_tier_meets_the_threshold() {
        let current = features("repo-a", BuildTopology::Cargo, Some("claude-sonnet-5"));
        // Only 2 matching receipts -- below MIN_CLASS_SAMPLES.
        let history: Vec<_> = (1..=2)
            .map(|n| dated_receipt_with_tool_calls(n, n * 3, Some(current.clone())))
            .collect();
        assert_eq!(typical_tool_call_count_bucketed(&history, &current), None);
    }

    #[test]
    fn typical_tool_call_count_bucketed_computes_the_median_over_the_matching_tier() {
        let current = features("repo-a", BuildTopology::Cargo, Some("claude-sonnet-5"));
        let history: Vec<_> = [2u64, 4, 6, 8, 10]
            .into_iter()
            .map(|n| dated_receipt_with_tool_calls(n, n, Some(current.clone())))
            .collect();
        assert_eq!(
            typical_tool_call_count_bucketed(&history, &current),
            Some(6)
        );
    }

    #[test]
    fn typical_tool_call_count_bucketed_prefers_the_most_specific_matching_tier() {
        let current = features("repo-a", BuildTopology::Cargo, Some("claude-sonnet-5"));
        // Exact-match tier: small counts.
        let mut history: Vec<_> = [1u64, 2, 3, 4, 5]
            .into_iter()
            .map(|n| dated_receipt_with_tool_calls(n, n, Some(current.clone())))
            .collect();
        // Same repo, different topology/model (Repo tier only): large counts.
        history.extend([50u64, 60, 70, 80, 90].into_iter().map(|n| {
            dated_receipt_with_tool_calls(
                n,
                n,
                Some(features("repo-a", BuildTopology::Npm, Some("other-model"))),
            )
        }));

        assert_eq!(
            typical_tool_call_count_bucketed(&history, &current),
            Some(3),
            "must use the exact-match tier's median, not the broader Repo tier's"
        );
    }

    #[test]
    fn resource_amounts_remain_none_on_every_bucketed_estimate_from_real_local_data() {
        let current = features("repo-a", BuildTopology::Cargo, None);
        let history: Vec<_> = (1..=10)
            .map(|n| dated_receipt(n, Some(current.clone())))
            .collect();
        let result = estimate_bucketed(&history, &current, &RegimeProvenance::pre_regime_record());
        assert!(result.resource_p50.is_none());
        assert!(result.resource_p80.is_none());
        assert!(result.resource_p90.is_none());
    }

    // -- remaining_bucketed (HORO-1669) ------------------------------------

    fn progress_at(elapsed_secs: u64) -> ProgressEvidence {
        ProgressEvidence {
            elapsed_secs,
            spend_so_far: SpendSoFar::NoBasis {
                reason: libra_governor_domain::NoSpendBasis::NoAccount,
            },
            tool_calls_total: 1,
            tool_calls_since_last_replan: 1,
            same_tool_streak: 1,
            plan_revision: 1,
            auto_replan_count: 0,
            active_lease_count: 0,
            child_account_count: 0,
            gateway_request_count: 0,
            observed_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn remaining_bucketed_is_insufficient_with_no_history() {
        let current = features("repo-a", BuildTopology::Cargo, None);
        let result = remaining_bucketed(
            &[],
            &current,
            &RegimeProvenance::pre_regime_record(),
            &progress_at(10),
        );
        assert!(matches!(
            result.duration,
            RemainingDuration::Insufficient { .. }
        ));
    }

    #[test]
    fn remaining_bucketed_never_reads_zero_at_the_moment_of_overrunning() {
        // The pathology this ticket exists to avoid: `total_p90 -
        // elapsed` would read 0 (or saturate) once elapsed exceeds every
        // historical sample. The conditional-quantile method must
        // instead report Insufficient, never a fabricated zero duration.
        let current = features("repo-a", BuildTopology::Cargo, None);
        let history: Vec<_> = (1..=10)
            .map(|n| dated_receipt(n * 10, Some(current.clone())))
            .collect();
        // Elapsed already exceeds every sample in history (max is 100).
        let result = remaining_bucketed(
            &history,
            &current,
            &RegimeProvenance::pre_regime_record(),
            &progress_at(1000),
        );
        match result.duration {
            RemainingDuration::Insufficient { conditional_n, .. } => {
                assert_eq!(conditional_n, 0);
            }
            RemainingDuration::Quantiles { p50_secs, .. } => {
                panic!("expected Insufficient, not a fabricated quantile (got p50={p50_secs})")
            }
        }
    }

    #[test]
    fn remaining_bucketed_shrinks_the_conditional_sample_set_as_elapsed_grows() {
        let current = features("repo-a", BuildTopology::Cargo, None);
        // Durations 10..=200 step 10 (20 samples): plenty to clear
        // MIN_CONDITIONAL_SAMPLES at low elapsed, fewer qualify as
        // elapsed grows.
        let history: Vec<_> = (1..=20)
            .map(|n| dated_receipt(n * 10, Some(current.clone())))
            .collect();

        let early = remaining_bucketed(
            &history,
            &current,
            &RegimeProvenance::pre_regime_record(),
            &progress_at(10),
        );
        let late = remaining_bucketed(
            &history,
            &current,
            &RegimeProvenance::pre_regime_record(),
            &progress_at(150),
        );

        let early_n = match early.duration {
            RemainingDuration::Quantiles { conditional_n, .. } => conditional_n,
            RemainingDuration::Insufficient { conditional_n, .. } => conditional_n,
        };
        let late_n = match late.duration {
            RemainingDuration::Quantiles { conditional_n, .. } => conditional_n,
            RemainingDuration::Insufficient { conditional_n, .. } => conditional_n,
        };
        assert!(
            late_n < early_n,
            "conditional sample set must shrink monotonically as elapsed grows: early={early_n}, late={late_n}"
        );
    }

    #[test]
    fn remaining_bucketed_resource_arm_is_unavailable_with_no_usage_data() {
        let current = features("repo-a", BuildTopology::Cargo, None);
        let history: Vec<_> = (1..=10)
            .map(|n| dated_receipt(n, Some(current.clone())))
            .collect();
        let result = remaining_bucketed(
            &history,
            &current,
            &RegimeProvenance::pre_regime_record(),
            &progress_at(1),
        );
        assert!(matches!(
            result.resource,
            RemainingResource::Unavailable { .. }
        ));
    }

    #[test]
    fn remaining_bucketed_consumes_confidence_and_regime_from_the_base_estimate() {
        let current = features("repo-a", BuildTopology::Cargo, None);
        let history: Vec<_> = (1..=20)
            .map(|n| dated_receipt(n * 10, Some(current.clone())))
            .collect();
        let base = estimate_bucketed(&history, &current, &RegimeProvenance::pre_regime_record());
        let remaining = remaining_bucketed(
            &history,
            &current,
            &RegimeProvenance::pre_regime_record(),
            &progress_at(10),
        );
        assert_eq!(remaining.confidence, base.confidence);
        assert_eq!(remaining.regime, base.regime);
        assert_eq!(remaining.bucket_tier, base.bucket_tier);
    }
}
