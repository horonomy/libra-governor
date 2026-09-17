//! [`Estimate`] — the probabilistic preflight cost/time estimate produced
//! by `libra-governor-estimator` from accumulated local
//! [`crate::ExecutionReceipt`] history.
//!
//! Lives in this crate (rather than `libra-governor-protocol`, which owns
//! `PreflightResult`) because [`crate::ExecutionPlan`] needs to carry the
//! estimate it was produced from through to receipt finalization, so a
//! Stop-hook receipt can compare actual-vs-estimate without a second
//! estimator call (see `docs/adr` on plan/estimate linkage and
//! HORO-1126). `libra-governor-protocol` re-exports this type into
//! `PreflightResult`.

use serde::{Deserialize, Serialize};

use crate::{confidence::Confidence, resource_amount::ResourceAmount};

/// The version/feature-set string every produced [`Estimate`] is tagged
/// with, so a stored estimate is always traceable to the exact estimator
/// logic that produced it (the campaign's estimator-versioning rule).
/// Bump this any time the quantile method, confidence thresholds, or
/// bucketing logic changes.
pub const ESTIMATOR_VERSION: &str = "v1-empirical-quantile";

/// A probabilistic preflight estimate: P50/P80/P90 for both wall-clock
/// duration and resource usage, plus the confidence/provenance metadata
/// needed to judge how much to trust it.
///
/// # Cold start
///
/// When zero local [`crate::ExecutionReceipt`] rows exist yet, this type
/// must not fabricate a plausible-looking number. [`Estimate::cold_start`]
/// produces a structurally distinct value: `cold_start: true`, every
/// numeric bound `None`, `confidence: Confidence::Low`, and a `reason`
/// explaining why. A real (if thin) computed estimate from 1-4 samples
/// also carries `confidence: Confidence::Low` but has `cold_start: false`
/// and real numeric bounds — the two are never confusable by checking
/// `confidence` alone, only by checking `cold_start`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Estimate {
    pub duration_p50_secs: Option<u64>,
    pub duration_p80_secs: Option<u64>,
    pub duration_p90_secs: Option<u64>,
    /// Resource quantiles, computed over whichever [`crate::ResourceKind`]
    /// is most common in the sample set (see estimator crate docs on why
    /// mixed-unit history cannot be quantiled together).
    pub resource_p50: Option<ResourceAmount>,
    pub resource_p80: Option<ResourceAmount>,
    pub resource_p90: Option<ResourceAmount>,
    pub confidence: Confidence,
    /// How many local [`crate::ExecutionReceipt`] rows this estimate was
    /// computed from. `0` if and only if `cold_start` is `true`.
    pub sample_count: usize,
    /// `true` only when zero local receipts existed at all — see type
    /// docs. Never inferred from `confidence` alone.
    pub cold_start: bool,
    /// Traceability tag — see [`ESTIMATOR_VERSION`].
    pub estimator_version: String,
    /// Human-readable explanation, always present on a cold-start
    /// estimate; `None` on a normally computed one.
    pub reason: Option<String>,
}

impl Estimate {
    /// The honest "no local history yet" result. Carries no fabricated
    /// numeric bounds.
    pub fn cold_start() -> Self {
        Self {
            duration_p50_secs: None,
            duration_p80_secs: None,
            duration_p90_secs: None,
            resource_p50: None,
            resource_p80: None,
            resource_p90: None,
            confidence: Confidence::Low,
            sample_count: 0,
            cold_start: true,
            estimator_version: ESTIMATOR_VERSION.to_string(),
            reason: Some(
                "insufficient local history: no ExecutionReceipt rows recorded yet".to_string(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cold_start_has_no_numeric_bounds_and_is_flagged() {
        let estimate = Estimate::cold_start();
        assert!(estimate.cold_start);
        assert_eq!(estimate.sample_count, 0);
        assert_eq!(estimate.confidence, Confidence::Low);
        assert!(estimate.duration_p50_secs.is_none());
        assert!(estimate.resource_p50.is_none());
        assert!(estimate.reason.is_some());
    }
}
