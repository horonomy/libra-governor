//! [`Confidence`] — how much a producer trusts a result it computed
//! without human confirmation.
//!
//! Originally introduced by the daemon/protocol crates (HORO-1125) for
//! [`crate::CompletionContract`] drafting confidence. Lives in this crate
//! (rather than `libra-governor-protocol`) because [`crate::Estimate`] also
//! needs it and `libra-governor-domain` must not depend on
//! `libra-governor-protocol` (dependency direction runs the other way).
//! `libra-governor-protocol` re-exports this type so existing callers are
//! unaffected.

use serde::{Deserialize, Serialize};

use crate::task_features::BucketTier;

/// Minimum number of same-task-class samples required to prefer a
/// class-bucketed history tier over the full global history, and (as of
/// HORO-1132) the low/medium threshold [`Confidence::from_evidence`]
/// applies to a narrow (non-`Global`, non-`ColdStart`) tier.
///
/// Canonical home for this value: `libra-governor-estimator` re-exports
/// it (`libra_governor_estimator::MIN_CLASS_SAMPLES`) rather than
/// redeclaring it, so the estimator's bucket ladder and this crate's
/// confidence rule can never silently drift apart. Lives here rather
/// than in the estimator crate because `Confidence::from_evidence` needs
/// it too and `libra-governor-domain` must not depend on
/// `libra-governor-estimator` (dependency direction runs the other way).
pub const MIN_CLASS_SAMPLES: usize = 5;

/// How much a producer trusts a result it computed without human
/// confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// Insufficient evidence: an explicit reason is carried alongside
    /// (e.g. [`crate::ReconSummary`]-equivalent `reason` fields, or
    /// [`crate::Estimate::reason`]) rather than silently expanding scope
    /// or guessing confidently.
    Low,
    Medium,
    High,
}

impl Confidence {
    /// A simple, defensible confidence-from-sample-count rule for
    /// estimator output: fewer than 5 samples is not enough to trust,
    /// 5-19 is a reasonable but still thin basis, 20+ is a solid basis.
    /// These thresholds are a deliberately simple MVP 1.0 choice, not a
    /// calibrated statistical result — revisit once real usage data
    /// exists to validate them against actual estimate error.
    ///
    /// Superseded by [`Self::from_evidence`] (HORO-1132), which also
    /// accounts for *which* [`BucketTier`] the sample count came from —
    /// a raw sample count alone cannot distinguish "20 samples from this
    /// exact repo+topology+model" from "20 samples pooled across every
    /// unrelated task class this machine has ever seen," and only the
    /// former is real evidence of relevance. Kept (not removed) because
    /// removing it would be a behavior change for any caller outside
    /// this workspace that already depends on it; no in-repo caller uses
    /// it after HORO-1132 wires `libra-governor-estimator::compute` onto
    /// [`Self::from_evidence`] instead.
    #[deprecated(
        since = "0.0.0",
        note = "use Confidence::from_evidence(tier, n) instead — a bare sample count cannot tell a narrow, relevant bucket from a large pool of unrelated task classes"
    )]
    pub fn from_sample_count(n: usize) -> Self {
        match n {
            0..=4 => Confidence::Low,
            5..=19 => Confidence::Medium,
            _ => Confidence::High,
        }
    }

    /// Evidence-based confidence rule (HORO-1132): how much to trust an
    /// estimate depends on both how many samples it rests on *and* how
    /// specific to the task at hand those samples were.
    ///
    /// - [`BucketTier::ColdStart`]: always [`Confidence::Low`] — there is
    ///   no local history at all.
    /// - [`BucketTier::Global`]: capped at [`Confidence::Medium`]
    ///   regardless of `n`. A large pool of mixed, unrelated task classes
    ///   does not become more *relevant* to the task at hand just because
    ///   it is bigger — only more populous.
    /// - Any narrower tier (`RepoTopologyModel` / `RepoTopology` / `Repo`
    ///   / `Topology`): `n < `[`MIN_CLASS_SAMPLES`] is [`Confidence::Low`]
    ///   (too thin to have meaningfully informed the bucket-ladder choice
    ///   in the first place), `n` in `[MIN_CLASS_SAMPLES, 20)` is
    ///   [`Confidence::Medium`], `n >= 20` is [`Confidence::High`].
    pub fn from_evidence(tier: BucketTier, n: usize) -> Self {
        match tier {
            BucketTier::ColdStart => Confidence::Low,
            BucketTier::Global => Confidence::Medium,
            BucketTier::RepoTopologyModel
            | BucketTier::RepoTopology
            | BucketTier::Repo
            | BucketTier::Topology => {
                if n < MIN_CLASS_SAMPLES {
                    Confidence::Low
                } else if n < 20 {
                    Confidence::Medium
                } else {
                    Confidence::High
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(deprecated)]
    fn from_sample_count_thresholds() {
        assert_eq!(Confidence::from_sample_count(0), Confidence::Low);
        assert_eq!(Confidence::from_sample_count(4), Confidence::Low);
        assert_eq!(Confidence::from_sample_count(5), Confidence::Medium);
        assert_eq!(Confidence::from_sample_count(19), Confidence::Medium);
        assert_eq!(Confidence::from_sample_count(20), Confidence::High);
        assert_eq!(Confidence::from_sample_count(1000), Confidence::High);
    }

    #[test]
    fn from_evidence_cold_start_is_always_low() {
        assert_eq!(
            Confidence::from_evidence(BucketTier::ColdStart, 0),
            Confidence::Low
        );
        assert_eq!(
            Confidence::from_evidence(BucketTier::ColdStart, 1000),
            Confidence::Low
        );
    }

    #[test]
    fn from_evidence_global_is_capped_at_medium_regardless_of_n() {
        assert_eq!(
            Confidence::from_evidence(BucketTier::Global, 0),
            Confidence::Medium
        );
        assert_eq!(
            Confidence::from_evidence(BucketTier::Global, 5),
            Confidence::Medium
        );
        assert_eq!(
            Confidence::from_evidence(BucketTier::Global, 1_000_000),
            Confidence::Medium,
            "a bigger pool of unrelated task classes is not more relevant, only more populous"
        );
    }

    #[test]
    fn from_evidence_narrow_tiers_follow_the_min_class_samples_threshold() {
        for tier in [
            BucketTier::RepoTopologyModel,
            BucketTier::RepoTopology,
            BucketTier::Repo,
            BucketTier::Topology,
        ] {
            assert_eq!(
                Confidence::from_evidence(tier, 0),
                Confidence::Low,
                "{tier:?} with 0 samples must be Low"
            );
            assert_eq!(
                Confidence::from_evidence(tier, MIN_CLASS_SAMPLES - 1),
                Confidence::Low,
                "{tier:?} just below MIN_CLASS_SAMPLES must be Low"
            );
            assert_eq!(
                Confidence::from_evidence(tier, MIN_CLASS_SAMPLES),
                Confidence::Medium,
                "{tier:?} at MIN_CLASS_SAMPLES must be Medium"
            );
            assert_eq!(
                Confidence::from_evidence(tier, 19),
                Confidence::Medium,
                "{tier:?} at 19 must still be Medium"
            );
            assert_eq!(
                Confidence::from_evidence(tier, 20),
                Confidence::High,
                "{tier:?} at 20 must be High"
            );
            assert_eq!(
                Confidence::from_evidence(tier, 1000),
                Confidence::High,
                "{tier:?} with many samples must be High"
            );
        }
    }
}
