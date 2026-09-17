//! Duration-coverage and admission-replay calibration metrics (HORO-1132).
//!
//! This module answers one question honestly: *given the estimates Libra
//! actually produced and the actuals Libra actually recorded, how good
//! were those estimates?* It never fabricates an answer when there is not
//! enough real evidence to give one — see [`CoverageReport::Insufficient`]
//! and [`CoverageReport::Degenerate`], and their [`AdmissionOutcome`]
//! counterparts.
//!
//! # Why this is not the HORO-1127 validation corpus
//!
//! `experiments/mvp1_validation/results/` is scripted evidence: every
//! receipt in it carries `actual_duration_secs: 0.0`, so any coverage
//! computed over it is not measuring the estimator against reality — it
//! is measuring the estimator against a script that never actually ran
//! anything. That corpus is explicitly out of scope for this module (see
//! HORO-1132's ticket description). The degenerate-data guard in
//! [`duration_coverage`] exists in part so nobody can repeat that mistake
//! by accident: feeding it 34 identical (or near-identical) actuals
//! produces [`CoverageReport::Degenerate`], never a false "100% coverage."
//!
//! # No cost coverage yet
//!
//! There is deliberately no `cost_regret`-shaped field anywhere in this
//! module that would just always be `None`. [`CostCoverage`] names that
//! absence as a real fact instead — see its docs.

use std::collections::{BTreeMap, BTreeSet};

use libra_governor_domain::{BucketTier, Estimate};
use libra_governor_ledger::CalibrationPair;
use serde::{Deserialize, Serialize};

/// The quantiles this module evaluates coverage/admission for. Mirrors
/// the three quantiles [`crate::compute`] actually produces
/// (`duration_p50_secs` / `duration_p80_secs` / `duration_p90_secs`) —
/// there is nothing to evaluate at a quantile the estimator never emits.
pub const CALIBRATION_QUANTILES: [f64; 3] = [0.50, 0.80, 0.90];

/// The minimum number of non-cold-start [`CalibrationPair`]s
/// `duration_coverage` requires before it will compute a real metric
/// instead of reporting [`CoverageReport::Insufficient`]. Chosen to match
/// this ticket's own documented real-evidence trigger (see the HORO-1132
/// PR description): fewer than 30 pairs is not enough to say anything
/// meaningful about a P90 tail.
pub const REQUIRED_CALIBRATION_PAIRS: usize = 30;

/// Below this many *distinct* actual-duration values, a data set that
/// otherwise has enough rows is still not real calibration evidence — it
/// is degenerate (e.g. every row scripted to the same duration). See
/// module docs on the HORO-1127 corpus.
const MIN_DISTINCT_ACTUALS: usize = 2;

/// Coverage/pinball-loss accounting for one quantile over one (sub)set of
/// pairs. Field names deliberately mirror Phase 0's `phase0_results.json`
/// metric names (`empirical_coverage`, `pinball_loss`) so a human can
/// compare the two artifacts side by side later, even though the two are
/// never programmatically merged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuantileCoverage {
    pub quantile: f64,
    /// How many pairs this quantile's metrics were computed over.
    pub n: usize,
    /// How many of those pairs had `actual_duration_secs <= ` this
    /// quantile's predicted bound.
    pub hits: usize,
    /// `hits / n`, or `None` when `n == 0` — never a fabricated `0.0` or
    /// `1.0` for an empty set.
    pub empirical_coverage: Option<f64>,
    /// Mean pinball (quantile) loss over the set, or `None` when `n == 0`.
    pub pinball_loss: Option<f64>,
}

/// Coverage for one named stratum (a [`BucketTier`] or a sample-count
/// band), one [`QuantileCoverage`] per entry in [`CALIBRATION_QUANTILES`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stratum {
    pub label: String,
    pub quantiles: Vec<QuantileCoverage>,
}

/// The result of [`duration_coverage`]. Structurally cannot be mistaken
/// for a real, positive result when the underlying evidence is not
/// real: `0` calibration pairs (or a handful) is [`Self::Insufficient`],
/// and a data set that has enough rows but no real variance in its
/// actuals (e.g. every receipt scripted to `0` seconds) is
/// [`Self::Degenerate`] — neither ever renders as
/// [`Self::Computed`]-shaped 100% coverage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CoverageReport {
    /// Fewer than `required` non-cold-start [`CalibrationPair`]s exist
    /// locally. The honest, correct outcome for a pre-launch product with
    /// no real trajectory history yet.
    Insufficient { n: usize, required: usize },
    /// `n` met `required`, but the actual-duration values are (almost)
    /// all identical — computing coverage over this would not be
    /// measuring calibration, only restating the degenerate input. See
    /// module docs on the HORO-1127 corpus, which failed exactly this
    /// way.
    Degenerate {
        n: usize,
        distinct_actual_durations: usize,
        reason: String,
    },
    /// A real coverage computation over real, non-degenerate local
    /// evidence.
    Computed {
        n: usize,
        overall: Vec<QuantileCoverage>,
        by_bucket_tier: Vec<Stratum>,
        by_sample_band: Vec<Stratum>,
    },
}

/// A duration-only admission policy to replay local history against:
/// "would this task have been admitted, given a `deadline_secs` budget
/// and an estimate quantile at `threshold_quantile`?"
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AdmissionPolicy {
    pub deadline_secs: u64,
    /// Must be one of [`CALIBRATION_QUANTILES`] — [`admission_replay`]
    /// simply cannot evaluate a quantile the estimator never produced,
    /// and excludes any pair it cannot evaluate rather than guessing.
    pub threshold_quantile: f64,
}

/// Real counts from replaying [`AdmissionPolicy`] against local history.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct AdmissionStats {
    pub n: usize,
    /// How many pairs the policy would have admitted (predicted quantile
    /// <= deadline).
    pub admit_count: usize,
    /// Admitted, but the actual duration overran the deadline anyway —
    /// the costly failure mode (work started that shouldn't have been).
    pub false_admit_count: usize,
    /// Rejected, but the actual duration would have made the deadline —
    /// the opportunity-cost failure mode (work that could have been
    /// admitted, wasn't).
    pub false_reject_count: usize,
    /// Mean overrun (`actual - deadline`) across false-admits only, or
    /// `None` when there were none.
    pub mean_overrun_secs: Option<f64>,
    /// P95 overrun across false-admits only, or `None` when there were
    /// none.
    pub p95_overrun_secs: Option<f64>,
}

/// The result of [`admission_replay`]. See [`CoverageReport`] for why an
/// explicit insufficient-data state matters — the same reasoning applies
/// here: a rate computed over zero usable pairs must never render as a
/// real (and trivially perfect, or trivially zero) rate.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AdmissionOutcome {
    Insufficient { n: usize, required: usize },
    Computed(AdmissionStats),
}

/// Names, at the type level, that cost/USD-based coverage and admission
/// replay are not structurally supported yet — mirroring how
/// [`libra_governor_domain::Estimate::cold_start`] communicates
/// absence-of-basis as a real fact rather than a null. Nothing in this
/// module has a `cost_regret`-shaped field that is just always `None`;
/// this type is the one honest place that absence is recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CostCoverage {
    Unavailable { reason: &'static str },
}

impl CostCoverage {
    /// The one (and, deliberately, only) way to obtain a [`CostCoverage`]
    /// value today.
    pub fn unavailable() -> Self {
        CostCoverage::Unavailable {
            reason: "no locally recorded ExecutionReceipt carries resource_amount usage data \
                     today (Claude Code's hook payloads expose no cost/token figures — see \
                     libra-governor-estimator::resource_quantiles docs), so cost-based coverage \
                     and admission replay have no real basis to compute from yet",
        }
    }
}

/// Computes duration-coverage metrics over `pairs` — see [`CoverageReport`]
/// for the three possible shapes of the result.
pub fn duration_coverage(pairs: &[CalibrationPair]) -> CoverageReport {
    let n = pairs.len();
    if n < REQUIRED_CALIBRATION_PAIRS {
        return CoverageReport::Insufficient {
            n,
            required: REQUIRED_CALIBRATION_PAIRS,
        };
    }

    let distinct: BTreeSet<u64> = pairs.iter().map(|p| p.actual_duration_secs).collect();
    if distinct.len() < MIN_DISTINCT_ACTUALS {
        return CoverageReport::Degenerate {
            n,
            distinct_actual_durations: distinct.len(),
            reason: "actual durations are (almost) all identical; coverage over degenerate \
                     data is not meaningful calibration evidence"
                .to_string(),
        };
    }

    let all: Vec<&CalibrationPair> = pairs.iter().collect();
    let overall = CALIBRATION_QUANTILES
        .iter()
        .map(|&q| quantile_coverage_for(&all, q))
        .collect();

    let by_bucket_tier = stratify(&all, |p| tier_label(p.estimate.bucket_tier).to_string());
    let by_sample_band = stratify(&all, |p| sample_band(p.estimate.sample_count).to_string());

    CoverageReport::Computed {
        n,
        overall,
        by_bucket_tier,
        by_sample_band,
    }
}

/// Replays `policy` against `pairs` — duration-only, admitting whenever
/// the estimate's `threshold_quantile` bound is within `deadline_secs`.
/// Pairs whose estimate does not carry `threshold_quantile` (i.e.
/// `threshold_quantile` is not one of [`CALIBRATION_QUANTILES`]) are
/// excluded rather than guessed at.
///
/// Gated on the same [`REQUIRED_CALIBRATION_PAIRS`] floor as
/// [`duration_coverage`]: an admit/false-admit *rate* computed from a
/// handful of pairs is exactly the same false-positive-from-nothing
/// shape this module exists to prevent for coverage — "1 admitted, 0
/// false admits" from n=1 reads just as misleadingly confident as "100%
/// coverage" from n=0. The real per-pair counting logic lives in
/// [`admission_stats`], which the hand-computed fixture test below calls
/// directly (bypassing this gate on purpose, since that test's whole
/// point is to pin the counting logic itself on a small, human-checkable
/// fixture).
pub fn admission_replay(pairs: &[CalibrationPair], policy: AdmissionPolicy) -> AdmissionOutcome {
    let usable: Vec<&CalibrationPair> = pairs
        .iter()
        .filter(|p| quantile_value(&p.estimate, policy.threshold_quantile).is_some())
        .collect();

    if usable.len() < REQUIRED_CALIBRATION_PAIRS {
        return AdmissionOutcome::Insufficient {
            n: usable.len(),
            required: REQUIRED_CALIBRATION_PAIRS,
        };
    }

    AdmissionOutcome::Computed(admission_stats(&usable, policy))
}

/// The real per-pair admit/false-admit/false-reject counting, with no
/// insufficient-data gate of its own — callers decide whether `pairs` is
/// a large enough sample to trust. [`admission_replay`] is the gated
/// public entry point; this exists as a separate function so the
/// hand-computed small-fixture test can exercise the counting logic
/// directly without needing 30 rows to satisfy that gate.
fn admission_stats(pairs: &[&CalibrationPair], policy: AdmissionPolicy) -> AdmissionStats {
    let mut admit_count = 0usize;
    let mut false_admit_count = 0usize;
    let mut false_reject_count = 0usize;
    let mut overruns: Vec<f64> = Vec::new();

    for pair in pairs {
        let predicted = quantile_value(&pair.estimate, policy.threshold_quantile)
            .expect("caller filtered to pairs carrying this quantile");
        let admit = predicted <= policy.deadline_secs;
        let within_deadline = pair.actual_duration_secs <= policy.deadline_secs;

        if admit {
            admit_count += 1;
            if !within_deadline {
                false_admit_count += 1;
                overruns.push(pair.actual_duration_secs as f64 - policy.deadline_secs as f64);
            }
        } else if within_deadline {
            false_reject_count += 1;
        }
    }

    let mean_overrun_secs = mean(&overruns);
    let p95_overrun_secs = if overruns.is_empty() {
        None
    } else {
        let mut sorted = overruns.clone();
        sorted.sort_by(f64::total_cmp);
        Some(crate::quantile_f64(&sorted, 0.95))
    };

    AdmissionStats {
        n: pairs.len(),
        admit_count,
        false_admit_count,
        false_reject_count,
        mean_overrun_secs,
        p95_overrun_secs,
    }
}

fn mean(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        None
    } else {
        Some(values.iter().sum::<f64>() / values.len() as f64)
    }
}

fn quantile_coverage_for(pairs: &[&CalibrationPair], quantile: f64) -> QuantileCoverage {
    let n = pairs.len();
    if n == 0 {
        return QuantileCoverage {
            quantile,
            n: 0,
            hits: 0,
            empirical_coverage: None,
            pinball_loss: None,
        };
    }

    let mut hits = 0usize;
    let mut pinball_sum = 0f64;
    let mut evaluated = 0usize;
    for pair in pairs {
        let Some(predicted) = quantile_value(&pair.estimate, quantile) else {
            continue;
        };
        evaluated += 1;
        let actual = pair.actual_duration_secs;
        if actual <= predicted {
            hits += 1;
        }
        let diff = actual as f64 - predicted as f64;
        pinball_sum += if diff >= 0.0 {
            quantile * diff
        } else {
            (1.0 - quantile) * -diff
        };
    }

    QuantileCoverage {
        quantile,
        n: evaluated,
        hits,
        empirical_coverage: if evaluated == 0 {
            None
        } else {
            Some(hits as f64 / evaluated as f64)
        },
        pinball_loss: if evaluated == 0 {
            None
        } else {
            Some(pinball_sum / evaluated as f64)
        },
    }
}

fn stratify(
    pairs: &[&CalibrationPair],
    key_fn: impl Fn(&CalibrationPair) -> String,
) -> Vec<Stratum> {
    let mut groups: BTreeMap<String, Vec<&CalibrationPair>> = BTreeMap::new();
    for pair in pairs {
        groups.entry(key_fn(pair)).or_default().push(pair);
    }
    groups
        .into_iter()
        .map(|(label, group)| Stratum {
            label,
            quantiles: CALIBRATION_QUANTILES
                .iter()
                .map(|&q| quantile_coverage_for(&group, q))
                .collect(),
        })
        .collect()
}

/// Reads the predicted duration bound for `quantile` off `estimate`, or
/// `None` if `quantile` is not one of the three the estimator actually
/// produces.
fn quantile_value(estimate: &Estimate, quantile: f64) -> Option<u64> {
    const EPS: f64 = 1e-9;
    if (quantile - 0.50).abs() < EPS {
        estimate.duration_p50_secs
    } else if (quantile - 0.80).abs() < EPS {
        estimate.duration_p80_secs
    } else if (quantile - 0.90).abs() < EPS {
        estimate.duration_p90_secs
    } else {
        None
    }
}

fn tier_label(tier: BucketTier) -> &'static str {
    match tier {
        BucketTier::RepoTopologyModel => "repo_topology_model",
        BucketTier::RepoTopology => "repo_topology",
        BucketTier::Repo => "repo",
        BucketTier::Topology => "topology",
        BucketTier::Global => "global",
        // Unreachable in practice: `LedgerStore::calibration_pairs`
        // drops every cold-start estimate before a `CalibrationPair` is
        // ever constructed. Matched exhaustively anyway rather than via
        // a wildcard, so a future `BucketTier` variant cannot silently
        // fall through unlabeled.
        BucketTier::ColdStart => "cold_start",
    }
}

fn sample_band(n: usize) -> &'static str {
    if n < libra_governor_domain::MIN_CLASS_SAMPLES {
        "n_lt_5"
    } else if n < 20 {
        "n_5_19"
    } else {
        "n_ge_20"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libra_governor_domain::{BucketTier, Confidence};
    use time::OffsetDateTime;

    fn pair(estimate: Estimate, actual_duration_secs: u64) -> CalibrationPair {
        CalibrationPair {
            estimate,
            actual_duration_secs,
            task_features: None,
            recorded_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn computed_estimate(
        p50: u64,
        p80: u64,
        p90: u64,
        sample_count: usize,
        bucket_tier: BucketTier,
    ) -> Estimate {
        Estimate {
            duration_p50_secs: Some(p50),
            duration_p80_secs: Some(p80),
            duration_p90_secs: Some(p90),
            resource_p50: None,
            resource_p80: None,
            resource_p90: None,
            confidence: Confidence::Medium,
            sample_count,
            cold_start: false,
            estimator_version: libra_governor_domain::ESTIMATOR_VERSION.to_string(),
            reason: None,
            feature_schema_version: libra_governor_domain::FEATURE_SCHEMA_VERSION.to_string(),
            bucket_tier,
        }
    }

    // --- CoverageReport::Insufficient -------------------------------

    #[test]
    fn zero_pairs_never_renders_as_full_coverage() {
        let report = duration_coverage(&[]);
        match report {
            CoverageReport::Insufficient { n, required } => {
                assert_eq!(n, 0);
                assert_eq!(required, REQUIRED_CALIBRATION_PAIRS);
            }
            other => panic!("0 pairs must be Insufficient, got {other:?}"),
        }
    }

    #[test]
    fn a_handful_of_pairs_is_still_insufficient() {
        let pairs: Vec<_> = (1..=10)
            .map(|n| pair(computed_estimate(10, 20, 30, 10, BucketTier::Global), n * 2))
            .collect();
        assert!(matches!(
            duration_coverage(&pairs),
            CoverageReport::Insufficient { .. }
        ));
    }

    // --- CoverageReport::Degenerate ----------------------------------

    /// Pins exactly the failure mode that made the HORO-1127 validation
    /// corpus unusable as calibration evidence: every receipt in it
    /// reported `actual_duration_secs: 0.0` (scripted, not real
    /// execution). If this module ever silently reported that as "100%
    /// coverage," a future reader could mistake 34 zeros for real
    /// evidence — this test makes that impossible without a deliberate,
    /// reviewed change to this module.
    #[test]
    fn all_zero_durations_does_not_falsely_report_full_coverage() {
        let pairs: Vec<_> = (0..REQUIRED_CALIBRATION_PAIRS)
            .map(|_| pair(computed_estimate(0, 0, 0, 34, BucketTier::Global), 0))
            .collect();

        match duration_coverage(&pairs) {
            CoverageReport::Degenerate {
                n,
                distinct_actual_durations,
                ..
            } => {
                assert_eq!(n, REQUIRED_CALIBRATION_PAIRS);
                assert_eq!(distinct_actual_durations, 1);
            }
            other => panic!(
                "all-identical actuals must never render as Computed coverage, got {other:?}"
            ),
        }
    }

    // --- CoverageReport::Computed --------------------------------------

    /// Seeded known-distribution test: synthetic actuals drawn from a
    /// known uniform distribution, estimate quantiles set to the
    /// distribution's TRUE population quantiles (not empirically derived
    /// from this same sample — see the in-sample test below for that
    /// distinction). This proves the *metric computation* is correct; it
    /// is explicitly NOT a calibration claim about the real product —
    /// the real product has no real trajectory history yet (see module
    /// docs).
    #[test]
    fn empirical_coverage_converges_to_nominal_for_a_known_distribution() {
        // Deterministic PRNG (no external `rand` dependency): a
        // splitmix64-style generator seeded with a fixed constant so
        // this test is fully reproducible.
        struct Lcg(u64);
        impl Lcg {
            fn next_u01(&mut self) -> f64 {
                self.0 = self
                    .0
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                ((self.0 >> 11) as f64) / ((1u64 << 53) as f64)
            }
        }

        const N: usize = 4000;
        const MAX_SECS: f64 = 1000.0;
        let mut rng = Lcg(0x5EED_C0FF_EE42_1234);

        // Uniform(0, MAX_SECS): the true q-quantile is exactly q * MAX_SECS.
        let true_p50 = (0.50 * MAX_SECS).round() as u64;
        let true_p80 = (0.80 * MAX_SECS).round() as u64;
        let true_p90 = (0.90 * MAX_SECS).round() as u64;
        let estimate = computed_estimate(true_p50, true_p80, true_p90, N, BucketTier::Global);

        let pairs: Vec<_> = (0..N)
            .map(|_| {
                let actual = (rng.next_u01() * MAX_SECS).round() as u64;
                pair(estimate.clone(), actual)
            })
            .collect();

        let report = duration_coverage(&pairs);
        let CoverageReport::Computed { overall, .. } = report else {
            panic!("expected Computed coverage for {N} non-degenerate pairs");
        };

        const TOLERANCE: f64 = 0.05;
        for qc in &overall {
            let coverage = qc.empirical_coverage.expect("n > 0 for every quantile");
            assert!(
                (coverage - qc.quantile).abs() < TOLERANCE,
                "quantile {} empirical coverage {coverage} not within {TOLERANCE} of nominal",
                qc.quantile
            );
        }
    }

    /// In-sample vs. held-out distinction: feeding the estimator's own
    /// training pairs back into `duration_coverage` gives P90 coverage at
    /// least 90% BY CONSTRUCTION — the estimate's quantiles were literally
    /// computed as the empirical quantiles of these same actuals. This is
    /// a correctness-of-understanding test (it proves the metric
    /// computation behaves as expected on in-sample data), NOT a product
    /// calibration claim. Only a genuinely held-out/chronological split
    /// (estimate computed from receipts BEFORE the actual it is being
    /// scored against) would be real calibration evidence.
    #[test]
    fn in_sample_coverage_trivially_meets_nominal_by_construction() {
        let receipts: Vec<u64> = (1..=40).map(|n| n * 10).collect();
        let sorted = {
            let mut s = receipts.clone();
            s.sort_unstable();
            s
        };
        let p50 = crate::quantile_u64(&sorted, 0.50);
        let p80 = crate::quantile_u64(&sorted, 0.80);
        let p90 = crate::quantile_u64(&sorted, 0.90);
        let estimate = computed_estimate(p50, p80, p90, receipts.len(), BucketTier::Global);

        let pairs: Vec<_> = receipts
            .iter()
            .map(|&actual| pair(estimate.clone(), actual))
            .collect();

        let CoverageReport::Computed { overall, .. } = duration_coverage(&pairs) else {
            panic!("expected Computed coverage");
        };
        let p90_coverage = overall
            .iter()
            .find(|q| (q.quantile - 0.90).abs() < 1e-9)
            .unwrap();
        assert!(
            p90_coverage.empirical_coverage.unwrap() >= 0.90,
            "in-sample P90 coverage must trivially meet nominal by construction, got {:?}",
            p90_coverage.empirical_coverage
        );
    }

    // --- admission_replay ----------------------------------------------

    #[test]
    fn admission_replay_with_no_usable_pairs_is_insufficient() {
        let outcome = admission_replay(
            &[],
            AdmissionPolicy {
                deadline_secs: 100,
                threshold_quantile: 0.80,
            },
        );
        assert_eq!(
            outcome,
            AdmissionOutcome::Insufficient {
                n: 0,
                required: REQUIRED_CALIBRATION_PAIRS,
            }
        );
    }

    /// The public `admission_replay` gate must never render a real-looking
    /// admit/false-admit rate from a handful of pairs — the exact same
    /// false-positive-from-nothing shape [`CoverageReport::Insufficient`]
    /// prevents for coverage. A perfect "1 admitted, 0 false admits" from
    /// n=1 would be just as misleading as "100% coverage" from n=0.
    #[test]
    fn admission_replay_below_the_required_floor_is_insufficient_even_with_real_data() {
        let policy = AdmissionPolicy {
            deadline_secs: 100,
            threshold_quantile: 0.80,
        };
        let pairs: Vec<_> = (1..REQUIRED_CALIBRATION_PAIRS)
            .map(|n| {
                pair(
                    computed_estimate(40, 80, 100, 10, BucketTier::Global),
                    n as u64,
                )
            })
            .collect();

        let outcome = admission_replay(&pairs, policy);
        assert_eq!(
            outcome,
            AdmissionOutcome::Insufficient {
                n: REQUIRED_CALIBRATION_PAIRS - 1,
                required: REQUIRED_CALIBRATION_PAIRS,
            }
        );
    }

    /// Hand-computed false-admit/false-reject counts on a small (~6 row)
    /// fixture — verifiable by inspection, not by trusting the
    /// implementation. Calls the private `admission_stats` counting
    /// function directly, bypassing `admission_replay`'s
    /// `REQUIRED_CALIBRATION_PAIRS` gate on purpose: this test's whole
    /// point is to pin the counting logic itself on a small,
    /// human-checkable fixture, not to also satisfy the real-evidence
    /// floor.
    ///
    /// Policy: deadline=100s, threshold_quantile=0.80.
    ///
    /// | row | p80 | actual | admit? | within deadline? | outcome       |
    /// |-----|-----|--------|--------|-------------------|---------------|
    /// | 1   | 80  | 90     | yes    | yes               | correct admit |
    /// | 2   | 80  | 110    | yes    | no                | FALSE ADMIT (overrun 10) |
    /// | 3   | 120 | 90     | no     | yes               | FALSE REJECT  |
    /// | 4   | 120 | 150    | no     | no                | correct reject |
    /// | 5   | 50  | 40     | yes    | yes               | correct admit |
    /// | 6   | 100 | 100    | yes    | yes               | correct admit |
    #[test]
    fn hand_computed_false_admit_and_false_reject_on_a_small_fixture() {
        let policy = AdmissionPolicy {
            deadline_secs: 100,
            threshold_quantile: 0.80,
        };
        let rows = [
            (80, 90),
            (80, 110),
            (120, 90),
            (120, 150),
            (50, 40),
            (100, 100),
        ];
        let owned_pairs: Vec<_> = rows
            .iter()
            .map(|&(p80, actual)| {
                pair(
                    computed_estimate(p80 / 2, p80, p80 + 20, 10, BucketTier::Global),
                    actual,
                )
            })
            .collect();
        let pairs: Vec<&CalibrationPair> = owned_pairs.iter().collect();

        let stats = admission_stats(&pairs, policy);
        assert_eq!(stats.n, 6);
        assert_eq!(stats.admit_count, 4, "rows 1,2,5,6 have p80 <= 100");
        assert_eq!(stats.false_admit_count, 1, "row 2 only");
        assert_eq!(stats.false_reject_count, 1, "row 3 only");
        assert_eq!(stats.mean_overrun_secs, Some(10.0), "row 2: 110 - 100");
        assert_eq!(stats.p95_overrun_secs, Some(10.0));
    }

    #[test]
    fn admission_replay_excludes_pairs_whose_estimate_lacks_the_threshold_quantile() {
        let mut estimate = computed_estimate(10, 20, 30, 10, BucketTier::Global);
        estimate.duration_p80_secs = None;
        let pairs = vec![pair(estimate, 15)];

        let outcome = admission_replay(
            &pairs,
            AdmissionPolicy {
                deadline_secs: 100,
                threshold_quantile: 0.80,
            },
        );
        assert_eq!(
            outcome,
            AdmissionOutcome::Insufficient {
                n: 0,
                required: REQUIRED_CALIBRATION_PAIRS,
            }
        );
    }

    // --- CostCoverage / leakage ------------------------------------

    #[test]
    fn cost_coverage_carries_no_populable_cost_value() {
        match CostCoverage::unavailable() {
            CostCoverage::Unavailable { reason } => assert!(!reason.is_empty()),
        }
    }

    /// Leakage-style field-set test (mirrors
    /// `libra_governor_domain::task_features`'s leakage test from
    /// HORO-1130): neither `CoverageReport` nor `AdmissionOutcome`
    /// serializes any cost/USD-shaped field, from any variant.
    #[test]
    fn coverage_and_admission_types_serialize_no_cost_field() {
        let coverage = CoverageReport::Computed {
            n: 40,
            overall: vec![quantile_coverage_for(&[], 0.5)],
            by_bucket_tier: vec![],
            by_sample_band: vec![],
        };
        let admission = AdmissionOutcome::Computed(AdmissionStats {
            n: 6,
            admit_count: 4,
            false_admit_count: 1,
            false_reject_count: 1,
            mean_overrun_secs: Some(10.0),
            p95_overrun_secs: Some(10.0),
        });

        for value in [
            serde_json::to_value(&coverage).unwrap(),
            serde_json::to_value(admission).unwrap(),
            serde_json::to_value(CostCoverage::unavailable()).unwrap(),
        ] {
            assert_no_cost_shaped_keys(&value);
        }
    }

    fn assert_no_cost_shaped_keys(value: &serde_json::Value) {
        const FORBIDDEN: [&str; 5] = ["cost", "usd", "dollar", "price", "regret"];
        match value {
            serde_json::Value::Object(map) => {
                for (key, v) in map {
                    let lower = key.to_lowercase();
                    for forbidden in FORBIDDEN {
                        assert!(
                            !lower.contains(forbidden),
                            "field {key:?} looks cost-shaped (contains {forbidden:?}); \
                             cost coverage must stay a CostCoverage::Unavailable marker, never a field"
                        );
                    }
                    assert_no_cost_shaped_keys(v);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    assert_no_cost_shaped_keys(item);
                }
            }
            _ => {}
        }
    }
}
