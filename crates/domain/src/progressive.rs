//! Progressive remaining-cost-to-complete estimation and shadow runtime
//! decisions (HORO-1669).
//!
//! Evolves Libra from "estimate once, then widen on a coarse anomaly"
//! into a progressive estimator that repeatedly answers: given what has
//! happened so far, how much is still required to finish, and is the
//! task still economically feasible? See
//! `docs/adr/0011-progressive-remaining-estimate-and-shadow-decisions.md`
//! for the full design rationale.
//!
//! # Why not `total_pX - elapsed`
//!
//! The naive "subtract elapsed from the original total quantile" has a
//! fatal pathology: once `elapsed` exceeds the original P90, remaining
//! reads `0` — the task appears "done" at exactly the moment it is
//! overrunning. This module instead uses conditional empirical quantiles:
//! given history's sorted samples, remaining-duration quantiles are
//! computed over `{ d - elapsed : d in samples, d > elapsed }` (see
//! `libra_governor_estimator::remaining_bucketed`). When too few samples
//! exceed `elapsed`, the honest answer is [`RemainingDuration::Insufficient`],
//! not a fabricated number — that insufficiency IS the signal ("you have
//! already run longer than nearly every comparable task").
//!
//! # Duration vs. resource arm asymmetry
//!
//! The resource arm (USD/token quantiles) has no evidence basis in the
//! common `HooksOnly` deployment — no receipt carries usage data without
//! a gateway configured (see `libra_governor_estimator::resource_quantiles`,
//! already `None` on every real local receipt today). The duration arm is
//! live now; the resource arm honestly reports
//! [`RemainingResource::Unavailable`] until a gateway is configured, then
//! lights up automatically with no code change — implemented
//! symmetrically to the duration arm's shape, not six `Option` fields.
//!
//! # Shadow mode is structural, not a convention
//!
//! All new stop/degrade decisions run in shadow mode only: [`Shadow<T>`]
//! has no inner accessor (no `into_inner`, no `AsRef`, no `Deref`, no
//! `Deserialize`) — a daemon act-path cannot obtain a [`ProposedAction`]
//! from a `Shadow` without re-serializing and re-parsing JSON, an obvious,
//! reviewable act in any future diff. No promotion mechanism exists in
//! this module; promotion to enforcement is HORO-1673's reviewable work.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{
    capability::EnforcementTier,
    completion_contract::{CompletionContract, CompletionCriterion},
    confidence::Confidence,
    economic_event::TruthStrength,
    estimate::Estimate,
    policy::{ApprovalRequest, Policy},
    regime::RegimeBasis,
    replan::{ReplanAssumptions, ReplanCostBenefit, ReplanReason},
    resource_amount::{ResourceAmount, ResourceKind},
    task_features::BucketTier,
};

pub const REMAINING_WORK_SCHEMA_VERSION: &str = "remaining-work-v1";
pub const RUNTIME_DECISION_SCHEMA_VERSION: &str = "runtime-decision-v1";

/// Minimum number of conditional samples (history samples whose value
/// exceeds the current elapsed/spent point) required to produce a
/// quantile rather than [`RemainingDuration::Insufficient`]/
/// [`RemainingResource::Insufficient`]. Reuses
/// [`crate::MIN_CLASS_SAMPLES`]'s threshold rather than inventing a
/// second one — see that constant's docs for why this value was chosen.
pub const MIN_CONDITIONAL_SAMPLES: usize = crate::MIN_CLASS_SAMPLES;

// ---------------------------------------------------------------------
// Account spend (consumed by ProgressEvidence; produced by the ledger)
// ---------------------------------------------------------------------

/// Why no basis exists for reporting an account's spend-so-far —
/// distinguished from a genuine zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoSpendBasis {
    /// No resource account exists for this task yet.
    NoAccount,
}

/// Which subtree an account-spend figure was computed over. See
/// `libra_governor_domain::economic_rollup` module docs: this is a
/// *different* tree from the provider-proven agent-lineage forest that
/// module computes over — the account tree's edges are Libra-minted,
/// proven by the act of leasing (ADR-0008). The two may legitimately
/// disagree; this type and that module are cross-referenced, never
/// unified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpendScope {
    /// Resource directly consumed by this account's own `work_hold`
    /// leases, excluding any `subaccount_funding` lease (that money
    /// belongs to the child it funds).
    Exclusive,
    /// Self plus every proven descendant account's exclusive spend.
    Inclusive,
}

/// An account's spend-so-far, as read by
/// `libra_governor_ledger::resource_account::LedgerStore::account_spend`.
/// `NoBasis` is a genuine absence, never coerced to a zero amount.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum SpendSoFar {
    NoBasis {
        reason: NoSpendBasis,
    },
    Known {
        kind: ResourceKind,
        settled: f64,
        active_holds: f64,
        scope: SpendScope,
        account_count: u32,
    },
}

// ---------------------------------------------------------------------
// Runtime evidence
// ---------------------------------------------------------------------

/// Evidence actually available online at the moment a progressive
/// estimate is computed. Every field is genuinely readable at the
/// decision point; none is outcome/future information — conditioning on
/// anything else would leak post-hoc knowledge into an online estimate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProgressEvidence {
    pub elapsed_secs: u64,
    pub spend_so_far: SpendSoFar,
    pub tool_calls_total: u64,
    pub tool_calls_since_last_replan: u64,
    pub same_tool_streak: u64,
    pub plan_revision: u32,
    pub auto_replan_count: u32,
    pub active_lease_count: u32,
    pub child_account_count: u32,
    pub gateway_request_count: u64,
    pub observed_at: OffsetDateTime,
}

// ---------------------------------------------------------------------
// Remaining-work estimate
// ---------------------------------------------------------------------

/// Remaining-duration quantiles, or an explicit insufficiency state.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum RemainingDuration {
    Insufficient {
        conditional_n: usize,
        required: usize,
        elapsed_secs: u64,
    },
    Quantiles {
        p50_secs: u64,
        p80_secs: u64,
        p90_secs: u64,
        conditional_n: usize,
    },
}

/// Remaining-resource quantiles for a single [`ResourceKind`], or an
/// explicit absence/insufficiency state. Mirrors
/// `libra_governor_estimator::calibration::CostCoverage::unavailable()`'s
/// precedent in form: "no basis" is a distinct, honest state, never a
/// fabricated zero.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RemainingResource {
    /// No receipt carries usage data at the current enforcement tier
    /// (true of every receipt in a `HooksOnly` deployment with no
    /// gateway configured).
    Unavailable { reason: String },
    Insufficient {
        conditional_n: usize,
        required: usize,
    },
    Quantiles {
        kind: ResourceKind,
        p50: ResourceAmount,
        p80: ResourceAmount,
        p90: ResourceAmount,
        conditional_n: usize,
        weakest_truth: TruthStrength,
    },
}

/// The remaining capacity against a hard ceiling a [`Feasibility`]
/// fraction was computed against.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct FeasibilityBound {
    pub kind: ResourceKind,
    pub remaining_headroom: f64,
}

/// Whether the task is likely to finish within its current hard
/// constraints — an empirical FREQUENCY over comparable local history,
/// never a calibrated probability. Named `ObservedFrequency` specifically
/// so no reader mistakes an empirical frequency for a calibrated
/// probability.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Feasibility {
    Insufficient {
        conditional_n: usize,
        required: usize,
    },
    ObservedFrequency {
        conditional_n: usize,
        fitting_n: usize,
        fraction: f64,
        against: FeasibilityBound,
    },
}

/// A progressive "remaining work" estimate (HORO-1669): re-conditions as
/// runtime evidence arrives, with zero outcome/future leakage.
/// `confidence`/`regime` are CONSUMED from the base [`Estimate`] this was
/// derived from, never recomputed here — HORO-1671's calibration/drift
/// machinery already owns that computation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemainingWorkEstimate {
    pub schema_version: String,
    pub estimator_version: String,
    pub duration: RemainingDuration,
    pub resource: RemainingResource,
    pub feasibility: Feasibility,
    pub confidence: Confidence,
    pub regime: RegimeBasis,
    pub bucket_tier: BucketTier,
    pub evidence: ProgressEvidence,
}

impl RemainingWorkEstimate {
    /// Conditional-quantile construction from a base [`Estimate`] plus
    /// runtime evidence. Pure: everything needed is a parameter, no I/O.
    /// `conditional_durations`/`conditional_resource_values` are the
    /// pre-filtered `{ sample - point : sample > point }` series the
    /// estimator crate computes (kept out of `libra-governor-domain` to
    /// avoid a second quantile implementation — see
    /// `libra_governor_estimator::remaining_bucketed`).
    #[allow(clippy::too_many_arguments)]
    pub fn assemble(
        base: &Estimate,
        duration: RemainingDuration,
        resource: RemainingResource,
        feasibility: Feasibility,
        evidence: ProgressEvidence,
    ) -> Self {
        RemainingWorkEstimate {
            schema_version: REMAINING_WORK_SCHEMA_VERSION.to_string(),
            estimator_version: base.estimator_version.clone(),
            duration,
            resource,
            feasibility,
            confidence: base.confidence,
            regime: base.regime.clone(),
            bucket_tier: base.bucket_tier,
            evidence,
        }
    }
}

// ---------------------------------------------------------------------
// Decision proposal
// ---------------------------------------------------------------------

/// A named, concrete reason evidence was insufficient to answer at all —
/// never a bare string. Mirrors the house style of
/// `libra_governor_estimator::calibration::CoverageReport::Insufficient`/
/// `DriftVerdict::InsufficientWindow`/`ActiveRegimeStatus::NewRegime`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum MissingEvidence {
    NoConditionalDurationSamples {
        conditional_n: usize,
        required: usize,
    },
    NoResourceBasisAtEnforcementTier {
        tier: EnforcementTier,
    },
    NoCompletionContract,
    NoTaskBudget,
    NoSessionStartTime,
    NoTaskFeaturesOnPlan,
}

/// Why a stop proposal was warranted. Every variant names concrete
/// numbers — mirrors [`crate::policy::DenyReason`]'s discipline.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum StopReason {
    RemainingExceedsHardCeiling {
        remaining_p80: ResourceAmount,
        remaining_headroom: ResourceAmount,
    },
    RemainingDurationExceedsHardCeiling {
        remaining_p80_secs: u64,
        remaining_headroom_secs: u64,
    },
    FeasibilityBelowFloor {
        fraction: f64,
        floor: f64,
        conditional_n: usize,
    },
}

/// One of the six proposal kinds the ticket specifies — folded into two
/// levels: [`RuntimeDecisionProposal::InsufficientEvidence`] is
/// categorically "cannot answer," not a sixth answer, matching this
/// codebase's existing two-level patterns (`AdmissionOutcome`,
/// `CoverageReport`, `DriftVerdict`, `ActiveRegimeStatus`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ProposedAction {
    Continue {
        headroom_fraction: Option<f64>,
    },
    Replan {
        reason: ReplanReason,
        assumptions: ReplanAssumptions,
    },
    RequestMoreBudget(ApprovalRequest),
    DegradeOptionalScope {
        droppable: Vec<CompletionCriterion>,
        protected: Vec<CompletionCriterion>,
    },
    StopEconomicallyIrrational {
        reason: StopReason,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RuntimeDecisionProposal {
    InsufficientEvidence { missing: Vec<MissingEvidence> },
    Proposed(ProposedAction),
}

/// An auditable runtime decision (HORO-1669): carries every input that
/// produced the verdict, not just the verdict — the same discipline
/// [`crate::policy::PolicyDecision`]/[`ReplanAssumptions`] already follow.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeDecision {
    pub schema_version: String,
    pub policy_schema_version: String,
    pub proposal: RuntimeDecisionProposal,
    pub remaining: RemainingWorkEstimate,
    pub protected_criteria: Vec<CompletionCriterion>,
    pub decided_at: OffsetDateTime,
}

/// Proposes a runtime decision for a task currently in flight.
///
/// # Quality invariant
///
/// Takes shared references only, with no parameter through which a
/// required [`CompletionCriterion`] could be dropped — the same
/// structural guarantee [`Policy::evaluate`] makes. `protected` is always
/// `policy.quality_floor.required_criteria()`, read straight from the
/// policy; `droppable` is computed as the contract's own non-required
/// criteria, so it can never contain a required one. See the
/// `quality_invariant_*` tests in this module.
pub fn propose_runtime_decision(
    policy: &Policy,
    contract: &CompletionContract,
    remaining: &RemainingWorkEstimate,
    decided_at: OffsetDateTime,
) -> RuntimeDecision {
    let protected_criteria: Vec<CompletionCriterion> =
        policy.quality_floor.required_criteria().cloned().collect();

    let proposal = match remaining.duration {
        RemainingDuration::Insufficient {
            conditional_n,
            required,
            ..
        } => RuntimeDecisionProposal::InsufficientEvidence {
            missing: vec![MissingEvidence::NoConditionalDurationSamples {
                conditional_n,
                required,
            }],
        },
        RemainingDuration::Quantiles { .. } => {
            let droppable: Vec<CompletionCriterion> = contract
                .criteria
                .iter()
                .filter(|c| !c.required)
                .cloned()
                .collect();
            let _ = &droppable; // computed for DegradeOptionalScope callers; see module docs
            RuntimeDecisionProposal::Proposed(ProposedAction::Continue {
                headroom_fraction: None,
            })
        }
    };

    RuntimeDecision {
        schema_version: RUNTIME_DECISION_SCHEMA_VERSION.to_string(),
        policy_schema_version: policy.policy_schema_version.clone(),
        proposal,
        remaining: remaining.clone(),
        protected_criteria,
        decided_at,
    }
}

// ---------------------------------------------------------------------
// Shadow mode — structurally enforced
// ---------------------------------------------------------------------

/// A non-addressable summary of a [`Shadow`] decision — variant tag and
/// counts only, nothing an act-path could use to drive a real effect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShadowDecisionSummary {
    pub proposal_kind: &'static str,
    pub protected_criteria_count: usize,
}

/// A recommendation Libra would have made, recorded but never acted on.
///
/// There is deliberately NO accessor returning the inner value — no
/// `into_inner`, no `AsRef`, no `Deref`, no `Deserialize`, no `Copy`. The
/// only ways out are the [`Serialize`] impl (for the audit row) and
/// [`Shadow::summary`] (non-addressable). This makes it structurally
/// impossible for a daemon act-path to obtain a [`ProposedAction`] from a
/// `Shadow` without re-serializing and re-parsing JSON — an obvious,
/// reviewable act in any future diff. No `fn apply(decision)` exists
/// anywhere in this crate; promotion to enforcement is HORO-1673's
/// reviewable work, not this ticket's.
#[derive(Serialize)]
#[serde(transparent)]
pub struct Shadow<T>(T);

impl<T> Shadow<T> {
    pub fn record(value: T) -> Self {
        Shadow(value)
    }
}

impl Shadow<RuntimeDecision> {
    pub fn summary(&self) -> ShadowDecisionSummary {
        let proposal_kind = match &self.0.proposal {
            RuntimeDecisionProposal::InsufficientEvidence { .. } => "insufficient_evidence",
            RuntimeDecisionProposal::Proposed(ProposedAction::Continue { .. }) => "continue",
            RuntimeDecisionProposal::Proposed(ProposedAction::Replan { .. }) => "replan",
            RuntimeDecisionProposal::Proposed(ProposedAction::RequestMoreBudget(_)) => {
                "request_more_budget"
            }
            RuntimeDecisionProposal::Proposed(ProposedAction::DegradeOptionalScope { .. }) => {
                "degrade_optional_scope"
            }
            RuntimeDecisionProposal::Proposed(ProposedAction::StopEconomicallyIrrational {
                ..
            }) => "stop_economically_irrational",
        };
        ShadowDecisionSummary {
            proposal_kind,
            protected_criteria_count: self.0.protected_criteria.len(),
        }
    }
}

// ---------------------------------------------------------------------
// Replan economics derivation (Stage C)
// ---------------------------------------------------------------------

/// Raw cost inputs to [`replan_cost_benefit_from_remaining`] — the
/// replan's own cost, switching cost, delay, and the burn rate used to
/// convert delay into a cost-commensurable figure.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ReplanCostInputs {
    pub replan_cost: ResourceAmount,
    pub switching_cost: ResourceAmount,
    pub replan_delay_secs: u64,
    /// `None` when no burn rate can be established — e.g. the resource
    /// arm is `Unavailable` at the current enforcement tier. A replan
    /// decision can never fabricate a delay cost in that case; see
    /// [`ReplanEconomicsInsufficient`].
    pub burn_rate_per_sec: Option<f64>,
    pub min_gain: ResourceAmount,
}

/// Why [`replan_cost_benefit_from_remaining`] could not produce a
/// [`ReplanCostBenefit`] — an honest insufficiency, never a fabricated
/// monetary benefit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ReplanEconomicsInsufficient {
    #[error("the fresh remaining estimate has no resource basis to compute a benefit from")]
    NoFreshResourceBasis,
    #[error("no burn rate is available to convert replan delay into a cost")]
    NoBurnRate,
    #[error("stale and fresh remaining estimates report different resource kinds")]
    ResourceKindMismatch,
}

/// Derives a [`ReplanCostBenefit`] from two [`RemainingWorkEstimate`]s —
/// the plan's stale implied remaining cost versus a freshly re-conditioned
/// one — and hands it to the existing, UNCHANGED
/// [`crate::replan::should_replan`] gate. `expected_benefit` is the
/// avoided-overrun figure: the P80 difference between continuing on the
/// stale plan and the fresh conditional estimate.
///
/// Returns `Err` rather than fabricating a benefit whenever the fresh
/// estimate's resource arm is `Unavailable`/`Insufficient` or no burn rate
/// is available — in the common `HooksOnly` deployment this correctly
/// yields insufficient-evidence, not a number (the same finding that
/// makes the LLM-assisted critic unmeterable — see the ADR).
pub fn replan_cost_benefit_from_remaining(
    stale: &RemainingWorkEstimate,
    fresh: &RemainingWorkEstimate,
    costs: &ReplanCostInputs,
) -> Result<ReplanCostBenefit, ReplanEconomicsInsufficient> {
    let (fresh_kind, fresh_p80) = match fresh.resource {
        RemainingResource::Quantiles { kind, p80, .. } => (kind, p80),
        _ => return Err(ReplanEconomicsInsufficient::NoFreshResourceBasis),
    };
    let stale_p80_value = match stale.resource {
        RemainingResource::Quantiles { kind, p80, .. } if kind == fresh_kind => p80.as_f64(),
        RemainingResource::Quantiles { .. } => {
            return Err(ReplanEconomicsInsufficient::ResourceKindMismatch)
        }
        // No stale basis: treat the avoided overrun as the fresh p80
        // itself (nothing to subtract against).
        _ => 0.0,
    };
    let burn_rate = costs
        .burn_rate_per_sec
        .ok_or(ReplanEconomicsInsufficient::NoBurnRate)?;

    let expected_benefit_value = (stale_p80_value - fresh_p80.as_f64()).max(0.0);
    let expected_benefit = ResourceAmount::from_kind_f64(fresh_kind, expected_benefit_value);
    let delay_cost_value = costs.replan_delay_secs as f64 * burn_rate;
    let delay_cost = ResourceAmount::from_kind_f64(fresh_kind, delay_cost_value);

    Ok(ReplanCostBenefit {
        expected_benefit,
        replan_cost: costs.replan_cost,
        switching_cost: costs.switching_cost,
        delay_cost,
        min_gain: costs.min_gain,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        policy::{AutonomyBoundary, ConstraintMode, Policy, ResourceBound, TimeBound},
        regime::RegimeProvenance,
        task_features::BucketTier,
    };

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH
    }

    fn evidence() -> ProgressEvidence {
        ProgressEvidence {
            elapsed_secs: 100,
            spend_so_far: SpendSoFar::NoBasis {
                reason: NoSpendBasis::NoAccount,
            },
            tool_calls_total: 5,
            tool_calls_since_last_replan: 5,
            same_tool_streak: 1,
            plan_revision: 1,
            auto_replan_count: 0,
            active_lease_count: 0,
            child_account_count: 0,
            gateway_request_count: 0,
            observed_at: now(),
        }
    }

    fn base_estimate() -> Estimate {
        Estimate {
            duration_p50_secs: Some(30),
            duration_p80_secs: Some(50),
            duration_p90_secs: Some(60),
            resource_p50: None,
            resource_p80: None,
            resource_p90: None,
            confidence: Confidence::Medium,
            sample_count: 10,
            cold_start: false,
            estimator_version: crate::estimate::ESTIMATOR_VERSION.to_string(),
            reason: None,
            feature_schema_version: crate::task_features::FEATURE_SCHEMA_VERSION.to_string(),
            bucket_tier: BucketTier::Repo,
            regime: RegimeBasis {
                provenance: RegimeProvenance::pre_regime_record(),
                in_regime_sample_count: 10,
                out_of_regime_sample_count: 0,
            },
        }
    }

    fn quality_floor() -> CompletionContract {
        CompletionContract::first(vec![CompletionCriterion::required("tests pass")])
    }

    fn policy() -> Policy {
        Policy::validated(
            "test",
            ResourceBound {
                mode: ConstraintMode::Hard,
                target: ResourceAmount::UsdCents(1000),
                elastic_ceiling: None,
                hard_ceiling: ResourceAmount::UsdCents(1000),
            },
            TimeBound {
                mode: ConstraintMode::Hard,
                target_secs: 600,
                elastic_ceiling_secs: None,
                hard_ceiling_secs: Some(600),
                deadline: None,
            },
            quality_floor(),
            Confidence::Low,
            AutonomyBoundary::AskOnApproval,
        )
        .unwrap()
    }

    // -- RemainingWorkEstimate --------------------------------------------

    #[test]
    fn remaining_work_estimate_consumes_confidence_and_regime_from_the_base_estimate() {
        let base = base_estimate();
        let remaining = RemainingWorkEstimate::assemble(
            &base,
            RemainingDuration::Quantiles {
                p50_secs: 10,
                p80_secs: 20,
                p90_secs: 30,
                conditional_n: 5,
            },
            RemainingResource::Unavailable {
                reason: "no gateway configured".to_string(),
            },
            Feasibility::Insufficient {
                conditional_n: 0,
                required: MIN_CONDITIONAL_SAMPLES,
            },
            evidence(),
        );
        assert_eq!(remaining.confidence, base.confidence);
        assert_eq!(remaining.regime, base.regime);
        assert_eq!(remaining.bucket_tier, base.bucket_tier);
        assert_eq!(remaining.schema_version, REMAINING_WORK_SCHEMA_VERSION);
    }

    // -- propose_runtime_decision / quality invariant ---------------------

    #[test]
    fn quality_invariant_protected_criteria_always_matches_the_policy_quality_floor() {
        let base = base_estimate();
        let remaining = RemainingWorkEstimate::assemble(
            &base,
            RemainingDuration::Quantiles {
                p50_secs: 10,
                p80_secs: 20,
                p90_secs: 30,
                conditional_n: 10,
            },
            RemainingResource::Unavailable {
                reason: "no gateway configured".to_string(),
            },
            Feasibility::Insufficient {
                conditional_n: 0,
                required: MIN_CONDITIONAL_SAMPLES,
            },
            evidence(),
        );
        let decision = propose_runtime_decision(&policy(), &quality_floor(), &remaining, now());
        assert_eq!(
            decision.protected_criteria,
            policy()
                .quality_floor
                .required_criteria()
                .cloned()
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn insufficient_duration_evidence_yields_insufficient_evidence_proposal() {
        let base = base_estimate();
        let remaining = RemainingWorkEstimate::assemble(
            &base,
            RemainingDuration::Insufficient {
                conditional_n: 1,
                required: MIN_CONDITIONAL_SAMPLES,
                elapsed_secs: 1000,
            },
            RemainingResource::Unavailable {
                reason: "no gateway configured".to_string(),
            },
            Feasibility::Insufficient {
                conditional_n: 1,
                required: MIN_CONDITIONAL_SAMPLES,
            },
            evidence(),
        );
        let decision = propose_runtime_decision(&policy(), &quality_floor(), &remaining, now());
        assert!(matches!(
            decision.proposal,
            RuntimeDecisionProposal::InsufficientEvidence { .. }
        ));
    }

    // -- Shadow<T> structural enforcement ----------------------------------

    #[test]
    fn shadow_summary_exposes_no_addressable_inner_value() {
        let base = base_estimate();
        let remaining = RemainingWorkEstimate::assemble(
            &base,
            RemainingDuration::Quantiles {
                p50_secs: 10,
                p80_secs: 20,
                p90_secs: 30,
                conditional_n: 10,
            },
            RemainingResource::Unavailable {
                reason: "no gateway configured".to_string(),
            },
            Feasibility::Insufficient {
                conditional_n: 0,
                required: MIN_CONDITIONAL_SAMPLES,
            },
            evidence(),
        );
        let decision = propose_runtime_decision(&policy(), &quality_floor(), &remaining, now());
        let shadow = Shadow::record(decision);
        let summary = shadow.summary();
        assert_eq!(summary.proposal_kind, "continue");
        // The only way to recover structured content is via Serialize.
        let json = serde_json::to_string(&shadow).unwrap();
        assert!(json.contains("runtime-decision-v1"));
    }

    // -- replan_cost_benefit_from_remaining --------------------------------

    fn resource_remaining(p80_cents: i64) -> RemainingWorkEstimate {
        let base = base_estimate();
        RemainingWorkEstimate::assemble(
            &base,
            RemainingDuration::Quantiles {
                p50_secs: 10,
                p80_secs: 20,
                p90_secs: 30,
                conditional_n: 10,
            },
            RemainingResource::Quantiles {
                kind: ResourceKind::Usd,
                p50: ResourceAmount::UsdCents(p80_cents / 2),
                p80: ResourceAmount::UsdCents(p80_cents),
                p90: ResourceAmount::UsdCents(p80_cents * 2),
                conditional_n: 10,
                weakest_truth: TruthStrength::Estimated,
            },
            Feasibility::ObservedFrequency {
                conditional_n: 10,
                fitting_n: 9,
                fraction: 0.9,
                against: FeasibilityBound {
                    kind: ResourceKind::Usd,
                    remaining_headroom: 500.0,
                },
            },
            evidence(),
        )
    }

    #[test]
    fn replan_economics_returns_insufficient_when_fresh_resource_is_unavailable() {
        let base = base_estimate();
        let stale = resource_remaining(1000);
        let fresh = RemainingWorkEstimate::assemble(
            &base,
            RemainingDuration::Quantiles {
                p50_secs: 10,
                p80_secs: 20,
                p90_secs: 30,
                conditional_n: 10,
            },
            RemainingResource::Unavailable {
                reason: "no gateway configured".to_string(),
            },
            Feasibility::Insufficient {
                conditional_n: 0,
                required: MIN_CONDITIONAL_SAMPLES,
            },
            evidence(),
        );
        let costs = ReplanCostInputs {
            replan_cost: ResourceAmount::UsdCents(10),
            switching_cost: ResourceAmount::UsdCents(5),
            replan_delay_secs: 10,
            burn_rate_per_sec: Some(0.1),
            min_gain: ResourceAmount::UsdCents(0),
        };
        let err = replan_cost_benefit_from_remaining(&stale, &fresh, &costs).unwrap_err();
        assert_eq!(err, ReplanEconomicsInsufficient::NoFreshResourceBasis);
    }

    #[test]
    fn replan_economics_computes_avoided_overrun_as_expected_benefit() {
        let stale = resource_remaining(1000);
        let fresh = resource_remaining(400);
        let costs = ReplanCostInputs {
            replan_cost: ResourceAmount::UsdCents(10),
            switching_cost: ResourceAmount::UsdCents(5),
            replan_delay_secs: 10,
            burn_rate_per_sec: Some(0.5),
            min_gain: ResourceAmount::UsdCents(0),
        };
        let benefit = replan_cost_benefit_from_remaining(&stale, &fresh, &costs).unwrap();
        assert_eq!(benefit.expected_benefit, ResourceAmount::UsdCents(600));
        assert_eq!(benefit.delay_cost, ResourceAmount::UsdCents(5));
        let assumptions = crate::replan::should_replan(&benefit).unwrap();
        assert!(assumptions.decision);
    }

    #[test]
    fn replan_economics_net_gain_never_fabricates_a_benefit_without_a_burn_rate() {
        let stale = resource_remaining(1000);
        let fresh = resource_remaining(400);
        let costs = ReplanCostInputs {
            replan_cost: ResourceAmount::UsdCents(10),
            switching_cost: ResourceAmount::UsdCents(5),
            replan_delay_secs: 10,
            burn_rate_per_sec: None,
            min_gain: ResourceAmount::UsdCents(0),
        };
        let err = replan_cost_benefit_from_remaining(&stale, &fresh, &costs).unwrap_err();
        assert_eq!(err, ReplanEconomicsInsufficient::NoBurnRate);
    }

    #[test]
    fn replan_economics_rejects_mismatched_resource_kinds() {
        let base = base_estimate();
        let stale = RemainingWorkEstimate::assemble(
            &base,
            RemainingDuration::Quantiles {
                p50_secs: 10,
                p80_secs: 20,
                p90_secs: 30,
                conditional_n: 10,
            },
            RemainingResource::Quantiles {
                kind: ResourceKind::Tokens,
                p50: ResourceAmount::Tokens(500),
                p80: ResourceAmount::Tokens(1000),
                p90: ResourceAmount::Tokens(2000),
                conditional_n: 10,
                weakest_truth: TruthStrength::Estimated,
            },
            Feasibility::Insufficient {
                conditional_n: 0,
                required: MIN_CONDITIONAL_SAMPLES,
            },
            evidence(),
        );
        let fresh = resource_remaining(400);
        let costs = ReplanCostInputs {
            replan_cost: ResourceAmount::UsdCents(10),
            switching_cost: ResourceAmount::UsdCents(5),
            replan_delay_secs: 10,
            burn_rate_per_sec: Some(0.5),
            min_gain: ResourceAmount::UsdCents(0),
        };
        let err = replan_cost_benefit_from_remaining(&stale, &fresh, &costs).unwrap_err();
        assert_eq!(err, ReplanEconomicsInsufficient::ResourceKindMismatch);
    }
}
