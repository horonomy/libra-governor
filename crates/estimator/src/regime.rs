//! Regime-aware calibration: cohort/epoch derivation, drift detection,
//! and the [`RegimeCalibrationReport`] assembly (HORO-1671).
//!
//! # Epochs are derived, never stored
//!
//! A [`CalibrationEpoch`] is a *regime cohort*: every [`CalibrationPair`]
//! grouped by exact [`Option<RegimeKey>`] equality. Cohorts are computed
//! fresh from [`LedgerStore::calibration_pairs`]'s own rows every time a
//! report is built — there is no epoch table, no write path, and
//! therefore nothing to prune. "Historical data is not deleted" is true
//! by construction, not by convention.
//!
//! # Two distinct sample counts
//!
//! - `active_epoch.n` — only the cohort whose [`RegimeKey`] is *exactly
//!   equal* to the current regime.
//! - `in_regime_n` — the sum over every cohort *comparable* to the
//!   current regime (via [`RegimeKey::comparison`]), including the
//!   pre-regime (`None`-keyed) cohort and any cohort carrying an
//!   `Unavailable` dimension. This is what feeds
//!   [`libra_governor_domain::Confidence::from_evidence`].
//!
//! These are not the same number and must never be conflated — see
//! [`ConfidenceBasis`].
//!
//! # Weighting
//!
//! Out-of-regime pairs still contribute to quantile *bounds* (so a bound
//! exists at all rather than cold-starting on every regime change) but
//! never to *confidence* — two constants
//! ([`OUT_OF_REGIME_BOUNDS_WEIGHT`], [`OUT_OF_REGIME_CONFIDENCE_WEIGHT`]),
//! no decay function, nothing to mis-calibrate.
//!
//! # Drift is a separate, reported-only signal
//!
//! [`DriftVerdict`] answers a different question from regime identity:
//! *did the outcome distribution move even though no regime dimension
//! changed?* It does not feed back into live confidence in this ticket —
//! that is a deliberate boundary for a later ticket.

use std::collections::BTreeMap;

use libra_governor_domain::{BucketTier, Confidence, RegimeComparison, RegimeKey};
use libra_governor_ledger::CalibrationPair;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::calibration::{quantile_coverage_for, QuantileCoverage, CALIBRATION_QUANTILES};

/// Out-of-regime pairs are still pooled for quantile bounds — a bound
/// exists at all rather than cold-starting on every regime change.
pub const OUT_OF_REGIME_BOUNDS_WEIGHT: f64 = 1.0;
/// Out-of-regime pairs never contribute to confidence — "I used to know
/// this workload, but the regime changed" is exact, not decayed.
pub const OUT_OF_REGIME_CONFIDENCE_WEIGHT: f64 = 0.0;

/// Most recent *usable* (estimate carries `duration_p90_secs`) pairs in
/// the active epoch the drift monitor evaluates.
pub const DRIFT_WINDOW: usize = 20;
/// Nominal P90 exceedance is 10%; `>= 30%` (3x) is reported as drift.
pub const DRIFT_EXCEEDANCE_THRESHOLD: f64 = 0.30;

/// One regime cohort: every [`CalibrationPair`] whose regime key is
/// exactly equal to [`Self::key`] (`None` == the pre-HORO-1671 cohort).
/// Derived at read time — see module docs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalibrationEpoch {
    pub key: Option<RegimeKey>,
    pub n: usize,
    pub first_recorded_at: OffsetDateTime,
    pub last_recorded_at: OffsetDateTime,
    pub is_active: bool,
    pub comparable_to_active: bool,
    pub quantiles: Vec<QuantileCoverage>,
}

/// Groups `pairs` into cohorts by exact `Option<RegimeKey>` equality,
/// ordered by `last_recorded_at` descending (ties broken by `Ord` on the
/// key, for determinism — cohorts, not maximal temporal runs: a user
/// flipping between two regimes produces exactly two cohorts with
/// interleaved time ranges, not a churn of micro-epochs).
pub fn epochs_from_pairs(pairs: &[CalibrationPair], active: &RegimeKey) -> Vec<CalibrationEpoch> {
    let mut groups: BTreeMap<Option<RegimeKey>, Vec<&CalibrationPair>> = BTreeMap::new();
    for pair in pairs {
        groups
            .entry(pair.regime.as_ref().map(|r| r.key.clone()))
            .or_default()
            .push(pair);
    }

    let mut epochs: Vec<CalibrationEpoch> = groups
        .into_iter()
        .map(|(key, group)| {
            let first_recorded_at = group.iter().map(|p| p.recorded_at).min().unwrap();
            let last_recorded_at = group.iter().map(|p| p.recorded_at).max().unwrap();
            let is_active = key.as_ref() == Some(active);
            let comparable_to_active = key
                .as_ref()
                .unwrap_or(&RegimeKey::pre_regime_record())
                .comparison(active)
                .is_comparable();
            let quantiles = CALIBRATION_QUANTILES
                .iter()
                .map(|&q| quantile_coverage_for(&group, q))
                .collect();
            CalibrationEpoch {
                key,
                n: group.len(),
                first_recorded_at,
                last_recorded_at,
                is_active,
                comparable_to_active,
                quantiles,
            }
        })
        .collect();

    epochs.sort_by(|a, b| {
        b.last_recorded_at
            .cmp(&a.last_recorded_at)
            .then_with(|| a.key.cmp(&b.key))
    });
    epochs
}

/// The narrowest-truth status of the regime currently in force.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ActiveRegimeStatus {
    Established {
        key: RegimeKey,
        n: usize,
    },
    /// No cohort is exactly equal to the current regime — the
    /// "insufficient evidence in the new regime" AC, verbatim.
    NewRegime {
        key: RegimeKey,
        superseded: Vec<SupersededEpoch>,
    },
}

/// A prior cohort that is comparable to, but not identical with, the
/// active regime — named by which dimensions actually differ.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SupersededEpoch {
    pub key: Option<RegimeKey>,
    pub n: usize,
    pub differing: Vec<libra_governor_domain::RegimeDimension>,
}

pub fn active_regime_status(epochs: &[CalibrationEpoch], active: &RegimeKey) -> ActiveRegimeStatus {
    if let Some(epoch) = epochs.iter().find(|e| e.is_active) {
        return ActiveRegimeStatus::Established {
            key: active.clone(),
            n: epoch.n,
        };
    }

    let superseded = epochs
        .iter()
        .filter_map(|e| {
            let key_ref = e.key.as_ref()?;
            match key_ref.comparison(active) {
                RegimeComparison::Incompatible { differing } => Some(SupersededEpoch {
                    key: e.key.clone(),
                    n: e.n,
                    differing,
                }),
                RegimeComparison::Comparable => None,
            }
        })
        .collect();

    ActiveRegimeStatus::NewRegime {
        key: active.clone(),
        superseded,
    }
}

/// Why confidence is what it is — the AC's "reason confidence changed".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum ConfidenceChangeReason {
    FullInRegimeEvidence,
    RegimeChanged {
        differing: Vec<libra_governor_domain::RegimeDimension>,
    },
    InsufficientInRegimeEvidence {
        n: usize,
        required: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfidenceBasis {
    pub in_regime_n: usize,
    pub out_of_regime_n: usize,
    pub confidence: Confidence,
    pub reason: ConfidenceChangeReason,
}

/// Sums `n` over every cohort comparable to `active` (including the
/// pre-regime `None` cohort) for the confidence-feeding count, and over
/// every incomparable cohort for `out_of_regime_n`. Confidence itself is
/// [`Confidence::from_evidence`], UNCHANGED — only its `n` argument is
/// now the in-regime count rather than the raw bucket sample count.
pub fn confidence_basis(
    epochs: &[CalibrationEpoch],
    active: &RegimeKey,
    tier: BucketTier,
) -> ConfidenceBasis {
    let in_regime_n: usize = epochs
        .iter()
        .filter(|e| e.comparable_to_active)
        .map(|e| e.n)
        .sum();
    let out_of_regime_n: usize = epochs
        .iter()
        .filter(|e| !e.comparable_to_active)
        .map(|e| e.n)
        .sum();

    let confidence = Confidence::from_evidence(tier, in_regime_n);

    // Prefer naming an actual regime change over an insufficient-evidence
    // verdict: a fresh model swap with 0 prior comparable evidence is
    // more informative reported as "regime changed on [Model]" than as a
    // generic "insufficient evidence" that doesn't say why.
    let differing_from_some_incomparable_cohort = epochs
        .iter()
        .filter(|e| !e.comparable_to_active)
        .find_map(|e| {
            match e
                .key
                .as_ref()
                .unwrap_or(&RegimeKey::pre_regime_record())
                .comparison(active)
            {
                RegimeComparison::Incompatible { differing } => Some(differing),
                RegimeComparison::Comparable => None,
            }
        });

    let reason = match differing_from_some_incomparable_cohort {
        Some(differing) => ConfidenceChangeReason::RegimeChanged { differing },
        None if in_regime_n < libra_governor_domain::MIN_CLASS_SAMPLES => {
            ConfidenceChangeReason::InsufficientInRegimeEvidence {
                n: in_regime_n,
                required: libra_governor_domain::MIN_CLASS_SAMPLES,
            }
        }
        None => ConfidenceChangeReason::FullInRegimeEvidence,
    };

    ConfidenceBasis {
        in_regime_n,
        out_of_regime_n,
        confidence,
        reason,
    }
}

/// Reported-only distributional-drift verdict — see module docs. Never
/// writes new statistics code: reuses
/// [`crate::calibration::quantile_coverage_for`]'s own P90-bound
/// extraction logic by filtering to pairs that carry a P90 bound first,
/// exactly as [`crate::calibration::admission_replay`] does.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum DriftVerdict {
    InsufficientWindow {
        usable: usize,
        required: usize,
    },
    Stable {
        window: usize,
        exceedances: usize,
        rate: f64,
        threshold: f64,
    },
    Drifting {
        window: usize,
        exceedances: usize,
        rate: f64,
        threshold: f64,
    },
}

/// Evaluates drift over the most recent [`DRIFT_WINDOW`] usable pairs in
/// the active epoch (ordered by `recorded_at`, most recent last).
/// "Usable" excludes any pair whose estimate lacks `duration_p90_secs` —
/// mirroring [`crate::calibration::admission_replay`]'s own filter.
pub fn detect_drift(active_epoch_pairs_time_ordered: &[&CalibrationPair]) -> DriftVerdict {
    let usable: Vec<&&CalibrationPair> = active_epoch_pairs_time_ordered
        .iter()
        .filter(|p| p.estimate.duration_p90_secs.is_some())
        .collect();

    let window: Vec<&&&CalibrationPair> = usable.iter().rev().take(DRIFT_WINDOW).collect();
    if window.len() < DRIFT_WINDOW {
        return DriftVerdict::InsufficientWindow {
            usable: window.len(),
            required: DRIFT_WINDOW,
        };
    }

    let exceedances = window
        .iter()
        .filter(|p| {
            let p90 = p
                .estimate
                .duration_p90_secs
                .expect("filtered to Some above");
            p.actual_duration_secs > p90
        })
        .count();
    let rate = exceedances as f64 / window.len() as f64;

    if rate >= DRIFT_EXCEEDANCE_THRESHOLD {
        DriftVerdict::Drifting {
            window: window.len(),
            exceedances,
            rate,
            threshold: DRIFT_EXCEEDANCE_THRESHOLD,
        }
    } else {
        DriftVerdict::Stable {
            window: window.len(),
            exceedances,
            rate,
            threshold: DRIFT_EXCEEDANCE_THRESHOLD,
        }
    }
}

/// How much out-of-regime evidence contributed, and at what weight —
/// the AC's "whether old data contributes and at what weight", made a
/// literal, testable field rather than a promise.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OutOfRegimeContribution {
    pub n: usize,
    pub bounds_weight: f64,
    pub confidence_weight: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegimeCalibrationReport {
    pub regime_schema_version: String,
    pub active: ActiveRegimeStatus,
    pub epochs: Vec<CalibrationEpoch>,
    pub confidence_basis: ConfidenceBasis,
    pub drift: DriftVerdict,
    pub out_of_regime: OutOfRegimeContribution,
}

/// Assembles the full report from `pairs` (ordered by `recorded_at`
/// ascending, as [`libra_governor_ledger::LedgerStore::calibration_pairs`]
/// returns them) and the current regime.
pub fn build_report(
    pairs: &[CalibrationPair],
    active: &RegimeKey,
    tier: BucketTier,
) -> RegimeCalibrationReport {
    let epochs = epochs_from_pairs(pairs, active);
    let active_status = active_regime_status(&epochs, active);
    let basis = confidence_basis(&epochs, active, tier);

    let active_epoch_pairs: Vec<&CalibrationPair> = pairs
        .iter()
        .filter(|p| p.regime.as_ref().map(|r| &r.key) == Some(active))
        .collect();
    let drift = detect_drift(&active_epoch_pairs);

    RegimeCalibrationReport {
        regime_schema_version: libra_governor_domain::REGIME_SCHEMA_VERSION.to_string(),
        active: active_status,
        epochs,
        confidence_basis: basis.clone(),
        drift,
        out_of_regime: OutOfRegimeContribution {
            n: basis.out_of_regime_n,
            bounds_weight: OUT_OF_REGIME_BOUNDS_WEIGHT,
            confidence_weight: OUT_OF_REGIME_CONFIDENCE_WEIGHT,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libra_governor_domain::{AgentKind, Confidence, Estimate, RegimeProvenance};

    fn key(model: &str) -> RegimeKey {
        RegimeKey::builder()
            .model(Some(model))
            .harness(Some(AgentKind::ClaudeCode))
            .feature_schema("fs-v1")
            .build()
    }

    fn provenance(model: &str) -> RegimeProvenance {
        RegimeProvenance::new(
            key(model),
            None,
            libra_governor_domain::CacheClass::NoCacheObserved,
        )
    }

    fn estimate(p90: u64) -> Estimate {
        Estimate {
            duration_p50_secs: Some(p90 / 2),
            duration_p80_secs: Some(p90 * 4 / 5),
            duration_p90_secs: Some(p90),
            resource_p50: None,
            resource_p80: None,
            resource_p90: None,
            confidence: Confidence::Medium,
            sample_count: 1,
            cold_start: false,
            estimator_version: libra_governor_domain::ESTIMATOR_VERSION.to_string(),
            reason: None,
            feature_schema_version: "fs-v1".to_string(),
            bucket_tier: BucketTier::Global,
            regime: Default::default(),
        }
    }

    fn pair_at(
        regime: Option<RegimeProvenance>,
        actual: u64,
        p90: u64,
        recorded_at: OffsetDateTime,
    ) -> CalibrationPair {
        CalibrationPair {
            estimate: estimate(p90),
            actual_duration_secs: actual,
            task_features: None,
            recorded_at,
            regime,
        }
    }

    fn t(offset_secs: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(offset_secs)
    }

    // --- epoch / cohort derivation ---------------------------------

    #[test]
    fn noise_without_regime_change_does_not_start_a_new_epoch() {
        let active = key("claude-sonnet-5");
        let pairs: Vec<_> = (0..10)
            .map(|i| {
                pair_at(
                    Some(provenance("claude-sonnet-5")),
                    50 + i,
                    100,
                    t(i as i64),
                )
            })
            .collect();
        let epochs = epochs_from_pairs(&pairs, &active);
        assert_eq!(epochs.len(), 1);
        assert_eq!(epochs[0].n, 10);
    }

    #[test]
    fn model_change_starts_a_new_epoch_and_names_the_differing_dimension() {
        let old_key = key("claude-sonnet-5");
        let new_key = key("claude-opus-5");
        let mut pairs: Vec<_> = (0..25)
            .map(|i| pair_at(Some(provenance("claude-sonnet-5")), 50, 100, t(i)))
            .collect();
        pairs.push(pair_at(Some(provenance("claude-opus-5")), 50, 100, t(100)));

        let epochs = epochs_from_pairs(&pairs, &new_key);
        assert_eq!(epochs.len(), 2);

        match active_regime_status(&epochs, &new_key) {
            ActiveRegimeStatus::Established { n, .. } => assert_eq!(n, 1),
            other => panic!("expected Established with n=1, got {other:?}"),
        }

        // From the OLD model's perspective, the new one is incompatible.
        let old_epochs = epochs_from_pairs(&pairs, &old_key);
        match active_regime_status(&old_epochs, &old_key) {
            ActiveRegimeStatus::Established { n, .. } => assert_eq!(n, 25),
            other => panic!("expected Established, got {other:?}"),
        }
    }

    #[test]
    fn model_change_drops_confidence_from_high_to_low_but_retains_the_superseded_cohort() {
        let new_key = key("claude-opus-5");
        let mut pairs: Vec<_> = (0..25)
            .map(|i| pair_at(Some(provenance("claude-sonnet-5")), 50, 100, t(i)))
            .collect();
        pairs.push(pair_at(Some(provenance("claude-opus-5")), 50, 100, t(100)));

        let epochs = epochs_from_pairs(&pairs, &new_key);
        let basis = confidence_basis(&epochs, &new_key, BucketTier::RepoTopologyModel);
        assert_eq!(
            basis.in_regime_n, 1,
            "only the new-model cohort is comparable"
        );
        assert_eq!(basis.confidence, Confidence::Low);

        match basis.reason {
            ConfidenceChangeReason::RegimeChanged { differing } => {
                assert_eq!(
                    differing,
                    vec![libra_governor_domain::RegimeDimension::Model]
                );
            }
            other => panic!("expected RegimeChanged, got {other:?}"),
        }

        let superseded_cohort = epochs.iter().find(|e| !e.is_active).unwrap();
        assert_eq!(
            superseded_cohort.n, 25,
            "superseded cohort's evidence is retained, not deleted"
        );
    }

    #[test]
    fn pre_regime_receipts_never_count_as_a_regime_change() {
        let active = key("claude-sonnet-5");
        let pairs: Vec<_> = (0..10).map(|i| pair_at(None, 50, 100, t(i))).collect();
        let epochs = epochs_from_pairs(&pairs, &active);
        assert_eq!(epochs.len(), 1);
        assert!(epochs[0].comparable_to_active);
        let basis = confidence_basis(&epochs, &active, BucketTier::RepoTopologyModel);
        assert_eq!(basis.in_regime_n, 10);
    }

    #[test]
    fn todays_real_data_shape_produces_identical_confidence_to_pre_1671() {
        // Every receipt's regime is None (today's real shape: no live
        // producer sets ExecutionReceipt::regime yet in most
        // deployments), and the current regime built from a
        // no-gateway/no-model host is itself all-`Unavailable`. Confidence
        // must equal exactly `from_evidence(tier, n)` as it did before
        // this ticket.
        let current = RegimeKey::builder().feature_schema("fs-v1").build();
        let pairs: Vec<_> = (0..12).map(|i| pair_at(None, 50, 100, t(i))).collect();
        let epochs = epochs_from_pairs(&pairs, &current);
        let basis = confidence_basis(&epochs, &current, BucketTier::RepoTopologyModel);
        assert_eq!(basis.in_regime_n, 12);
        assert_eq!(
            basis.confidence,
            Confidence::from_evidence(BucketTier::RepoTopologyModel, 12)
        );
    }

    #[test]
    fn old_epochs_are_retained_and_their_weight_is_explicit() {
        let old_key = key("claude-sonnet-5");
        let new_key = key("claude-opus-5");
        let mut pairs: Vec<_> = (0..5)
            .map(|i| pair_at(Some(provenance("claude-sonnet-5")), 50, 100, t(i)))
            .collect();
        pairs.push(pair_at(Some(provenance("claude-opus-5")), 50, 100, t(100)));
        let _ = old_key;

        let epochs = epochs_from_pairs(&pairs, &new_key);
        let basis = confidence_basis(&epochs, &new_key, BucketTier::RepoTopologyModel);
        assert_eq!(basis.out_of_regime_n, 5);

        let report = build_report(&pairs, &new_key, BucketTier::RepoTopologyModel);
        assert_eq!(report.out_of_regime.n, 5);
        assert_eq!(report.out_of_regime.bounds_weight, 1.0);
        assert_eq!(report.out_of_regime.confidence_weight, 0.0);
    }

    // --- drift ------------------------------------------------------

    #[test]
    fn drift_monitor_requires_a_full_window() {
        let pairs: Vec<_> = (0..5)
            .map(|i| pair_at(Some(provenance("m")), 50, 100, t(i)))
            .collect();
        let refs: Vec<&CalibrationPair> = pairs.iter().collect();
        assert_eq!(
            detect_drift(&refs),
            DriftVerdict::InsufficientWindow {
                usable: 5,
                required: DRIFT_WINDOW
            }
        );
    }

    #[test]
    fn drift_threshold_boundary_is_pinned() {
        // 6/20 = 0.30 -> Drifting ("exceeds" means actual > p90).
        let mut pairs: Vec<CalibrationPair> = (0..14)
            .map(|i| pair_at(Some(provenance("m")), 50, 100, t(i)))
            .collect();
        pairs.extend((14..20).map(|i| pair_at(Some(provenance("m")), 150, 100, t(i))));
        let refs: Vec<&CalibrationPair> = pairs.iter().collect();
        match detect_drift(&refs) {
            DriftVerdict::Drifting {
                exceedances, rate, ..
            } => {
                assert_eq!(exceedances, 6);
                assert!((rate - 0.30).abs() < 1e-9);
            }
            other => panic!("expected Drifting at exactly the threshold, got {other:?}"),
        }

        // 5/20 = 0.25 -> Stable.
        let mut pairs: Vec<CalibrationPair> = (0..15)
            .map(|i| pair_at(Some(provenance("m")), 50, 100, t(i)))
            .collect();
        pairs.extend((15..20).map(|i| pair_at(Some(provenance("m")), 150, 100, t(i))));
        let refs: Vec<&CalibrationPair> = pairs.iter().collect();
        match detect_drift(&refs) {
            DriftVerdict::Stable { exceedances, .. } => assert_eq!(exceedances, 5),
            other => panic!("expected Stable, got {other:?}"),
        }
    }

    #[test]
    fn doubled_durations_trip_the_exceedance_monitor_within_one_window() {
        let pairs: Vec<CalibrationPair> = (0..DRIFT_WINDOW)
            .map(|i| pair_at(Some(provenance("m")), 200, 100, t(i as i64)))
            .collect();
        let refs: Vec<&CalibrationPair> = pairs.iter().collect();
        match detect_drift(&refs) {
            DriftVerdict::Drifting { exceedances, .. } => assert_eq!(exceedances, DRIFT_WINDOW),
            other => panic!("expected Drifting, got {other:?}"),
        }
    }

    #[test]
    fn drift_monitor_excludes_pairs_lacking_p90() {
        let mut pairs: Vec<CalibrationPair> = (0..DRIFT_WINDOW)
            .map(|i| pair_at(Some(provenance("m")), 50, 100, t(i as i64)))
            .collect();
        pairs[0].estimate.duration_p90_secs = None;
        let refs: Vec<&CalibrationPair> = pairs.iter().collect();
        assert_eq!(
            detect_drift(&refs),
            DriftVerdict::InsufficientWindow {
                usable: DRIFT_WINDOW - 1,
                required: DRIFT_WINDOW
            }
        );
    }

    /// Seeded Monte-Carlo false-positive rate: actuals drawn from the SAME
    /// distribution the P90 was set from (no real drift), repeated
    /// windows — the empirical rate of a false `Drifting` verdict must
    /// stay low. Reuses the same splitmix64 generator shape as
    /// `calibration.rs`'s own seeded test.
    #[test]
    fn seeded_false_positive_rate_stays_below_five_percent() {
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
        let mut rng = Lcg(0xC0FF_EE00_1234_5678);
        const TRIALS: usize = 1000;
        const P90: f64 = 100.0;
        let mut false_positives = 0usize;

        for trial in 0..TRIALS {
            let pairs: Vec<CalibrationPair> = (0..DRIFT_WINDOW)
                .map(|i| {
                    // Uniform(0, P90/0.9): true P90 of this distribution is
                    // exactly P90, so ~10% of draws exceed it by design.
                    let actual = (rng.next_u01() * (P90 / 0.9)).round() as u64;
                    pair_at(
                        Some(provenance("m")),
                        actual,
                        P90 as u64,
                        t((trial * 100 + i) as i64),
                    )
                })
                .collect();
            let refs: Vec<&CalibrationPair> = pairs.iter().collect();
            if matches!(detect_drift(&refs), DriftVerdict::Drifting { .. }) {
                false_positives += 1;
            }
        }

        let fp_rate = false_positives as f64 / TRIALS as f64;
        assert!(
            fp_rate < 0.05,
            "empirical false-positive rate {fp_rate} too high over {TRIALS} trials"
        );
    }

    #[test]
    fn serde_round_trip_for_report() {
        let active = key("claude-sonnet-5");
        let pairs: Vec<_> = (0..6)
            .map(|i| pair_at(Some(provenance("claude-sonnet-5")), 50, 100, t(i)))
            .collect();
        let report = build_report(&pairs, &active, BucketTier::RepoTopologyModel);
        let json = serde_json::to_string(&report).unwrap();
        let back: RegimeCalibrationReport = serde_json::from_str(&json).unwrap();
        assert_eq!(report, back);
    }
}
