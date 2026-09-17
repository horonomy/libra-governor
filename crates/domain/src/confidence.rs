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
    pub fn from_sample_count(n: usize) -> Self {
        match n {
            0..=4 => Confidence::Low,
            5..=19 => Confidence::Medium,
            _ => Confidence::High,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_sample_count_thresholds() {
        assert_eq!(Confidence::from_sample_count(0), Confidence::Low);
        assert_eq!(Confidence::from_sample_count(4), Confidence::Low);
        assert_eq!(Confidence::from_sample_count(5), Confidence::Medium);
        assert_eq!(Confidence::from_sample_count(19), Confidence::Medium);
        assert_eq!(Confidence::from_sample_count(20), Confidence::High);
        assert_eq!(Confidence::from_sample_count(1000), Confidence::High);
    }
}
