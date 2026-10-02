//! Counterfactual policy replay and decision-regret metrics (HORO-1670).
//!
//! Turns Libra's recorded execution history into an evaluation harness
//! for policy quality: given a real recorded trajectory of
//! [`crate::Shadow<crate::RuntimeDecision>`] rows, answer what decisions
//! another [`crate::Policy`] would have made at the same observable
//! decision points, and where the actual policy incurred avoidable
//! decision regret.
//!
//! # Why the counterfactual axis is the policy, never the estimator
//!
//! Replay re-evaluates [`crate::Policy::evaluate_resource`]/
//! [`crate::Policy::evaluate_time`] against the **recorded, frozen**
//! [`crate::RemainingWorkEstimate`] from each shadow decision row.
//! [`libra_governor_estimator::remaining_bucketed`] is never called
//! during replay. Two reasons:
//!
//! 1. Re-running the estimator means calling it with the *current* local
//!    receipt history, which on a real trajectory already includes the
//!    task's own final receipt — exactly the future-leakage the replay
//!    boundary forbids. Reading the recorded estimate verbatim closes
//!    this leak by construction: there is no history parameter anywhere
//!    in this module.
//! 2. [`crate::propose_runtime_decision`] (HORO-1669) is, as shipped,
//!    policy-insensitive — it branches only on duration sufficiency and
//!    never constructs `StopEconomicallyIrrational`/`DegradeOptionalScope`/
//!    `RequestMoreBudget`/`Replan`. Teaching it to do so is HORO-1673's
//!    reviewable work, not this ticket's — see the module's own doc
//!    comment on that gap. This module does not call
//!    `propose_runtime_decision` at all; it calls the lower-level,
//!    already-policy-sensitive `Policy::evaluate_resource`/`evaluate_time`
//!    directly.
//!
//! # The replay boundary is a signature, not a convention
//!
//! [`DecisionPoint`] has no field — and [`replay_point`] takes no
//! parameter — through which an [`crate::ExecutionReceipt`],
//! [`crate::ExecutionOutcome`], a later decision row, or future spend
//! could enter. [`replay_point`] takes a single `&DecisionPoint`, never a
//! slice: it structurally cannot see a later point. The only type in
//! this module permitted to reference [`crate::ExecutionOutcome`]/actual
//! duration/actual usage is [`PostHocRegret`], produced by a separate
//! function with no path back into decision-point replay.
//!
//! # How "what would the actual policy have decided" is obtained
//!
//! A shadow decision row's [`crate::RuntimeDecision`] does not itself
//! record an [`crate::Admission`] verdict — [`crate::propose_runtime_decision`]
//! never computes one (see above). This module instead replays the
//! **recorded policy** through the exact same [`replay_point`] path used
//! for every candidate: `replay_point(point, &point.recorded_policy)`.
//! Because [`replay_point`] is a pure function of `(point, policy)`, this
//! reproduces exactly what the recorded policy would decide at that point
//! — no separate "what really happened" bookkeeping is needed, and the
//! actual and every candidate are compared through identically-shaped
//! values.
//!
//! # Version pinning
//!
//! [`ReplayPins`] packages the 12 version/schema dimensions a
//! reproducible replay must agree on. 8 are already transitively carried
//! on a recorded [`crate::RuntimeDecision`]; [`PersistedPins`] is the
//! remaining 4 this module cannot derive and that migration `0014`
//! persists alongside the recorded [`crate::Policy`].
//! `RegimeKey::comparison`'s positive-evidence rule applies to
//! `pricing_version` (the one [`crate::DimensionValue`]-typed pin); every
//! other pin dimension compares by exact string/integer inequality.
//!
//! # Quality-floor violation is a type, not a flag
//!
//! [`PolicyComparison::QualityFloorViolated`] carries no regret, savings,
//! or headroom field at all — there is no way, in any renderer or
//! aggregate, to read a floor-violating candidate as "cheaper". See that
//! type's docs.
//!
//! # Unknown alternate-world outcomes are a type, not a comment
//!
//! [`AlternateOutcomeEffect`] has exactly one variant,
//! [`AlternateOutcomeEffect::Unknown`], and exactly one producer,
//! [`AlternateOutcomeEffect::unknown`]. Libra has no evidence about an
//! unobserved execution under a different policy; the type makes it
//! impossible to construct a value claiming otherwise.

use time::OffsetDateTime;

use serde::{Deserialize, Serialize};

use crate::{
    completion_contract::CompletionCriterion,
    execution_outcome::ExecutionOutcome,
    policy::{Admission, ConstraintOutcome, DenyReason, Policy},
    progressive::{
        NoSpendBasis, ProgressEvidence, RemainingDuration, RemainingResource,
        RemainingWorkEstimate, RuntimeDecision, SpendScope, SpendSoFar,
    },
    regime::DimensionValue,
    resource_account::AccountId,
    resource_amount::{ResourceAmount, ResourceKind},
    task_identity::TaskId,
};

/// The schema version every produced [`ReplayPins`]/[`PersistedPins`] is
/// tagged with.
pub const REPLAY_PINS_SCHEMA_VERSION: &str = "replay-pins-v1";

/// Minimum conditional samples a time/resource arm must carry to count as
/// a replay basis — reuses [`crate::MIN_CONDITIONAL_SAMPLES`] rather than
/// inventing a second threshold.
pub const MIN_REPLAY_SAMPLES: usize = crate::MIN_CONDITIONAL_SAMPLES;

// ---------------------------------------------------------------------
// Version pinning
// ---------------------------------------------------------------------

/// The 4 version constants a recorded shadow decision does not already
/// transitively carry. Persisted verbatim as `pins_json` (migration
/// `0014`) alongside the recorded [`Policy`] in `policy_json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedPins {
    pub schema_version: String,
    pub economic_attribution_contract_version: i64,
    pub execution_identity_envelope_version: i64,
    pub resource_account_schema_version: String,
    pub reservation_schema_version: String,
}

impl PersistedPins {
    /// The current build's pins — what the daemon writes on every new
    /// shadow decision.
    pub fn current() -> Self {
        PersistedPins {
            schema_version: REPLAY_PINS_SCHEMA_VERSION.to_string(),
            economic_attribution_contract_version: crate::ECONOMIC_ATTRIBUTION_CONTRACT_VERSION,
            execution_identity_envelope_version: crate::EXECUTION_IDENTITY_ENVELOPE_VERSION,
            resource_account_schema_version: crate::RESOURCE_ACCOUNT_SCHEMA_VERSION.to_string(),
            reservation_schema_version: crate::RESERVATION_SCHEMA_VERSION.to_string(),
        }
    }
}

/// The full 12-dimension replay-pin set: 8 derived from a recorded
/// [`RuntimeDecision`] (never a second stored copy — see module docs),
/// plus the 4 genuinely-persisted [`PersistedPins`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayPins {
    pub policy_schema_version: String,
    pub estimator_version: String,
    pub feature_schema: DimensionValue,
    pub remaining_work_schema: String,
    pub runtime_decision_schema: String,
    pub regime_schema: String,
    pub pricing_version: DimensionValue,
    pub resource_account_schema: String,
    pub reservation_schema: String,
    pub economic_attribution_contract_version: i64,
    pub execution_identity_envelope_version: i64,
    pub pins_schema: String,
}

impl ReplayPins {
    /// Derives the full pin set from one recorded decision plus its
    /// persisted pins. Never stores a second copy of the 8 derivable
    /// fields — this function is the only place they are read out.
    pub fn from_decision(decision: &RuntimeDecision, persisted: &PersistedPins) -> Self {
        let key = &decision.remaining.regime.provenance.key;
        ReplayPins {
            policy_schema_version: decision.policy_schema_version.clone(),
            estimator_version: decision.remaining.estimator_version.clone(),
            feature_schema: key.feature_schema.clone(),
            remaining_work_schema: decision.remaining.schema_version.clone(),
            runtime_decision_schema: decision.schema_version.clone(),
            regime_schema: decision.remaining.regime.provenance.schema_version.clone(),
            pricing_version: key.pricing_version.clone(),
            resource_account_schema: persisted.resource_account_schema_version.clone(),
            reservation_schema: persisted.reservation_schema_version.clone(),
            economic_attribution_contract_version: persisted.economic_attribution_contract_version,
            execution_identity_envelope_version: persisted.execution_identity_envelope_version,
            pins_schema: persisted.schema_version.clone(),
        }
    }

    /// Compares two pin sets dimension by dimension. Every dimension
    /// compares by exact (string/integer) inequality, **except**
    /// `pricing_version`, which uses `RegimeKey::comparison`'s own
    /// positive-evidence rule: a pricing-version mismatch is drift only
    /// when both sides are [`DimensionValue::Known`] and differ — an
    /// absent gateway on either side is never evidence of drift.
    pub fn comparison(&self, other: &ReplayPins) -> PinComparison {
        let mut differing = Vec::new();
        for dim in PinDimension::ALL {
            let drift = match dim {
                PinDimension::PolicySchema => string_drift(
                    dim,
                    &self.policy_schema_version,
                    &other.policy_schema_version,
                ),
                PinDimension::EstimatorVersion => {
                    string_drift(dim, &self.estimator_version, &other.estimator_version)
                }
                PinDimension::FeatureSchema => {
                    dimension_value_drift(dim, &self.feature_schema, &other.feature_schema)
                }
                PinDimension::RemainingWorkSchema => string_drift(
                    dim,
                    &self.remaining_work_schema,
                    &other.remaining_work_schema,
                ),
                PinDimension::RuntimeDecisionSchema => string_drift(
                    dim,
                    &self.runtime_decision_schema,
                    &other.runtime_decision_schema,
                ),
                PinDimension::RegimeSchema => {
                    string_drift(dim, &self.regime_schema, &other.regime_schema)
                }
                PinDimension::PricingVersion => {
                    dimension_value_drift(dim, &self.pricing_version, &other.pricing_version)
                }
                PinDimension::ResourceAccountSchema => string_drift(
                    dim,
                    &self.resource_account_schema,
                    &other.resource_account_schema,
                ),
                PinDimension::ReservationSchema => {
                    string_drift(dim, &self.reservation_schema, &other.reservation_schema)
                }
                PinDimension::EconomicAttributionContract => string_drift(
                    dim,
                    &self.economic_attribution_contract_version.to_string(),
                    &other.economic_attribution_contract_version.to_string(),
                ),
                PinDimension::ExecutionIdentityEnvelope => string_drift(
                    dim,
                    &self.execution_identity_envelope_version.to_string(),
                    &other.execution_identity_envelope_version.to_string(),
                ),
                PinDimension::PinsSchema => {
                    string_drift(dim, &self.pins_schema, &other.pins_schema)
                }
            };
            if let Some(d) = drift {
                differing.push(d);
            }
        }
        if differing.is_empty() {
            PinComparison::Identical
        } else {
            PinComparison::Drifted { differing }
        }
    }
}

fn string_drift(dim: PinDimension, a: &str, b: &str) -> Option<PinDrift> {
    (a != b).then(|| PinDrift {
        dimension: dim,
        recorded: a.to_string(),
        current: b.to_string(),
    })
}

/// The positive-evidence rule: a [`DimensionValue`] mismatch is drift
/// only when both sides are [`DimensionValue::Known`] and differ.
fn dimension_value_drift(
    dim: PinDimension,
    a: &DimensionValue,
    b: &DimensionValue,
) -> Option<PinDrift> {
    match (a, b) {
        (DimensionValue::Known(x), DimensionValue::Known(y)) if x != y => Some(PinDrift {
            dimension: dim,
            recorded: x.clone(),
            current: y.clone(),
        }),
        _ => None,
    }
}

/// Every [`ReplayPins`] dimension that participates in drift comparison.
/// The sole shared source of "all pin dimensions" for
/// [`ReplayPins::comparison`] and this module's exhaustiveness tripwire —
/// see [`Self::all_variants_is_exhaustive`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PinDimension {
    PolicySchema,
    EstimatorVersion,
    FeatureSchema,
    RemainingWorkSchema,
    RuntimeDecisionSchema,
    RegimeSchema,
    PricingVersion,
    ResourceAccountSchema,
    ReservationSchema,
    EconomicAttributionContract,
    ExecutionIdentityEnvelope,
    PinsSchema,
}

impl PinDimension {
    pub const ALL: [PinDimension; 12] = [
        PinDimension::PolicySchema,
        PinDimension::EstimatorVersion,
        PinDimension::FeatureSchema,
        PinDimension::RemainingWorkSchema,
        PinDimension::RuntimeDecisionSchema,
        PinDimension::RegimeSchema,
        PinDimension::PricingVersion,
        PinDimension::ResourceAccountSchema,
        PinDimension::ReservationSchema,
        PinDimension::EconomicAttributionContract,
        PinDimension::ExecutionIdentityEnvelope,
        PinDimension::PinsSchema,
    ];

    /// Compile-time-only exhaustiveness check — never called at runtime.
    /// Adding a variant without adding it to [`Self::ALL`] (and to
    /// [`ReplayPins::comparison`]) fails this match.
    #[allow(dead_code)]
    fn all_variants_is_exhaustive(dim: PinDimension) {
        match dim {
            PinDimension::PolicySchema
            | PinDimension::EstimatorVersion
            | PinDimension::FeatureSchema
            | PinDimension::RemainingWorkSchema
            | PinDimension::RuntimeDecisionSchema
            | PinDimension::RegimeSchema
            | PinDimension::PricingVersion
            | PinDimension::ResourceAccountSchema
            | PinDimension::ReservationSchema
            | PinDimension::EconomicAttributionContract
            | PinDimension::ExecutionIdentityEnvelope
            | PinDimension::PinsSchema => {}
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinDrift {
    pub dimension: PinDimension,
    pub recorded: String,
    pub current: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PinComparison {
    Identical,
    Drifted { differing: Vec<PinDrift> },
}

/// Why a shadow decision row cannot anchor a replay trajectory at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnpinnedReason {
    /// Row predates migration `0014` — no `pins_json` was ever recorded.
    NoPinsRecorded,
    /// Row predates migration `0014` — no `policy_json` was ever
    /// recorded, so no counterfactual baseline (`PolicyPresetInputs`)
    /// can be built.
    NoPolicyRecorded,
}

/// Whether a recorded row can participate in replay, and how it compares
/// to the current build. `Unpinned` refuses outright (no [`Policy`] means
/// no counterfactual baseline); `Drifted` still replays (the recorded
/// estimate is used verbatim, so the counterfactual decision remains
/// exactly reproducible) but is segregated from other cohorts for
/// cross-row aggregation — see [`UnpinnedWithinTrajectory`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplayEligibility {
    Unpinned { reason: UnpinnedReason },
    Drifted { differing: Vec<PinDrift> },
    Identical,
}

// ---------------------------------------------------------------------
// The replay boundary
// ---------------------------------------------------------------------

/// Everything observable at ONE historical decision point. Constructed
/// only from a `shadow_runtime_decisions` row. There is deliberately no
/// field — and [`replay_point`] takes no parameter — through which an
/// [`crate::ExecutionReceipt`], [`ExecutionOutcome`], a later decision
/// row, or any future spend could enter. That absence IS the replay
/// boundary, the same structural discipline [`Policy::evaluate`]/
/// [`crate::propose_runtime_decision`] already document.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionPoint {
    pub task_id: TaskId,
    pub plan_id: crate::execution_plan::PlanId,
    pub session_id: String,
    pub decided_at: OffsetDateTime,
    pub recorded_policy: Policy,
    pub pins: ReplayPins,
    pub recorded_decision: RuntimeDecision,
}

/// Remaining-duration projection derived from a recorded decision's
/// evidence — never a substituted zero.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TimeProjection {
    NoBasis {
        conditional_n: usize,
        required: usize,
    },
    Projected {
        elapsed_secs: u64,
        remaining_p80_secs: u64,
        projected_total_secs: u64,
    },
}

/// Why a resource projection has no basis — a named absence, mirroring
/// `CostCoverage::unavailable()`'s precedent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ResourceNoBasis {
    RemainingResourceUnavailable,
    RemainingResourceInsufficient {
        conditional_n: usize,
        required: usize,
    },
    SpendSoFarNoBasis {
        reason: NoSpendBasis,
    },
    /// The policy's resource kind does not match the remaining estimate's
    /// resource kind (or the recorded spend-so-far's kind) — a real, live
    /// case: policy config defaults to `Tokens`, while the remaining
    /// estimate's kind is whichever is most common in local receipt
    /// history. Never silently coerced or unwrapped.
    KindMismatch {
        policy_kind: ResourceKind,
        projected_kind: ResourceKind,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ResourceProjection {
    NoBasis(ResourceNoBasis),
    Projected {
        kind: ResourceKind,
        spent: ResourceAmount,
        remaining_p80: ResourceAmount,
        projected_total: ResourceAmount,
    },
}

fn time_projection(remaining: &RemainingWorkEstimate) -> TimeProjection {
    match remaining.duration {
        RemainingDuration::Insufficient {
            conditional_n,
            required,
            ..
        } => TimeProjection::NoBasis {
            conditional_n,
            required,
        },
        RemainingDuration::Quantiles { p80_secs, .. } => TimeProjection::Projected {
            elapsed_secs: remaining.evidence.elapsed_secs,
            remaining_p80_secs: p80_secs,
            projected_total_secs: remaining.evidence.elapsed_secs + p80_secs,
        },
    }
}

fn resource_projection(
    remaining: &RemainingWorkEstimate,
    candidate: &Policy,
) -> ResourceProjection {
    let (kind, p80) = match remaining.resource {
        RemainingResource::Unavailable { .. } => {
            return ResourceProjection::NoBasis(ResourceNoBasis::RemainingResourceUnavailable)
        }
        RemainingResource::Insufficient {
            conditional_n,
            required,
        } => {
            return ResourceProjection::NoBasis(ResourceNoBasis::RemainingResourceInsufficient {
                conditional_n,
                required,
            })
        }
        RemainingResource::Quantiles { kind, p80, .. } => (kind, p80),
    };

    let policy_kind = candidate.resource.target.kind();
    if policy_kind != kind {
        return ResourceProjection::NoBasis(ResourceNoBasis::KindMismatch {
            policy_kind,
            projected_kind: kind,
        });
    }

    let spent = match remaining.evidence.spend_so_far {
        SpendSoFar::NoBasis { reason } => {
            return ResourceProjection::NoBasis(ResourceNoBasis::SpendSoFarNoBasis { reason })
        }
        SpendSoFar::Known {
            kind: spent_kind,
            settled,
            ..
        } => {
            if spent_kind != kind {
                return ResourceProjection::NoBasis(ResourceNoBasis::KindMismatch {
                    policy_kind,
                    projected_kind: kind,
                });
            }
            ResourceAmount::from_kind_f64(kind, settled)
        }
    };

    let projected_total = ResourceAmount::from_kind_f64(kind, spent.as_f64() + p80.as_f64());
    ResourceProjection::Projected {
        kind,
        spent,
        remaining_p80: p80,
        projected_total,
    }
}

/// Which replay dimension a verdict/disagreement is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayDimension {
    Time,
    Resource,
    Confidence,
}

/// Why a dimension could not be replayed at all.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ReplayNoBasis {
    Time {
        conditional_n: usize,
        required: usize,
    },
    /// The recorded policy/candidate's time bound is internally
    /// inconsistent (did not go through [`Policy::validated`]) —
    /// `PolicyEvaluationError::InconsistentTimeBound`.
    TimeBoundInconsistent,
    Resource(ResourceNoBasis),
    /// Same as `TimeBoundInconsistent`, for the resource bound.
    ResourceBoundInconsistent,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DimensionVerdict {
    Replayed(ConstraintOutcome),
    NoBasis(ReplayNoBasis),
}

/// The admission [`replay_point`] computed for one candidate at one
/// decision point. A `Deny` in `admission` means "denied on the
/// dimensions that had evidence", never "denied overall" when a
/// dimension was blind — see `dimensions_without_basis`. A plain struct,
/// not an enum with a second "no basis at all" variant: the `Confidence`
/// dimension is always recorded on a real decision point (see
/// [`CounterfactualDecision::confidence_ok`]'s docs), so
/// `dimensions_replayed` is never empty and that second variant could
/// never be constructed.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayedAdmission {
    pub admission: Admission,
    pub dimensions_replayed: Vec<ReplayDimension>,
    pub dimensions_without_basis: Vec<ReplayDimension>,
}

/// The outcome of replaying one [`Policy`] against one [`DecisionPoint`].
#[derive(Debug, Clone, PartialEq)]
pub struct CounterfactualDecision {
    pub policy_name: String,
    pub time: DimensionVerdict,
    pub resource: DimensionVerdict,
    /// `true` iff the point's recorded [`crate::Confidence`] meets this
    /// policy's `min_confidence` floor. Always computable — the recorded
    /// confidence is always present — which is what keeps replay
    /// non-vacuous even when the resource dimension always lacks basis
    /// (the common `HooksOnly` deployment).
    pub confidence_ok: bool,
    pub admission: ReplayedAdmission,
    pub protected_criteria: Vec<CompletionCriterion>,
}

/// Replays `candidate` against one historical decision point. Pure: no
/// I/O, no clock, no parameter through which a later point or an outcome
/// could enter. See module docs for the full replay-boundary rationale.
pub fn replay_point(point: &DecisionPoint, candidate: &Policy) -> CounterfactualDecision {
    let remaining = &point.recorded_decision.remaining;

    let time = match time_projection(remaining) {
        TimeProjection::NoBasis {
            conditional_n,
            required,
        } => DimensionVerdict::NoBasis(ReplayNoBasis::Time {
            conditional_n,
            required,
        }),
        TimeProjection::Projected {
            projected_total_secs,
            ..
        } => match Policy::evaluate_time(&candidate.time, projected_total_secs) {
            Ok(outcome) => DimensionVerdict::Replayed(outcome),
            // `Policy::evaluate_time` only ever constructs
            // `InconsistentTimeBound` itself, but `candidate` is not
            // guaranteed to have gone through `Policy::validated` (its
            // fields are `pub`) — any other error variant is routed to
            // the same "no basis" verdict rather than assumed impossible.
            Err(_) => DimensionVerdict::NoBasis(ReplayNoBasis::TimeBoundInconsistent),
        },
    };

    let resource = match resource_projection(remaining, candidate) {
        ResourceProjection::NoBasis(reason) => {
            DimensionVerdict::NoBasis(ReplayNoBasis::Resource(reason))
        }
        ResourceProjection::Projected {
            projected_total, ..
        } => {
            match Policy::evaluate_resource(&candidate.resource, projected_total) {
                Ok(outcome) => DimensionVerdict::Replayed(outcome),
                // Same reasoning as `evaluate_time` above: `candidate`
                // may not have gone through `Policy::validated`, so any
                // error — not just `InconsistentResourceBound` — is
                // routed to "no basis" rather than assumed unreachable.
                Err(_) => DimensionVerdict::NoBasis(ReplayNoBasis::ResourceBoundInconsistent),
            }
        }
    };

    let confidence_ok = remaining.confidence >= candidate.min_confidence;

    let mut dimensions_replayed = Vec::new();
    let mut dimensions_without_basis = Vec::new();
    let mut deny_reasons = Vec::new();
    let mut approval_requests = Vec::new();

    for (dim, verdict) in [
        (ReplayDimension::Time, &time),
        (ReplayDimension::Resource, &resource),
    ] {
        match verdict {
            DimensionVerdict::Replayed(outcome) => {
                dimensions_replayed.push(dim);
                match outcome {
                    ConstraintOutcome::Deny(reason) => deny_reasons.push(reason.clone()),
                    ConstraintOutcome::ApprovalRequired(request) => {
                        approval_requests.push(request.clone())
                    }
                    ConstraintOutcome::Admit => {}
                }
            }
            DimensionVerdict::NoBasis(_) => {
                dimensions_without_basis.push(dim);
            }
        }
    }

    // Confidence is always recorded on a real decision point, so it
    // always has a basis — this is what keeps `dimensions_replayed`
    // always non-empty (see `ReplayedAdmission`'s docs).
    dimensions_replayed.push(ReplayDimension::Confidence);
    if !confidence_ok {
        deny_reasons.push(DenyReason::ConfidenceBelowThreshold {
            actual: remaining.confidence,
            required: candidate.min_confidence,
        });
    }

    let resolved_admission = if !deny_reasons.is_empty() {
        Admission::Deny(deny_reasons)
    } else if !approval_requests.is_empty() {
        Admission::ApprovalRequired(approval_requests)
    } else {
        Admission::Admit
    };
    let admission = ReplayedAdmission {
        admission: resolved_admission,
        dimensions_replayed,
        dimensions_without_basis,
    };

    CounterfactualDecision {
        policy_name: candidate.name.clone(),
        time,
        resource,
        confidence_ok,
        admission,
        protected_criteria: candidate
            .quality_floor
            .required_criteria()
            .cloned()
            .collect(),
    }
}

// ---------------------------------------------------------------------
// Decision-regret metrics
// ---------------------------------------------------------------------

fn restrictiveness(outcome: &ConstraintOutcome) -> u8 {
    match outcome {
        ConstraintOutcome::Admit => 0,
        ConstraintOutcome::ApprovalRequired(_) => 1,
        ConstraintOutcome::Deny(_) => 2,
    }
}

/// How a candidate's per-point decision compares to the actual recorded
/// policy's own replayed decision at the same point.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Disagreement {
    Agree,
    CandidateMoreRestrictive {
        dimensions: Vec<ReplayDimension>,
    },
    CandidateMorePermissive {
        dimensions: Vec<ReplayDimension>,
    },
    /// Restrictive on one dimension, permissive on another — never
    /// collapsed into a single more/less verdict.
    Mixed {
        more_restrictive: Vec<ReplayDimension>,
        more_permissive: Vec<ReplayDimension>,
    },
    /// Neither policy had a replayable basis on any dimension at this
    /// point.
    NoComparableDimension,
}

fn classify_disagreement(
    actual: &CounterfactualDecision,
    candidate: &CounterfactualDecision,
) -> Disagreement {
    let mut more_restrictive = Vec::new();
    let mut more_permissive = Vec::new();

    for (dim, a, c) in [
        (ReplayDimension::Time, &actual.time, &candidate.time),
        (
            ReplayDimension::Resource,
            &actual.resource,
            &candidate.resource,
        ),
    ] {
        if let (DimensionVerdict::Replayed(ao), DimensionVerdict::Replayed(co)) = (a, c) {
            let (ar, cr) = (restrictiveness(ao), restrictiveness(co));
            if cr > ar {
                more_restrictive.push(dim);
            } else if cr < ar {
                more_permissive.push(dim);
            }
        }
    }

    // Confidence always has a basis on both sides (see replay_point).
    if actual.confidence_ok && !candidate.confidence_ok {
        more_restrictive.push(ReplayDimension::Confidence);
    } else if !actual.confidence_ok && candidate.confidence_ok {
        more_permissive.push(ReplayDimension::Confidence);
    }

    match (more_restrictive.is_empty(), more_permissive.is_empty()) {
        (true, true) => Disagreement::Agree,
        (false, true) => Disagreement::CandidateMoreRestrictive {
            dimensions: more_restrictive,
        },
        (true, false) => Disagreement::CandidateMorePermissive {
            dimensions: more_permissive,
        },
        (false, false) => Disagreement::Mixed {
            more_restrictive,
            more_permissive,
        },
    }
}

/// Decision-regret measured at ONE decision point, from decision-point-
/// observable facts only — every field is a measured difference between
/// what the actual (recorded) policy authorized and what the candidate
/// would have, never an inference about what the software would have
/// done.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PointRegret {
    pub decided_at: OffsetDateTime,
    pub elapsed_secs: u64,
    pub tool_calls_total: u64,
    pub disagreement: Disagreement,
    pub admitted_spend_so_far: SpendSoFar,
}

fn point_regret(
    point: &DecisionPoint,
    candidate: &Policy,
) -> (PointRegret, CounterfactualDecision) {
    let actual = replay_point(point, &point.recorded_policy);
    let candidate_decision = replay_point(point, candidate);
    let disagreement = classify_disagreement(&actual, &candidate_decision);
    let evidence: &ProgressEvidence = &point.recorded_decision.remaining.evidence;
    (
        PointRegret {
            decided_at: point.decided_at,
            elapsed_secs: evidence.elapsed_secs,
            tool_calls_total: evidence.tool_calls_total,
            disagreement,
            admitted_spend_so_far: evidence.spend_so_far,
        },
        candidate_decision,
    )
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisagreementCounts {
    pub agree: u32,
    pub candidate_more_restrictive: u32,
    pub candidate_more_permissive: u32,
    pub mixed: u32,
    pub no_comparable_dimension: u32,
}

impl DisagreementCounts {
    fn record(&mut self, d: &Disagreement) {
        match d {
            Disagreement::Agree => self.agree += 1,
            Disagreement::CandidateMoreRestrictive { .. } => self.candidate_more_restrictive += 1,
            Disagreement::CandidateMorePermissive { .. } => self.candidate_more_permissive += 1,
            Disagreement::Mixed { .. } => self.mixed += 1,
            Disagreement::NoComparableDimension => self.no_comparable_dimension += 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FirstRefusal {
    pub decided_at: OffsetDateTime,
    pub elapsed_secs: u64,
    pub tool_calls_total: u64,
    pub dimensions: Vec<ReplayDimension>,
}

/// Per-trajectory decision-regret aggregate over every eligible,
/// cohort-matching [`DecisionPoint`] — see [`PolicyComparison::evaluate`]
/// for how the cohort (`(pins, recorded_policy)`) is pinned and how
/// out-of-cohort points are skipped rather than silently pooled.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrajectoryRegret {
    pub task_id: TaskId,
    pub pins: ReplayPins,
    pub actual_policy_name: String,
    pub candidate_policy_name: String,
    pub spend_scope: Option<SpendScope>,
    pub points_replayed: usize,
    pub points_skipped: usize,
    pub disagreement_counts: DisagreementCounts,
    pub first_candidate_refusal: Option<FirstRefusal>,
    pub elapsed_after_first_candidate_refusal_secs: Option<u64>,
    pub admitted_spend_at_first_refusal: Option<SpendSoFar>,
    pub admitted_spend_at_last_point: Option<SpendSoFar>,
}

// ---------------------------------------------------------------------
// Quality-floor type-level guard
// ---------------------------------------------------------------------

/// Whether a candidate [`Policy`] is even comparable to the recorded one
/// — a type-level partition, not a flag. The floor-violating variant has
/// NO field carrying a savings/regret/headroom figure, so there is no way
/// — in any renderer, aggregate, or test — to read "cheaper" for it.
/// Cheapness is never a defense against a quality-floor violation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
// The size asymmetry between variants is the point, not an oversight:
// `QualityFloorViolated` deliberately carries no regret payload at all,
// while `QualityFloorPreserved` carries the full `TrajectoryRegret`.
// Boxing the regret to satisfy the lint would add an indirection purely
// to flatten a size difference this type exists to encode.
pub enum PolicyComparison {
    QualityFloorPreserved {
        regret: TrajectoryRegret,
    },
    QualityFloorViolated {
        dropped_required: Vec<CompletionCriterion>,
        recorded_required: Vec<CompletionCriterion>,
        candidate_required: Vec<CompletionCriterion>,
    },
}

impl PolicyComparison {
    /// The only producer of [`PolicyComparison`]. Checks the quality
    /// floor FIRST, before computing any regret — a floor-violating
    /// candidate's regret is never computed at all, not computed-then-
    /// hidden. `first`/`rest` must already share one cohort key (the
    /// caller — [`replay_trajectory`] — guarantees this); this function
    /// does not re-check pin/policy identity across points. Taking
    /// `first` separately from `rest` makes an empty trajectory
    /// unrepresentable at the type level — there is no slice-length
    /// check to get wrong, and no placeholder value is ever needed for
    /// "no points".
    pub fn evaluate(first: &DecisionPoint, rest: &[DecisionPoint], candidate: &Policy) -> Self {
        let candidate_required: Vec<CompletionCriterion> = candidate
            .quality_floor
            .required_criteria()
            .cloned()
            .collect();
        let recorded_required: Vec<CompletionCriterion> = first
            .recorded_policy
            .quality_floor
            .required_criteria()
            .cloned()
            .collect();
        let candidate_descriptions: std::collections::HashSet<&str> = candidate_required
            .iter()
            .map(|c| c.description.as_str())
            .collect();
        let dropped: Vec<CompletionCriterion> = recorded_required
            .iter()
            .filter(|c| !candidate_descriptions.contains(c.description.as_str()))
            .cloned()
            .collect();
        if !dropped.is_empty() {
            return PolicyComparison::QualityFloorViolated {
                dropped_required: dropped,
                recorded_required,
                candidate_required,
            };
        }

        let mut counts = DisagreementCounts::default();
        let mut first_candidate_refusal: Option<FirstRefusal> = None;
        let mut admitted_spend_at_first_refusal = None;
        let mut admitted_spend_at_last_point = None;
        let mut spend_scope = None;
        let mut points_replayed = 0usize;
        let mut last_elapsed_secs = first.recorded_decision.remaining.evidence.elapsed_secs;

        for point in std::iter::once(first).chain(rest.iter()) {
            let (regret, candidate_decision) = point_regret(point, candidate);
            counts.record(&regret.disagreement);
            points_replayed += 1;
            last_elapsed_secs = point.recorded_decision.remaining.evidence.elapsed_secs;

            if let SpendSoFar::Known { scope, .. } = regret.admitted_spend_so_far {
                spend_scope = Some(scope);
            }
            admitted_spend_at_last_point = Some(regret.admitted_spend_so_far);

            let candidate_refused =
                matches!(candidate_decision.admission.admission, Admission::Deny(_));
            if candidate_refused && first_candidate_refusal.is_none() {
                let dims = candidate_decision.admission.dimensions_replayed.clone();
                first_candidate_refusal = Some(FirstRefusal {
                    decided_at: regret.decided_at,
                    elapsed_secs: regret.elapsed_secs,
                    tool_calls_total: regret.tool_calls_total,
                    dimensions: dims,
                });
                admitted_spend_at_first_refusal = Some(regret.admitted_spend_so_far);
            }
        }

        let elapsed_after_first_candidate_refusal_secs = first_candidate_refusal
            .as_ref()
            .filter(|refusal| last_elapsed_secs >= refusal.elapsed_secs)
            .map(|refusal| last_elapsed_secs - refusal.elapsed_secs);

        PolicyComparison::QualityFloorPreserved {
            regret: TrajectoryRegret {
                task_id: first.task_id,
                pins: first.pins.clone(),
                actual_policy_name: first.recorded_policy.name.clone(),
                candidate_policy_name: candidate.name.clone(),
                spend_scope,
                points_replayed,
                points_skipped: 0,
                disagreement_counts: counts,
                first_candidate_refusal,
                elapsed_after_first_candidate_refusal_secs,
                admitted_spend_at_first_refusal,
                admitted_spend_at_last_point,
            },
        }
    }

    /// `Some` only for [`Self::QualityFloorPreserved`]. There is no
    /// accessor returning a regret/savings figure for a violated floor —
    /// a ranking API over multiple [`PolicyComparison`]s can only read
    /// this method, so a floor-violator can never enter a ranking.
    pub fn comparable_regret(&self) -> Option<&TrajectoryRegret> {
        match self {
            PolicyComparison::QualityFloorPreserved { regret } => Some(regret),
            PolicyComparison::QualityFloorViolated { .. } => None,
        }
    }
}

/// Groups a task's recorded shadow decisions (already ordered
/// `decided_at ASC` by the caller) into one replayable trajectory: the
/// first eligible row's `(pins, recorded_policy)` becomes the cohort key,
/// and any later row differing on either is skipped rather than silently
/// pooled (mirrors `DimensionUnavailable::MixedWithinTask`'s shape).
/// Rows whose own pins are [`ReplayEligibility::Unpinned`] are always
/// skipped.
pub fn replay_trajectory(
    points: Vec<(DecisionPoint, ReplayEligibility)>,
    candidate: &Policy,
) -> Option<PolicyComparison> {
    let mut cohort: Vec<DecisionPoint> = Vec::new();
    let mut skipped = 0usize;
    let mut cohort_key: Option<(ReplayPins, Policy)> = None;

    for (point, eligibility) in points {
        match eligibility {
            ReplayEligibility::Unpinned { .. } => {
                skipped += 1;
                continue;
            }
            ReplayEligibility::Identical | ReplayEligibility::Drifted { .. } => {}
        }
        match &cohort_key {
            None => {
                cohort_key = Some((point.pins.clone(), point.recorded_policy.clone()));
                cohort.push(point);
            }
            Some((key_pins, key_policy)) => {
                // A later row diverging on pins ("pins drifted within
                // trajectory") or on policy ("policy changed within
                // trajectory") is excluded rather than silently pooled —
                // see the disclosed scope note in ADR-0012: both
                // conditions are folded into the one `skipped` count
                // here rather than kept as named per-reason tallies.
                let cohort_matches = key_pins.comparison(&point.pins) == PinComparison::Identical
                    && *key_policy == point.recorded_policy;
                if cohort_matches {
                    cohort.push(point);
                } else {
                    skipped += 1;
                }
            }
        }
    }

    let (first, rest) = cohort.split_first()?;
    let mut comparison = PolicyComparison::evaluate(first, rest, candidate);
    if let PolicyComparison::QualityFloorPreserved { regret } = &mut comparison {
        regret.points_skipped = skipped;
    }
    Some(comparison)
}

// ---------------------------------------------------------------------
// Post-hoc layer — separately typed, structurally unable to contaminate
// decision-point replay
// ---------------------------------------------------------------------

/// What would have happened to the software under a candidate policy.
/// Exactly one variant exists and no code path produces another: Libra
/// has no evidence about an unobserved execution. Kept as a real field
/// with a single variant rather than omitted, so no reader can mistake a
/// missing field for "no effect".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AlternateOutcomeEffect {
    Unknown { reason: String },
}

impl AlternateOutcomeEffect {
    /// The only producer of [`Self::Unknown`].
    pub fn unknown() -> Self {
        AlternateOutcomeEffect::Unknown {
            reason: "Libra has no evidence about an execution that did not happen; a different \
                     policy's software-level effect is never inferred from recorded data"
                .to_string(),
        }
    }
}

/// Post-hoc analysis: the ONLY type in this module permitted to read the
/// finalized [`ExecutionOutcome`]/actual duration/actual usage. A
/// separate type produced by a separate function, so no decision-point
/// replay can be contaminated — [`replay_trajectory`]/[`replay_point`]
/// have no parameter through which a receipt can enter, and this type
/// cannot be constructed without one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PostHocRegret {
    pub trajectory: TrajectoryRegret,
    pub actual_outcome: ExecutionOutcome,
    pub actual_duration_secs: u64,
    pub actual_usage: Vec<ResourceAmount>,
    pub alternate_outcome_effect: AlternateOutcomeEffect,
}

impl PostHocRegret {
    pub fn new(
        trajectory: TrajectoryRegret,
        actual_outcome: ExecutionOutcome,
        actual_duration_secs: u64,
        actual_usage: Vec<ResourceAmount>,
    ) -> Self {
        PostHocRegret {
            trajectory,
            actual_outcome,
            actual_duration_secs,
            actual_usage,
            alternate_outcome_effect: AlternateOutcomeEffect::unknown(),
        }
    }
}

// ---------------------------------------------------------------------
// Aggregation — never double-count across the account custody tree
// ---------------------------------------------------------------------

/// Why cross-task regret aggregation was refused rather than silently
/// producing a possibly-wrong sum.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AggregationError {
    #[error("account {descendant} is a descendant of {ancestor}; spend cannot be summed across an account custody subtree without double counting")]
    OverlappingAccountSubtrees {
        ancestor: AccountId,
        descendant: AccountId,
    },
    #[error(
        "entries report different SpendSoFar scopes; aggregation requires one consistent scope"
    )]
    MixedSpendScope,
    #[error("entries report different replay pins; aggregation requires one consistent pin set")]
    MixedPins { differing: Vec<PinDrift> },
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AggregateRegret {
    pub trajectories: usize,
    pub disagreement_counts: DisagreementCounts,
    pub points_replayed: usize,
    pub points_skipped: usize,
}

/// Sums ONLY provably non-overlapping quantities (disagreement counts,
/// points replayed/skipped) across a flat set of entries, refusing when
/// any entry's own `parent` field names another entry present in the
/// same call's `entries` slice (a direct-parent check against the
/// immediate list, not a walk of the full account tree — callers passing
/// a non-sibling set, e.g. a grandparent and grandchild with the
/// intermediate account omitted, are outside this check's reach; keep
/// `entries` to one flat sibling generation). Spend is carried per-task
/// in [`TrajectoryRegret`] and NEVER summed here — this is NOT
/// `economic_rollup::inclusive_spend` and does not re-derive the
/// provider-proven agent-lineage forest (ADR-0008: two different trees,
/// cross-referenced, never unified). Declining to produce an incorrect
/// cross-tree sum is the correct answer, not a limitation. No production
/// call site exists yet (HORO-1673 is the expected first one) — the
/// HORO-1670 replay runner sums per-candidate counts by hand instead of
/// calling this.
pub fn aggregate_regret(
    entries: &[(AccountId, Option<AccountId>, TrajectoryRegret)],
) -> Result<AggregateRegret, AggregationError> {
    // Overlap check: an O(n^2) ancestor scan is fine at this scale (one
    // call site compares a handful of tasks at a time, never the whole
    // ledger).
    for (account, parent, _) in entries {
        if let Some(parent) = parent {
            if entries.iter().any(|(other, _, _)| other == parent) {
                return Err(AggregationError::OverlappingAccountSubtrees {
                    ancestor: *parent,
                    descendant: *account,
                });
            }
        }
    }

    let mut scope: Option<SpendScope> = None;
    let mut pins: Option<&ReplayPins> = None;
    let mut out = AggregateRegret::default();
    for (_, _, regret) in entries {
        if let Some(s) = regret.spend_scope {
            match scope {
                None => scope = Some(s),
                Some(existing) if existing != s => return Err(AggregationError::MixedSpendScope),
                Some(_) => {}
            }
        }
        match pins {
            None => pins = Some(&regret.pins),
            Some(existing) => {
                if let PinComparison::Drifted { differing } = existing.comparison(&regret.pins) {
                    return Err(AggregationError::MixedPins { differing });
                }
            }
        }
        out.trajectories += 1;
        out.points_replayed += regret.points_replayed;
        out.points_skipped += regret.points_skipped;
        out.disagreement_counts.agree += regret.disagreement_counts.agree;
        out.disagreement_counts.candidate_more_restrictive +=
            regret.disagreement_counts.candidate_more_restrictive;
        out.disagreement_counts.candidate_more_permissive +=
            regret.disagreement_counts.candidate_more_permissive;
        out.disagreement_counts.mixed += regret.disagreement_counts.mixed;
        out.disagreement_counts.no_comparable_dimension +=
            regret.disagreement_counts.no_comparable_dimension;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        completion_contract::CompletionContract,
        policy::{
            AutonomyBoundary, ConstraintMode, Policy, PolicyPresetInputs, ResourceBound, TimeBound,
        },
        progressive::{Feasibility, RemainingWorkEstimate},
        regime::{CacheClass, DimensionUnavailable, RegimeKey, RegimeProvenance},
        resource_amount::ResourceKind,
        task_features::BucketTier,
        Confidence,
    };

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    fn quality_floor() -> CompletionContract {
        CompletionContract::first(vec![CompletionCriterion::required("tests pass")])
    }

    fn test_policy(name: &str, resource_kind: ResourceKind, min_confidence: Confidence) -> Policy {
        let target = match resource_kind {
            ResourceKind::Usd => ResourceAmount::UsdCents(1000),
            ResourceKind::Tokens => ResourceAmount::Tokens(1000),
            ResourceKind::QuotaPercent => ResourceAmount::QuotaPercent(50.0),
        };
        Policy::validated(
            name,
            ResourceBound {
                mode: ConstraintMode::Hard,
                target,
                elastic_ceiling: None,
                hard_ceiling: target,
            },
            TimeBound {
                mode: ConstraintMode::Hard,
                target_secs: 600,
                elastic_ceiling_secs: None,
                hard_ceiling_secs: Some(600),
                deadline: None,
            },
            quality_floor(),
            min_confidence,
            AutonomyBoundary::AskOnApproval,
        )
        .unwrap()
    }

    fn regime_key() -> RegimeKey {
        RegimeKey::builder()
            .model(None)
            .harness(None)
            .harness_version(None)
            .effort(None)
            .gateway_pricing(None)
            .enforcement_tier(None)
            .feature_schema(crate::task_features::FEATURE_SCHEMA_VERSION)
            .build()
    }

    fn remaining_with(
        duration: RemainingDuration,
        resource: RemainingResource,
        elapsed_secs: u64,
        spend_so_far: SpendSoFar,
        confidence: Confidence,
    ) -> RemainingWorkEstimate {
        let evidence = ProgressEvidence {
            elapsed_secs,
            spend_so_far,
            tool_calls_total: 5,
            tool_calls_since_last_replan: 5,
            same_tool_streak: 1,
            plan_revision: 1,
            auto_replan_count: 0,
            active_lease_count: 0,
            child_account_count: 0,
            gateway_request_count: 0,
            observed_at: now(),
        };
        RemainingWorkEstimate {
            schema_version: crate::progressive::REMAINING_WORK_SCHEMA_VERSION.to_string(),
            estimator_version: crate::ESTIMATOR_VERSION.to_string(),
            duration,
            resource,
            feasibility: Feasibility::Insufficient {
                conditional_n: 0,
                required: MIN_REPLAY_SAMPLES,
            },
            confidence,
            regime: crate::regime::RegimeBasis {
                provenance: RegimeProvenance::new(regime_key(), None, CacheClass::NoCacheObserved),
                in_regime_sample_count: 10,
                out_of_regime_sample_count: 0,
            },
            bucket_tier: BucketTier::Repo,
            evidence,
        }
    }

    fn decision_point(
        task_id: TaskId,
        recorded_policy: Policy,
        remaining: RemainingWorkEstimate,
        decided_at: OffsetDateTime,
    ) -> DecisionPoint {
        let decision = RuntimeDecision {
            schema_version: crate::progressive::RUNTIME_DECISION_SCHEMA_VERSION.to_string(),
            policy_schema_version: recorded_policy.policy_schema_version.clone(),
            proposal: crate::progressive::RuntimeDecisionProposal::Proposed(
                crate::progressive::ProposedAction::Continue {
                    headroom_fraction: None,
                },
            ),
            remaining,
            protected_criteria: recorded_policy
                .quality_floor
                .required_criteria()
                .cloned()
                .collect(),
            decided_at,
        };
        let pins = ReplayPins::from_decision(&decision, &PersistedPins::current());
        DecisionPoint {
            task_id,
            plan_id: crate::execution_plan::PlanId(uuid::Uuid::new_v4()),
            session_id: "sess-1".to_string(),
            decided_at,
            recorded_policy,
            pins,
            recorded_decision: decision,
        }
    }

    // -- ReplayPins / pin-drift comparison --------------------------------

    #[test]
    fn identical_pins_compare_identical() {
        let decision_a = {
            let p = test_policy("balanced", ResourceKind::Tokens, Confidence::Low);
            decision_point(
                TaskId(uuid::Uuid::new_v4()),
                p,
                remaining_with(
                    RemainingDuration::Quantiles {
                        p50_secs: 10,
                        p80_secs: 20,
                        p90_secs: 30,
                        conditional_n: 10,
                    },
                    RemainingResource::Unavailable {
                        reason: "no gateway".to_string(),
                    },
                    100,
                    SpendSoFar::NoBasis {
                        reason: NoSpendBasis::NoAccount,
                    },
                    Confidence::Low,
                ),
                now(),
            )
        };
        assert_eq!(
            decision_a.pins.comparison(&decision_a.pins.clone()),
            PinComparison::Identical
        );
    }

    #[test]
    fn pricing_version_unavailable_on_both_sides_is_never_drift() {
        let mut pins_a = ReplayPins::from_decision(
            &RuntimeDecision {
                schema_version: "runtime-decision-v1".to_string(),
                policy_schema_version: "policy-v1".to_string(),
                proposal: crate::progressive::RuntimeDecisionProposal::Proposed(
                    crate::progressive::ProposedAction::Continue {
                        headroom_fraction: None,
                    },
                ),
                remaining: remaining_with(
                    RemainingDuration::Quantiles {
                        p50_secs: 1,
                        p80_secs: 2,
                        p90_secs: 3,
                        conditional_n: 10,
                    },
                    RemainingResource::Unavailable {
                        reason: "x".to_string(),
                    },
                    1,
                    SpendSoFar::NoBasis {
                        reason: NoSpendBasis::NoAccount,
                    },
                    Confidence::Low,
                ),
                protected_criteria: vec![],
                decided_at: now(),
            },
            &PersistedPins::current(),
        );
        let pins_b = pins_a.clone();
        assert_eq!(
            pins_a.pricing_version,
            DimensionValue::Unavailable(DimensionUnavailable::NoGatewayConfigured)
        );
        assert_eq!(pins_a.comparison(&pins_b), PinComparison::Identical);

        // Flip one side to a different *known* pricing version: now it's
        // real drift.
        pins_a.pricing_version = DimensionValue::known("pricing-2026-01");
        let mut pins_c = pins_b.clone();
        pins_c.pricing_version = DimensionValue::known("pricing-2026-02");
        match pins_a.comparison(&pins_c) {
            PinComparison::Drifted { differing } => {
                assert!(differing
                    .iter()
                    .any(|d| d.dimension == PinDimension::PricingVersion));
            }
            PinComparison::Identical => {
                panic!("expected drift between two distinct Known pricing versions")
            }
        }
    }

    // -- Quality-floor type-level guard — the ticket's key AC -------------

    #[test]
    fn a_cheaper_policy_missing_a_required_criterion_is_a_floor_violation_not_a_savings() {
        let task_id = TaskId(uuid::Uuid::new_v4());
        let recorded_policy = test_policy("balanced", ResourceKind::Tokens, Confidence::Low);
        let point = decision_point(
            task_id,
            recorded_policy.clone(),
            remaining_with(
                RemainingDuration::Quantiles {
                    p50_secs: 10,
                    p80_secs: 20,
                    p90_secs: 30,
                    conditional_n: 10,
                },
                RemainingResource::Unavailable {
                    reason: "no gateway".to_string(),
                },
                100,
                SpendSoFar::NoBasis {
                    reason: NoSpendBasis::NoAccount,
                },
                Confidence::Low,
            ),
            now(),
        );

        // A much tighter, "cheaper" policy — but with a quality floor
        // that is a proper, non-empty SUBSET of the recorded one (drops
        // "tests pass"). Policy::validated still accepts it because it
        // has its own single required criterion.
        let cheap_but_unsafe = Policy::validated(
            "cheap_but_unsafe",
            ResourceBound {
                mode: ConstraintMode::Hard,
                target: ResourceAmount::Tokens(1),
                elastic_ceiling: None,
                hard_ceiling: ResourceAmount::Tokens(1),
            },
            TimeBound {
                mode: ConstraintMode::Hard,
                target_secs: 1,
                elastic_ceiling_secs: None,
                hard_ceiling_secs: Some(1),
                deadline: None,
            },
            CompletionContract::first(vec![CompletionCriterion::required(
                "a different, unrelated required criterion",
            )]),
            Confidence::Low,
            AutonomyBoundary::AskOnApproval,
        )
        .expect("valid policy with its own non-empty required set");

        let comparison = PolicyComparison::evaluate(&point, &[], &cheap_but_unsafe);
        match &comparison {
            PolicyComparison::QualityFloorViolated {
                dropped_required, ..
            } => {
                assert_eq!(dropped_required.len(), 1);
                assert_eq!(dropped_required[0].description, "tests pass");
            }
            PolicyComparison::QualityFloorPreserved { .. } => {
                panic!("expected QualityFloorViolated — the candidate drops a required criterion")
            }
        }
        assert!(
            comparison.comparable_regret().is_none(),
            "a floor-violating comparison must expose no regret/savings figure at all"
        );
    }

    #[test]
    fn a_candidate_with_an_identical_or_wider_quality_floor_is_preserved() {
        let task_id = TaskId(uuid::Uuid::new_v4());
        let recorded_policy = test_policy("balanced", ResourceKind::Tokens, Confidence::Low);
        let point = decision_point(
            task_id,
            recorded_policy.clone(),
            remaining_with(
                RemainingDuration::Quantiles {
                    p50_secs: 10,
                    p80_secs: 20,
                    p90_secs: 30,
                    conditional_n: 10,
                },
                RemainingResource::Unavailable {
                    reason: "no gateway".to_string(),
                },
                100,
                SpendSoFar::NoBasis {
                    reason: NoSpendBasis::NoAccount,
                },
                Confidence::Low,
            ),
            now(),
        );
        let candidate = test_policy("strict_budget", ResourceKind::Tokens, Confidence::High);
        let comparison = PolicyComparison::evaluate(&point, &[], &candidate);
        assert!(matches!(
            comparison,
            PolicyComparison::QualityFloorPreserved { .. }
        ));
        assert!(comparison.comparable_regret().is_some());
    }

    // -- Confidence-dimension non-vacuousness -----------------------------

    #[test]
    fn confidence_dimension_alone_makes_replay_non_vacuous_with_no_resource_basis() {
        // Mirrors the real HooksOnly shape: resource arm always
        // Unavailable. Even so, a Low-confidence recorded estimate
        // disagrees with a High-confidence-requiring candidate.
        let task_id = TaskId(uuid::Uuid::new_v4());
        let recorded_policy = test_policy("balanced", ResourceKind::Tokens, Confidence::Low);
        let point = decision_point(
            task_id,
            recorded_policy,
            remaining_with(
                RemainingDuration::Insufficient {
                    conditional_n: 0,
                    required: MIN_REPLAY_SAMPLES,
                    elapsed_secs: 100,
                },
                RemainingResource::Unavailable {
                    reason: "no gateway".to_string(),
                },
                100,
                SpendSoFar::NoBasis {
                    reason: NoSpendBasis::NoAccount,
                },
                Confidence::Low,
            ),
            now(),
        );
        let strict = test_policy("strict_budget", ResourceKind::Tokens, Confidence::High);
        let comparison = PolicyComparison::evaluate(&point, &[], &strict);
        let regret = comparison.comparable_regret().expect("floor preserved");
        assert_eq!(regret.disagreement_counts.candidate_more_restrictive, 1);
    }

    // -- Aggregation overlap refusal --------------------------------------

    #[test]
    fn aggregating_a_parent_and_child_account_is_refused() {
        let parent = AccountId::new();
        let child = AccountId::new();
        let regret_a = TrajectoryRegret {
            task_id: TaskId(uuid::Uuid::new_v4()),
            pins: ReplayPins::from_decision(
                &RuntimeDecision {
                    schema_version: "v".to_string(),
                    policy_schema_version: "v".to_string(),
                    proposal: crate::progressive::RuntimeDecisionProposal::InsufficientEvidence {
                        missing: vec![],
                    },
                    remaining: remaining_with(
                        RemainingDuration::Insufficient {
                            conditional_n: 0,
                            required: 1,
                            elapsed_secs: 1,
                        },
                        RemainingResource::Unavailable {
                            reason: "x".to_string(),
                        },
                        1,
                        SpendSoFar::NoBasis {
                            reason: NoSpendBasis::NoAccount,
                        },
                        Confidence::Low,
                    ),
                    protected_criteria: vec![],
                    decided_at: now(),
                },
                &PersistedPins::current(),
            ),
            actual_policy_name: "balanced".to_string(),
            candidate_policy_name: "strict_budget".to_string(),
            spend_scope: None,
            points_replayed: 1,
            points_skipped: 0,
            disagreement_counts: DisagreementCounts::default(),
            first_candidate_refusal: None,
            elapsed_after_first_candidate_refusal_secs: None,
            admitted_spend_at_first_refusal: None,
            admitted_spend_at_last_point: None,
        };
        let regret_b = regret_a.clone();
        let err = aggregate_regret(&[(parent, None, regret_a), (child, Some(parent), regret_b)])
            .unwrap_err();
        assert_eq!(
            err,
            AggregationError::OverlappingAccountSubtrees {
                ancestor: parent,
                descendant: child,
            }
        );
    }

    #[test]
    fn aggregating_unrelated_accounts_sums_counts_only() {
        let a = AccountId::new();
        let b = AccountId::new();
        let base = TrajectoryRegret {
            task_id: TaskId(uuid::Uuid::new_v4()),
            pins: ReplayPins::from_decision(
                &RuntimeDecision {
                    schema_version: "v".to_string(),
                    policy_schema_version: "v".to_string(),
                    proposal: crate::progressive::RuntimeDecisionProposal::InsufficientEvidence {
                        missing: vec![],
                    },
                    remaining: remaining_with(
                        RemainingDuration::Insufficient {
                            conditional_n: 0,
                            required: 1,
                            elapsed_secs: 1,
                        },
                        RemainingResource::Unavailable {
                            reason: "x".to_string(),
                        },
                        1,
                        SpendSoFar::NoBasis {
                            reason: NoSpendBasis::NoAccount,
                        },
                        Confidence::Low,
                    ),
                    protected_criteria: vec![],
                    decided_at: now(),
                },
                &PersistedPins::current(),
            ),
            actual_policy_name: "balanced".to_string(),
            candidate_policy_name: "strict_budget".to_string(),
            spend_scope: None,
            points_replayed: 3,
            points_skipped: 1,
            disagreement_counts: DisagreementCounts {
                agree: 2,
                candidate_more_restrictive: 1,
                ..Default::default()
            },
            first_candidate_refusal: None,
            elapsed_after_first_candidate_refusal_secs: None,
            admitted_spend_at_first_refusal: None,
            admitted_spend_at_last_point: None,
        };
        let aggregate = aggregate_regret(&[(a, None, base.clone()), (b, None, base)]).unwrap();
        assert_eq!(aggregate.trajectories, 2);
        assert_eq!(aggregate.points_replayed, 6);
        assert_eq!(aggregate.points_skipped, 2);
        assert_eq!(aggregate.disagreement_counts.agree, 4);
    }

    // -- Post-hoc layer ----------------------------------------------------

    #[test]
    fn alternate_outcome_effect_has_exactly_one_producible_variant() {
        let effect = AlternateOutcomeEffect::unknown();
        assert!(matches!(effect, AlternateOutcomeEffect::Unknown { .. }));
    }

    // -- preset round-trip ---------------------------------------------------

    #[test]
    fn preset_by_name_round_trips_through_from_recorded_policy() {
        let inputs = PolicyPresetInputs {
            resource_target: ResourceAmount::Tokens(5000),
            time_target_secs: 1200,
            quality_floor: quality_floor(),
        };
        let original = crate::policy::preset_by_name("deadline_first", inputs).unwrap();
        let recovered_inputs = PolicyPresetInputs::from_recorded_policy(&original);
        let rebuilt = crate::policy::preset_by_name("deadline_first", recovered_inputs).unwrap();
        assert_eq!(original, rebuilt);
    }
}
