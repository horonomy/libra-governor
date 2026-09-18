//! [`Policy`] — the pre-authorized cost/time/quality/confidence/autonomy
//! envelope a task's work is admitted and re-planned against (HORO-1137).
//!
//! # Why not a single weighted score
//!
//! Averaging "went 20% over budget" against "dropped a required test" into
//! one number is a category error: cost overrun is a preference trade-off,
//! a quality-floor violation is a correctness failure. [`Policy`] keeps
//! every constraint dimension — resource, time, quality, confidence,
//! autonomy — as its own explicit field, never collapsed into a score.
//!
//! # Per-constraint modes, not one policy-wide mode
//!
//! [`ConstraintMode`] is attached to the resource and time bounds
//! individually (`Policy::resource.mode`, `Policy::time.mode`), not to
//! `Policy` as a whole. The ticket's own motivating example —
//! "deadline-first... tolerate bounded cost elasticity" — is a policy
//! where the *time* constraint should behave close to [`ConstraintMode::Hard`]
//! while the *resource* constraint behaves as [`ConstraintMode::Elastic`].
//! A single policy-wide mode cannot express that combination at all,
//! whereas a per-constraint mode expresses it directly and lets
//! [`Policy::deadline_first`] and [`Policy::cost_first`] be literal
//! mirror images of each other (see their doc comments).
//!
//! The quality floor is deliberately *not* modal at all — see
//! [`Policy::evaluate`] docs for why required completion criteria are
//! structurally read-only rather than gated by a mode.
//!
//! # This is domain modeling only
//!
//! [`Policy::evaluate`] is a pure function — projected estimate in,
//! [`PolicyDecision`] out — with no side effects and no I/O. It is
//! deliberately not wired into `crates/daemon`'s admission flow in this
//! ticket; that wiring (blocking real actions on a `Deny`, surfacing
//! [`ApprovalRequest`] to a human) is HORO-1139/1141/1144 scope.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{
    completion_contract::{CompletionContract, CompletionCriterion},
    confidence::Confidence,
    resource_amount::{ResourceAmount, ResourceKind},
};

/// The policy schema version every produced [`Policy`] and
/// [`PolicyDecision`] is tagged with (HORO-1137), following the same
/// traceability pattern as [`crate::ESTIMATOR_VERSION`] and
/// [`crate::FEATURE_SCHEMA_VERSION`]: any stored admission, replan, or
/// receipt decision can record exactly which policy semantics produced
/// it. Bump any time [`ConstraintMode`] semantics, [`Policy::evaluate`]'s
/// boundary logic, or a preset's concrete values change.
pub const POLICY_SCHEMA_VERSION: &str = "policy-v1";

/// How a single constraint (resource or time) is enforced against its
/// target and hard ceiling (HORO-1137).
///
/// All three variants are deterministic, pure functions of `(projected,
/// target, elastic_ceiling, hard_ceiling)` — see [`Policy::evaluate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConstraintMode {
    /// No admission when the projected requirement would exceed the hard
    /// ceiling. There is no band: a `Hard` constraint carries no elastic
    /// ceiling (validated at construction — see
    /// [`PolicyValidationError::ResourceElasticCeilingSetForMode`] and
    /// [`PolicyValidationError::TimeElasticCeilingSetForMode`]).
    Hard,
    /// Work is pre-authorized within `target..=elastic_ceiling`.
    /// Projections beyond the elastic ceiling but within the hard
    /// ceiling are a decision point ([`ConstraintOutcome::ApprovalRequired`]),
    /// not a silent pass. Requires an elastic ceiling.
    Elastic,
    /// No pre-authorized band beyond `target` at all: any projection past
    /// `target` (but within the hard ceiling, if one is set) is a
    /// decision point requiring explicit authorization. Unlike `Elastic`,
    /// there is nothing the caller can spend without asking first.
    Approval,
}

/// The pre-authorized resource (cost/tokens/quota) envelope for a policy
/// (HORO-1137).
///
/// `target`, `elastic_ceiling`, and `hard_ceiling` must share the same
/// [`ResourceKind`] — validated by [`Policy::validated`]
/// ([`PolicyValidationError::ResourceKindMismatch`]) rather than left to
/// crash later inside [`Policy::evaluate`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceBound {
    pub mode: ConstraintMode,
    /// The soft, pre-authorized target spend.
    pub target: ResourceAmount,
    /// The pre-authorized elastic band ceiling. Required (`Some`) iff
    /// `mode == ConstraintMode::Elastic`; must be `None` for `Hard` and
    /// `Approval` (see [`ConstraintMode`] docs).
    pub elastic_ceiling: Option<ResourceAmount>,
    /// The absolute ceiling this constraint may never be admitted past.
    /// Always present — a resource constraint always has *some* hard
    /// physical limit, even if it equals `target` (i.e. no slack at
    /// all — see [`Policy::strict_budget`]).
    pub hard_ceiling: ResourceAmount,
}

/// The pre-authorized wall-clock envelope for a policy (HORO-1137).
///
/// `hard_ceiling_secs` is optional (unlike [`ResourceBound::hard_ceiling`]):
/// a task may have a soft time target with genuinely no absolute deadline
/// — see [`ConstraintMode::Approval`] docs. `deadline` is an optional
/// absolute cutoff timestamp on top of the relative `target_secs`/
/// `hard_ceiling_secs`, kept separate because "300 seconds from whenever
/// admission happens" and "must finish before 2026-09-20T00:00:00Z" are
/// different kinds of constraints and a policy may carry either, both, or
/// neither.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TimeBound {
    pub mode: ConstraintMode,
    pub target_secs: u64,
    /// Required (`Some`) iff `mode == ConstraintMode::Elastic`; must be
    /// `None` for `Hard` and `Approval`.
    pub elastic_ceiling_secs: Option<u64>,
    /// Required (`Some`) iff `mode == ConstraintMode::Hard` (a `Hard`
    /// constraint needs a concrete boundary to be deterministic).
    /// Optional otherwise.
    pub hard_ceiling_secs: Option<u64>,
    /// An optional absolute deadline, independent of the relative
    /// target/ceilings above.
    pub deadline: Option<OffsetDateTime>,
}

/// How much unattended action is authorized before a human must be asked
/// (HORO-1137).
///
/// This is metadata carried on [`Policy`] for future gateway/replanning
/// wiring (HORO-1139/1144) to consume — [`Policy::evaluate`] in this
/// ticket does not branch on it, since no runtime enforcement is in
/// scope here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutonomyBoundary {
    /// May act without pausing until a constraint actually trips.
    FullyAutonomous,
    /// Must pause and ask once any constraint reaches
    /// [`ConstraintOutcome::ApprovalRequired`].
    AskOnApproval,
    /// Must confirm before every spend-incurring step, regardless of
    /// whether a constraint has been approached.
    ConfirmEachStep,
}

/// The full pre-authorized envelope work is admitted and re-planned
/// against (HORO-1137). See module docs for why this is several explicit
/// fields rather than one weighted score, and why [`ConstraintMode`] is
/// per-constraint rather than policy-wide.
///
/// Construct via [`Policy::validated`] (or a preset — [`Policy::balanced`],
/// [`Policy::deadline_first`], [`Policy::cost_first`],
/// [`Policy::strict_budget`]) rather than the struct literal directly, so
/// a contradictory configuration is always caught at construction. The
/// struct's fields are still `pub` (matching this crate's existing
/// `Estimate`/`ExecutionReceipt` convention) since a validated `Policy`
/// is otherwise an ordinary read-only data record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Policy {
    /// Traceability tag — see [`POLICY_SCHEMA_VERSION`].
    pub policy_schema_version: String,
    /// Human-readable identifier: a preset name (`"balanced"`,
    /// `"deadline_first"`, `"cost_first"`, `"strict_budget"`) or a
    /// caller-chosen name for a custom policy.
    pub name: String,
    pub resource: ResourceBound,
    pub time: TimeBound,
    /// The Definition of Done this policy protects. Its
    /// [`CompletionContract::required_criteria`] are the quality floor —
    /// there is no separate "quality mode": required criteria are always
    /// non-negotiable, by construction (see [`Policy::evaluate`] docs).
    pub quality_floor: CompletionContract,
    /// The minimum [`Confidence`] an estimate must carry for admission.
    /// A flat gate rather than a per-`ConstraintMode` dimension — unlike
    /// resource/time, there is no meaningful "elastic band" of
    /// confidence to pre-authorize spending into.
    pub min_confidence: Confidence,
    pub autonomy: AutonomyBoundary,
}

/// Why a candidate [`Policy`] configuration was rejected (HORO-1137).
///
/// Every variant is specific and carries the offending values — never a
/// generic string — so a caller (or a test) can match on exactly what was
/// wrong rather than parsing an error message.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PolicyValidationError {
    #[error(
        "resource kind mismatch: target is {target_kind:?}, hard ceiling is {hard_ceiling_kind:?}"
    )]
    ResourceKindMismatch {
        target_kind: ResourceKind,
        hard_ceiling_kind: ResourceKind,
    },
    #[error(
        "resource elastic ceiling kind {elastic_kind:?} does not match target kind {target_kind:?}"
    )]
    ResourceElasticKindMismatch {
        target_kind: ResourceKind,
        elastic_kind: ResourceKind,
    },
    #[error("resource target {target:?} exceeds hard ceiling {hard_ceiling:?}")]
    ResourceTargetExceedsHardCeiling {
        target: ResourceAmount,
        hard_ceiling: ResourceAmount,
    },
    #[error("resource elastic ceiling {elastic:?} is below target {target:?}")]
    ResourceElasticCeilingBelowTarget {
        target: ResourceAmount,
        elastic: ResourceAmount,
    },
    #[error("resource elastic ceiling {elastic:?} exceeds hard ceiling {hard_ceiling:?}")]
    ResourceElasticCeilingAboveHardCeiling {
        elastic: ResourceAmount,
        hard_ceiling: ResourceAmount,
    },
    #[error("resource mode {mode:?} requires an elastic ceiling to be set")]
    ResourceElasticModeMissingCeiling { mode: ConstraintMode },
    #[error("resource mode {mode:?} must not set an elastic ceiling (only Elastic mode does)")]
    ResourceElasticCeilingSetForMode { mode: ConstraintMode },
    #[error("time target {target_secs}s exceeds hard ceiling {hard_ceiling_secs}s")]
    TimeTargetExceedsHardCeiling {
        target_secs: u64,
        hard_ceiling_secs: u64,
    },
    #[error("time elastic ceiling {elastic_secs}s is below target {target_secs}s")]
    TimeElasticCeilingBelowTarget { target_secs: u64, elastic_secs: u64 },
    #[error("time elastic ceiling {elastic_secs}s exceeds hard ceiling {hard_ceiling_secs}s")]
    TimeElasticCeilingAboveHardCeiling {
        elastic_secs: u64,
        hard_ceiling_secs: u64,
    },
    #[error("time mode {mode:?} requires an elastic ceiling to be set")]
    TimeElasticModeMissingCeiling { mode: ConstraintMode },
    #[error("time mode {mode:?} must not set an elastic ceiling (only Elastic mode does)")]
    TimeElasticCeilingSetForMode { mode: ConstraintMode },
    #[error("time mode Hard requires a hard ceiling to be set")]
    TimeHardModeMissingHardCeiling,
    #[error("deadline {deadline} is in the past relative to {now}")]
    DeadlineInPast {
        deadline: OffsetDateTime,
        now: OffsetDateTime,
    },
    #[error("policy quality floor must declare at least one required completion criterion")]
    EmptyRequiredCriteria,
}

impl Policy {
    /// Constructs and validates a `Policy`, using the current wall-clock
    /// time to check `time.deadline`. See [`Self::validated_at`] for a
    /// deterministic, testable variant.
    pub fn validated(
        name: impl Into<String>,
        resource: ResourceBound,
        time: TimeBound,
        quality_floor: CompletionContract,
        min_confidence: Confidence,
        autonomy: AutonomyBoundary,
    ) -> Result<Self, PolicyValidationError> {
        Self::validated_at(
            name,
            resource,
            time,
            quality_floor,
            min_confidence,
            autonomy,
            OffsetDateTime::now_utc(),
        )
    }

    /// Constructs and validates a `Policy` against an explicit `now`,
    /// so deadline-in-the-past validation is deterministic and testable
    /// rather than depending on the system clock.
    #[allow(clippy::too_many_arguments)]
    pub fn validated_at(
        name: impl Into<String>,
        resource: ResourceBound,
        time: TimeBound,
        quality_floor: CompletionContract,
        min_confidence: Confidence,
        autonomy: AutonomyBoundary,
        now: OffsetDateTime,
    ) -> Result<Self, PolicyValidationError> {
        Self::validate_resource(&resource)?;
        Self::validate_time(&time, now)?;
        if quality_floor.required_criteria().next().is_none() {
            return Err(PolicyValidationError::EmptyRequiredCriteria);
        }

        Ok(Self {
            policy_schema_version: POLICY_SCHEMA_VERSION.to_string(),
            name: name.into(),
            resource,
            time,
            quality_floor,
            min_confidence,
            autonomy,
        })
    }

    fn validate_resource(resource: &ResourceBound) -> Result<(), PolicyValidationError> {
        let target_kind = resource.target.kind();
        let hard_kind = resource.hard_ceiling.kind();
        if target_kind != hard_kind {
            return Err(PolicyValidationError::ResourceKindMismatch {
                target_kind,
                hard_ceiling_kind: hard_kind,
            });
        }
        if resource.target.as_f64() > resource.hard_ceiling.as_f64() {
            return Err(PolicyValidationError::ResourceTargetExceedsHardCeiling {
                target: resource.target,
                hard_ceiling: resource.hard_ceiling,
            });
        }

        match (resource.mode, resource.elastic_ceiling) {
            (ConstraintMode::Elastic, None) => {
                return Err(PolicyValidationError::ResourceElasticModeMissingCeiling {
                    mode: resource.mode,
                })
            }
            (ConstraintMode::Hard | ConstraintMode::Approval, Some(_)) => {
                return Err(PolicyValidationError::ResourceElasticCeilingSetForMode {
                    mode: resource.mode,
                })
            }
            (ConstraintMode::Elastic, Some(elastic)) => {
                if elastic.kind() != target_kind {
                    return Err(PolicyValidationError::ResourceElasticKindMismatch {
                        target_kind,
                        elastic_kind: elastic.kind(),
                    });
                }
                if elastic.as_f64() < resource.target.as_f64() {
                    return Err(PolicyValidationError::ResourceElasticCeilingBelowTarget {
                        target: resource.target,
                        elastic,
                    });
                }
                if elastic.as_f64() > resource.hard_ceiling.as_f64() {
                    return Err(
                        PolicyValidationError::ResourceElasticCeilingAboveHardCeiling {
                            elastic,
                            hard_ceiling: resource.hard_ceiling,
                        },
                    );
                }
            }
            (ConstraintMode::Hard | ConstraintMode::Approval, None) => {}
        }

        Ok(())
    }

    fn validate_time(time: &TimeBound, now: OffsetDateTime) -> Result<(), PolicyValidationError> {
        if let Some(deadline) = time.deadline {
            if deadline < now {
                return Err(PolicyValidationError::DeadlineInPast { deadline, now });
            }
        }

        if time.mode == ConstraintMode::Hard && time.hard_ceiling_secs.is_none() {
            return Err(PolicyValidationError::TimeHardModeMissingHardCeiling);
        }

        if let Some(hard_ceiling_secs) = time.hard_ceiling_secs {
            if time.target_secs > hard_ceiling_secs {
                return Err(PolicyValidationError::TimeTargetExceedsHardCeiling {
                    target_secs: time.target_secs,
                    hard_ceiling_secs,
                });
            }
        }

        match (time.mode, time.elastic_ceiling_secs) {
            (ConstraintMode::Elastic, None) => {
                return Err(PolicyValidationError::TimeElasticModeMissingCeiling {
                    mode: time.mode,
                })
            }
            (ConstraintMode::Hard | ConstraintMode::Approval, Some(_)) => {
                return Err(PolicyValidationError::TimeElasticCeilingSetForMode { mode: time.mode })
            }
            (ConstraintMode::Elastic, Some(elastic_secs)) => {
                if elastic_secs < time.target_secs {
                    return Err(PolicyValidationError::TimeElasticCeilingBelowTarget {
                        target_secs: time.target_secs,
                        elastic_secs,
                    });
                }
                if let Some(hard_ceiling_secs) = time.hard_ceiling_secs {
                    if elastic_secs > hard_ceiling_secs {
                        return Err(PolicyValidationError::TimeElasticCeilingAboveHardCeiling {
                            elastic_secs,
                            hard_ceiling_secs,
                        });
                    }
                }
            }
            (ConstraintMode::Hard | ConstraintMode::Approval, None) => {}
        }

        Ok(())
    }
}

/// Why a projected estimate could not be evaluated against a [`Policy`]
/// at all (distinct from [`PolicyValidationError`], which rejects a
/// contradictory *policy*; this rejects incompatible *input data* against
/// an otherwise-valid policy).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PolicyEvaluationError {
    #[error(
        "projected resource kind {projected_kind:?} does not match policy resource kind {policy_kind:?}"
    )]
    ProjectedResourceKindMismatch {
        policy_kind: ResourceKind,
        projected_kind: ResourceKind,
    },
}

/// The outcome of evaluating one constraint (resource or time) against
/// its projected requirement (HORO-1137).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ConstraintOutcome {
    /// Within the pre-authorized envelope; no decision point reached.
    Admit,
    /// Beyond what is pre-authorized but within the hard ceiling (or no
    /// hard ceiling is set): a clean decision point, not a block.
    ApprovalRequired(ApprovalRequest),
    /// Beyond the hard ceiling: admission is refused outright.
    Deny(DenyReason),
}

/// A specific, structured request for explicit authorization to extend a
/// policy constraint — never an ad-hoc string (HORO-1137). Carries every
/// value a human (or a future replanning decision, HORO-1139) needs to
/// judge the request: what was projected, what was pre-authorized, and
/// what the absolute limit is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ApprovalRequest {
    Resource {
        projected: ResourceAmount,
        target: ResourceAmount,
        elastic_ceiling: Option<ResourceAmount>,
        hard_ceiling: ResourceAmount,
    },
    Time {
        projected_secs: u64,
        target_secs: u64,
        elastic_ceiling_secs: Option<u64>,
        hard_ceiling_secs: Option<u64>,
    },
}

/// Why admission was refused outright (HORO-1137). Distinct from
/// [`ApprovalRequest`]: a [`DenyReason`] is not a decision point that
/// more authorization can resolve within the current policy — the hard
/// ceiling (or the confidence floor) was exceeded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DenyReason {
    ResourceExceedsHardCeiling {
        projected: ResourceAmount,
        hard_ceiling: ResourceAmount,
    },
    TimeExceedsHardCeiling {
        projected_secs: u64,
        hard_ceiling_secs: u64,
    },
    ConfidenceBelowThreshold {
        actual: Confidence,
        required: Confidence,
    },
}

/// The aggregate admission verdict across every constraint (HORO-1137).
/// `Deny` wins over `ApprovalRequired` wins over `Admit`: any single hard
/// violation refuses admission regardless of what the other constraints
/// decided; any single approval-required decision point blocks a clean
/// `Admit` even if every other constraint is fully within band.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Admission {
    Admit,
    ApprovalRequired(Vec<ApprovalRequest>),
    Deny(Vec<DenyReason>),
}

/// The full result of evaluating a [`Policy`] against one projected
/// estimate (HORO-1137).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PolicyDecision {
    /// Traceability tag — copied from the [`Policy`] this decision was
    /// produced from, so any stored admission/replan/receipt record
    /// carries which policy schema version decided it.
    pub policy_schema_version: String,
    pub admission: Admission,
    pub resource_outcome: ConstraintOutcome,
    pub time_outcome: ConstraintOutcome,
    /// Whether the projected estimate's confidence met
    /// [`Policy::min_confidence`].
    pub confidence_ok: bool,
    /// The policy's required completion criteria, verbatim. Always
    /// exactly [`Policy::quality_floor`]'s `required_criteria()` — see
    /// [`Policy::evaluate`] docs for why this can never be a trimmed
    /// subset.
    pub protected_criteria: Vec<CompletionCriterion>,
}

impl Policy {
    /// Evaluates this policy against one projected estimate.
    ///
    /// # Quality invariant
    ///
    /// This function's signature takes `&self` — a shared, immutable
    /// reference — and no separate "which criteria are still required"
    /// input. There is structurally no parameter through which a caller
    /// could ask this function to admit work while dropping or weakening
    /// a required [`CompletionCriterion`] to make a tight budget "fit":
    /// [`PolicyDecision::protected_criteria`] is always read straight
    /// from `self.quality_floor.required_criteria()`, never computed,
    /// filtered, or influenced by the projected resource/time/confidence
    /// arguments below. See the `quality_invariant_*` tests in this
    /// module.
    pub fn evaluate(
        &self,
        projected_resource: ResourceAmount,
        projected_duration_secs: u64,
        estimate_confidence: Confidence,
    ) -> Result<PolicyDecision, PolicyEvaluationError> {
        let policy_kind = self.resource.target.kind();
        if projected_resource.kind() != policy_kind {
            return Err(PolicyEvaluationError::ProjectedResourceKindMismatch {
                policy_kind,
                projected_kind: projected_resource.kind(),
            });
        }

        let resource_outcome = Self::evaluate_resource(&self.resource, projected_resource);
        let time_outcome = Self::evaluate_time(&self.time, projected_duration_secs);
        let confidence_ok = estimate_confidence >= self.min_confidence;

        let mut deny_reasons = Vec::new();
        let mut approval_requests = Vec::new();
        for outcome in [&resource_outcome, &time_outcome] {
            match outcome {
                ConstraintOutcome::Deny(reason) => deny_reasons.push(reason.clone()),
                ConstraintOutcome::ApprovalRequired(request) => {
                    approval_requests.push(request.clone())
                }
                ConstraintOutcome::Admit => {}
            }
        }
        if !confidence_ok {
            deny_reasons.push(DenyReason::ConfidenceBelowThreshold {
                actual: estimate_confidence,
                required: self.min_confidence,
            });
        }

        let admission = if !deny_reasons.is_empty() {
            Admission::Deny(deny_reasons)
        } else if !approval_requests.is_empty() {
            Admission::ApprovalRequired(approval_requests)
        } else {
            Admission::Admit
        };

        Ok(PolicyDecision {
            policy_schema_version: self.policy_schema_version.clone(),
            admission,
            resource_outcome,
            time_outcome,
            confidence_ok,
            protected_criteria: self.quality_floor.required_criteria().cloned().collect(),
        })
    }

    fn evaluate_resource(bound: &ResourceBound, projected: ResourceAmount) -> ConstraintOutcome {
        let p = projected.as_f64();
        let target = bound.target.as_f64();
        let hard = bound.hard_ceiling.as_f64();

        match bound.mode {
            ConstraintMode::Hard => {
                if p > hard {
                    ConstraintOutcome::Deny(DenyReason::ResourceExceedsHardCeiling {
                        projected,
                        hard_ceiling: bound.hard_ceiling,
                    })
                } else {
                    ConstraintOutcome::Admit
                }
            }
            ConstraintMode::Elastic => {
                let elastic = bound
                    .elastic_ceiling
                    .expect("validated: Elastic mode always has an elastic_ceiling");
                if p <= target || p <= elastic.as_f64() {
                    ConstraintOutcome::Admit
                } else if p <= hard {
                    ConstraintOutcome::ApprovalRequired(ApprovalRequest::Resource {
                        projected,
                        target: bound.target,
                        elastic_ceiling: Some(elastic),
                        hard_ceiling: bound.hard_ceiling,
                    })
                } else {
                    ConstraintOutcome::Deny(DenyReason::ResourceExceedsHardCeiling {
                        projected,
                        hard_ceiling: bound.hard_ceiling,
                    })
                }
            }
            ConstraintMode::Approval => {
                if p <= target {
                    ConstraintOutcome::Admit
                } else if p <= hard {
                    ConstraintOutcome::ApprovalRequired(ApprovalRequest::Resource {
                        projected,
                        target: bound.target,
                        elastic_ceiling: None,
                        hard_ceiling: bound.hard_ceiling,
                    })
                } else {
                    ConstraintOutcome::Deny(DenyReason::ResourceExceedsHardCeiling {
                        projected,
                        hard_ceiling: bound.hard_ceiling,
                    })
                }
            }
        }
    }

    fn evaluate_time(bound: &TimeBound, projected_secs: u64) -> ConstraintOutcome {
        match bound.mode {
            ConstraintMode::Hard => {
                let hard = bound
                    .hard_ceiling_secs
                    .expect("validated: Hard mode always has a hard_ceiling_secs");
                if projected_secs > hard {
                    ConstraintOutcome::Deny(DenyReason::TimeExceedsHardCeiling {
                        projected_secs,
                        hard_ceiling_secs: hard,
                    })
                } else {
                    ConstraintOutcome::Admit
                }
            }
            ConstraintMode::Elastic => {
                let elastic = bound
                    .elastic_ceiling_secs
                    .expect("validated: Elastic mode always has an elastic_ceiling_secs");
                if projected_secs <= bound.target_secs || projected_secs <= elastic {
                    ConstraintOutcome::Admit
                } else if bound
                    .hard_ceiling_secs
                    .is_none_or(|hard| projected_secs <= hard)
                {
                    ConstraintOutcome::ApprovalRequired(ApprovalRequest::Time {
                        projected_secs,
                        target_secs: bound.target_secs,
                        elastic_ceiling_secs: Some(elastic),
                        hard_ceiling_secs: bound.hard_ceiling_secs,
                    })
                } else {
                    ConstraintOutcome::Deny(DenyReason::TimeExceedsHardCeiling {
                        projected_secs,
                        hard_ceiling_secs: bound
                            .hard_ceiling_secs
                            .expect("checked is_none_or above"),
                    })
                }
            }
            ConstraintMode::Approval => {
                if projected_secs <= bound.target_secs {
                    ConstraintOutcome::Admit
                } else if bound
                    .hard_ceiling_secs
                    .is_none_or(|hard| projected_secs <= hard)
                {
                    ConstraintOutcome::ApprovalRequired(ApprovalRequest::Time {
                        projected_secs,
                        target_secs: bound.target_secs,
                        elastic_ceiling_secs: None,
                        hard_ceiling_secs: bound.hard_ceiling_secs,
                    })
                } else {
                    ConstraintOutcome::Deny(DenyReason::TimeExceedsHardCeiling {
                        projected_secs,
                        hard_ceiling_secs: bound
                            .hard_ceiling_secs
                            .expect("checked is_none_or above"),
                    })
                }
            }
        }
    }
}

/// The task-specific inputs a preset compiles into a concrete [`Policy`]
/// (HORO-1137). Presets fix *how* constraints are enforced (which
/// [`ConstraintMode`], which multiplier); `PolicyPresetInputs` supplies
/// the task-specific *values* (what the target spend/deadline/quality
/// floor actually are) — see [`Policy::balanced`] and its sibling
/// presets.
#[derive(Debug, Clone, PartialEq)]
pub struct PolicyPresetInputs {
    pub resource_target: ResourceAmount,
    pub time_target_secs: u64,
    pub quality_floor: CompletionContract,
}

impl Policy {
    /// Balanced pre-authorization: both cost and time get a moderate
    /// elastic band (target..=1.25x, hard ceiling at 1.5x), medium
    /// confidence required, ask before crossing into either band's
    /// approval-required zone.
    pub fn balanced(inputs: PolicyPresetInputs) -> Result<Self, PolicyValidationError> {
        Self::validated(
            "balanced",
            ResourceBound {
                mode: ConstraintMode::Elastic,
                target: inputs.resource_target,
                elastic_ceiling: Some(inputs.resource_target.scaled(1.25)),
                hard_ceiling: inputs.resource_target.scaled(1.5),
            },
            TimeBound {
                mode: ConstraintMode::Elastic,
                target_secs: inputs.time_target_secs,
                elastic_ceiling_secs: Some(scale_secs(inputs.time_target_secs, 1.25)),
                hard_ceiling_secs: Some(scale_secs(inputs.time_target_secs, 1.5)),
                deadline: None,
            },
            inputs.quality_floor,
            Confidence::Medium,
            AutonomyBoundary::AskOnApproval,
        )
    }

    /// Deadline-first: time is `Hard` with zero slack (the deadline is
    /// firm), while cost is `Elastic` with a *wider* band than
    /// [`Self::balanced`] (target..=2x, hard ceiling at 3x) — the
    /// tolerated cost elasticity the ticket describes. The quality floor
    /// is identical to every other preset: deadline pressure never
    /// relaxes required completion criteria (see the
    /// `deadline_first_preserves_quality_floor_while_spending_more`
    /// test).
    pub fn deadline_first(inputs: PolicyPresetInputs) -> Result<Self, PolicyValidationError> {
        Self::validated(
            "deadline_first",
            ResourceBound {
                mode: ConstraintMode::Elastic,
                target: inputs.resource_target,
                elastic_ceiling: Some(inputs.resource_target.scaled(2.0)),
                hard_ceiling: inputs.resource_target.scaled(3.0),
            },
            TimeBound {
                mode: ConstraintMode::Hard,
                target_secs: inputs.time_target_secs,
                elastic_ceiling_secs: None,
                hard_ceiling_secs: Some(inputs.time_target_secs),
                deadline: None,
            },
            inputs.quality_floor,
            Confidence::Medium,
            AutonomyBoundary::AskOnApproval,
        )
    }

    /// Cost-first: the mirror image of [`Self::deadline_first`] — cost is
    /// `Hard` with zero slack (the budget is firm), while time is
    /// `Elastic` with a wide band (target..=2x, hard ceiling at 3x).
    pub fn cost_first(inputs: PolicyPresetInputs) -> Result<Self, PolicyValidationError> {
        Self::validated(
            "cost_first",
            ResourceBound {
                mode: ConstraintMode::Hard,
                target: inputs.resource_target,
                elastic_ceiling: None,
                hard_ceiling: inputs.resource_target,
            },
            TimeBound {
                mode: ConstraintMode::Elastic,
                target_secs: inputs.time_target_secs,
                elastic_ceiling_secs: Some(scale_secs(inputs.time_target_secs, 2.0)),
                hard_ceiling_secs: Some(scale_secs(inputs.time_target_secs, 3.0)),
                deadline: None,
            },
            inputs.quality_floor,
            Confidence::Medium,
            AutonomyBoundary::AskOnApproval,
        )
    }

    /// Strict budget: both cost and time are `Hard` with zero slack, the
    /// highest confidence bar ([`Confidence::High`]), and the most
    /// conservative autonomy boundary — confirm every spend-incurring
    /// step. For tasks where neither cost nor schedule overrun is
    /// tolerable at all.
    pub fn strict_budget(inputs: PolicyPresetInputs) -> Result<Self, PolicyValidationError> {
        Self::validated(
            "strict_budget",
            ResourceBound {
                mode: ConstraintMode::Hard,
                target: inputs.resource_target,
                elastic_ceiling: None,
                hard_ceiling: inputs.resource_target,
            },
            TimeBound {
                mode: ConstraintMode::Hard,
                target_secs: inputs.time_target_secs,
                elastic_ceiling_secs: None,
                hard_ceiling_secs: Some(inputs.time_target_secs),
                deadline: None,
            },
            inputs.quality_floor,
            Confidence::High,
            AutonomyBoundary::ConfirmEachStep,
        )
    }
}

/// Scales a duration in seconds by `factor`, rounding to the nearest
/// second. Kept local to this module (rather than a general `Duration`
/// extension) since it only exists to derive elastic/hard ceilings for
/// [`TimeBound`] from a target the same way [`ResourceAmount::scaled`]
/// does for resources.
fn scale_secs(secs: u64, factor: f64) -> u64 {
    ((secs as f64) * factor).round() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quality_floor() -> CompletionContract {
        CompletionContract::first(vec![
            CompletionCriterion::required("all new code has passing unit tests"),
            CompletionCriterion::optional("docs updated"),
        ])
    }

    fn hard_resource_policy() -> Policy {
        Policy::validated(
            "test-hard-resource",
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
            Confidence::Medium,
            AutonomyBoundary::AskOnApproval,
        )
        .expect("valid policy")
    }

    #[test]
    fn hard_mode_admits_exactly_at_the_boundary_value() {
        let policy = hard_resource_policy();
        let decision = policy
            .evaluate(ResourceAmount::UsdCents(1000), 600, Confidence::Medium)
            .expect("compatible kind");
        assert_eq!(decision.resource_outcome, ConstraintOutcome::Admit);
        assert_eq!(decision.time_outcome, ConstraintOutcome::Admit);
        assert_eq!(decision.admission, Admission::Admit);
    }

    #[test]
    fn hard_mode_denies_exactly_one_unit_past_the_boundary() {
        let policy = hard_resource_policy();
        let decision = policy
            .evaluate(ResourceAmount::UsdCents(1001), 601, Confidence::Medium)
            .expect("compatible kind");
        assert_eq!(
            decision.resource_outcome,
            ConstraintOutcome::Deny(DenyReason::ResourceExceedsHardCeiling {
                projected: ResourceAmount::UsdCents(1001),
                hard_ceiling: ResourceAmount::UsdCents(1000),
            })
        );
        assert_eq!(
            decision.time_outcome,
            ConstraintOutcome::Deny(DenyReason::TimeExceedsHardCeiling {
                projected_secs: 601,
                hard_ceiling_secs: 600,
            })
        );
        match decision.admission {
            Admission::Deny(reasons) => assert_eq!(reasons.len(), 2),
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn hard_mode_admits_below_the_boundary() {
        let policy = hard_resource_policy();
        let decision = policy
            .evaluate(ResourceAmount::UsdCents(999), 599, Confidence::Medium)
            .expect("compatible kind");
        assert_eq!(decision.admission, Admission::Admit);
    }

    fn elastic_resource_policy() -> Policy {
        Policy::validated(
            "test-elastic-resource",
            ResourceBound {
                mode: ConstraintMode::Elastic,
                target: ResourceAmount::UsdCents(1000),
                elastic_ceiling: Some(ResourceAmount::UsdCents(1500)),
                hard_ceiling: ResourceAmount::UsdCents(2000),
            },
            TimeBound {
                mode: ConstraintMode::Elastic,
                target_secs: 600,
                elastic_ceiling_secs: Some(900),
                hard_ceiling_secs: Some(1200),
                deadline: None,
            },
            quality_floor(),
            Confidence::Medium,
            AutonomyBoundary::AskOnApproval,
        )
        .expect("valid policy")
    }

    #[test]
    fn elastic_mode_admits_within_target() {
        let policy = elastic_resource_policy();
        let decision = policy
            .evaluate(ResourceAmount::UsdCents(1000), 600, Confidence::Medium)
            .expect("compatible kind");
        assert_eq!(decision.admission, Admission::Admit);
    }

    #[test]
    fn elastic_mode_admits_exactly_at_the_elastic_ceiling() {
        let policy = elastic_resource_policy();
        let decision = policy
            .evaluate(ResourceAmount::UsdCents(1500), 900, Confidence::Medium)
            .expect("compatible kind");
        assert_eq!(decision.admission, Admission::Admit);
    }

    #[test]
    fn elastic_mode_requires_approval_between_elastic_and_hard_ceiling() {
        let policy = elastic_resource_policy();
        let decision = policy
            .evaluate(ResourceAmount::UsdCents(1501), 901, Confidence::Medium)
            .expect("compatible kind");
        assert_eq!(
            decision.resource_outcome,
            ConstraintOutcome::ApprovalRequired(ApprovalRequest::Resource {
                projected: ResourceAmount::UsdCents(1501),
                target: ResourceAmount::UsdCents(1000),
                elastic_ceiling: Some(ResourceAmount::UsdCents(1500)),
                hard_ceiling: ResourceAmount::UsdCents(2000),
            })
        );
        match &decision.admission {
            Admission::ApprovalRequired(requests) => assert_eq!(requests.len(), 2),
            other => panic!("expected ApprovalRequired, got {other:?}"),
        }
    }

    #[test]
    fn elastic_mode_admits_exactly_at_the_hard_ceiling_as_approval_required() {
        // At the hard ceiling itself: still ApprovalRequired, not Deny —
        // Deny is reserved for strictly *past* the hard ceiling.
        let policy = elastic_resource_policy();
        let decision = policy
            .evaluate(ResourceAmount::UsdCents(2000), 1200, Confidence::Medium)
            .expect("compatible kind");
        assert!(matches!(
            decision.resource_outcome,
            ConstraintOutcome::ApprovalRequired(_)
        ));
        assert!(matches!(
            decision.time_outcome,
            ConstraintOutcome::ApprovalRequired(_)
        ));
    }

    #[test]
    fn elastic_mode_denies_past_the_hard_ceiling() {
        let policy = elastic_resource_policy();
        let decision = policy
            .evaluate(ResourceAmount::UsdCents(2001), 1201, Confidence::Medium)
            .expect("compatible kind");
        assert_eq!(
            decision.resource_outcome,
            ConstraintOutcome::Deny(DenyReason::ResourceExceedsHardCeiling {
                projected: ResourceAmount::UsdCents(2001),
                hard_ceiling: ResourceAmount::UsdCents(2000),
            })
        );
        assert!(matches!(decision.admission, Admission::Deny(_)));
    }

    fn approval_resource_policy() -> Policy {
        Policy::validated(
            "test-approval-resource",
            ResourceBound {
                mode: ConstraintMode::Approval,
                target: ResourceAmount::UsdCents(1000),
                elastic_ceiling: None,
                hard_ceiling: ResourceAmount::UsdCents(2000),
            },
            TimeBound {
                mode: ConstraintMode::Approval,
                target_secs: 600,
                elastic_ceiling_secs: None,
                hard_ceiling_secs: Some(1200),
                deadline: None,
            },
            quality_floor(),
            Confidence::Medium,
            AutonomyBoundary::AskOnApproval,
        )
        .expect("valid policy")
    }

    #[test]
    fn approval_mode_admits_at_and_below_target_with_no_pre_authorized_band() {
        let policy = approval_resource_policy();
        let decision = policy
            .evaluate(ResourceAmount::UsdCents(1000), 600, Confidence::Medium)
            .expect("compatible kind");
        assert_eq!(decision.admission, Admission::Admit);
    }

    #[test]
    fn approval_mode_requires_approval_for_any_amount_past_target() {
        // Unlike Elastic, there is nothing the caller can spend past
        // target without asking — the very next unit already requires
        // approval.
        let policy = approval_resource_policy();
        let decision = policy
            .evaluate(ResourceAmount::UsdCents(1001), 601, Confidence::Medium)
            .expect("compatible kind");
        assert_eq!(
            decision.resource_outcome,
            ConstraintOutcome::ApprovalRequired(ApprovalRequest::Resource {
                projected: ResourceAmount::UsdCents(1001),
                target: ResourceAmount::UsdCents(1000),
                elastic_ceiling: None,
                hard_ceiling: ResourceAmount::UsdCents(2000),
            })
        );
    }

    #[test]
    fn approval_mode_denies_past_the_hard_ceiling() {
        let policy = approval_resource_policy();
        let decision = policy
            .evaluate(ResourceAmount::UsdCents(2001), 1201, Confidence::Medium)
            .expect("compatible kind");
        assert!(matches!(decision.admission, Admission::Deny(_)));
    }

    #[test]
    fn approval_mode_time_with_no_hard_ceiling_never_denies() {
        let policy = Policy::validated(
            "test-approval-open-ended-time",
            ResourceBound {
                mode: ConstraintMode::Hard,
                target: ResourceAmount::UsdCents(1000),
                elastic_ceiling: None,
                hard_ceiling: ResourceAmount::UsdCents(1000),
            },
            TimeBound {
                mode: ConstraintMode::Approval,
                target_secs: 600,
                elastic_ceiling_secs: None,
                hard_ceiling_secs: None,
                deadline: None,
            },
            quality_floor(),
            Confidence::Medium,
            AutonomyBoundary::AskOnApproval,
        )
        .expect("valid policy");

        let decision = policy
            .evaluate(
                ResourceAmount::UsdCents(1000),
                1_000_000,
                Confidence::Medium,
            )
            .expect("compatible kind");
        assert!(matches!(
            decision.time_outcome,
            ConstraintOutcome::ApprovalRequired(_)
        ));
    }

    #[test]
    fn confidence_below_threshold_denies_admission_independent_of_bounds() {
        let policy = hard_resource_policy();
        let decision = policy
            .evaluate(ResourceAmount::UsdCents(1), 1, Confidence::Low)
            .expect("compatible kind");
        assert!(!decision.confidence_ok);
        match decision.admission {
            Admission::Deny(reasons) => assert_eq!(
                reasons,
                vec![DenyReason::ConfidenceBelowThreshold {
                    actual: Confidence::Low,
                    required: Confidence::Medium,
                }]
            ),
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn confidence_exactly_at_threshold_passes() {
        let policy = hard_resource_policy();
        let decision = policy
            .evaluate(ResourceAmount::UsdCents(1), 1, Confidence::Medium)
            .expect("compatible kind");
        assert!(decision.confidence_ok);
    }

    fn preset_inputs() -> PolicyPresetInputs {
        PolicyPresetInputs {
            resource_target: ResourceAmount::UsdCents(1000),
            time_target_secs: 600,
            quality_floor: quality_floor(),
        }
    }

    #[test]
    fn deadline_first_preserves_quality_floor_while_spending_more() {
        let balanced = Policy::balanced(preset_inputs()).expect("valid policy");
        let deadline_first = Policy::deadline_first(preset_inputs()).expect("valid policy");

        // The quality floor is identical, byte-for-byte — deadline
        // pressure never relaxes required criteria.
        assert_eq!(balanced.quality_floor, deadline_first.quality_floor);
        assert_eq!(
            balanced
                .quality_floor
                .required_criteria()
                .collect::<Vec<_>>(),
            deadline_first
                .quality_floor
                .required_criteria()
                .collect::<Vec<_>>(),
        );

        // A spend that balanced's Elastic band would already push into
        // ApprovalRequired territory is still a clean Admit under
        // deadline_first's wider, pre-authorized cost elasticity.
        let projected_spend = ResourceAmount::UsdCents(1400);
        let time_within_both_targets = 600;

        let balanced_decision = balanced
            .evaluate(
                projected_spend,
                time_within_both_targets,
                Confidence::Medium,
            )
            .expect("compatible kind");
        let deadline_first_decision = deadline_first
            .evaluate(
                projected_spend,
                time_within_both_targets,
                Confidence::Medium,
            )
            .expect("compatible kind");

        assert!(
            matches!(
                balanced_decision.resource_outcome,
                ConstraintOutcome::ApprovalRequired(_)
            ),
            "expected balanced's narrower cost band to require approval at 1400, got {:?}",
            balanced_decision.resource_outcome
        );
        assert_eq!(
            deadline_first_decision.resource_outcome,
            ConstraintOutcome::Admit,
            "expected deadline_first's wider cost band to admit 1800 cleanly"
        );
        assert_eq!(deadline_first_decision.admission, Admission::Admit);

        // Both decisions protect exactly the same required criteria —
        // spending more within pre-authorized bounds never touched them.
        assert_eq!(
            balanced_decision.protected_criteria,
            deadline_first_decision.protected_criteria
        );
        assert_eq!(
            deadline_first_decision.protected_criteria,
            deadline_first
                .quality_floor
                .required_criteria()
                .cloned()
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn deadline_first_time_bound_is_tighter_than_balanced() {
        let balanced = Policy::balanced(preset_inputs()).expect("valid policy");
        let deadline_first = Policy::deadline_first(preset_inputs()).expect("valid policy");

        assert_eq!(deadline_first.time.mode, ConstraintMode::Hard);
        assert_eq!(
            deadline_first.time.hard_ceiling_secs,
            Some(preset_inputs().time_target_secs),
            "deadline_first's time hard ceiling has zero slack above target"
        );
        assert!(
            deadline_first.time.hard_ceiling_secs.unwrap()
                < balanced.time.hard_ceiling_secs.unwrap(),
            "deadline_first must be at least as tight on time as balanced"
        );
    }

    #[test]
    fn quality_invariant_required_criteria_survive_a_tight_projected_budget() {
        // A policy whose quality floor has many required criteria and a
        // Hard resource bound with essentially no room at all.
        let many_required = CompletionContract::first(vec![
            CompletionCriterion::required("criterion 1"),
            CompletionCriterion::required("criterion 2"),
            CompletionCriterion::required("criterion 3"),
            CompletionCriterion::optional("nice to have"),
        ]);
        let policy = Policy::validated(
            "tight-budget",
            ResourceBound {
                mode: ConstraintMode::Hard,
                target: ResourceAmount::UsdCents(1),
                elastic_ceiling: None,
                hard_ceiling: ResourceAmount::UsdCents(1),
            },
            TimeBound {
                mode: ConstraintMode::Hard,
                target_secs: 1,
                elastic_ceiling_secs: None,
                hard_ceiling_secs: Some(1),
                deadline: None,
            },
            many_required.clone(),
            Confidence::Low,
            AutonomyBoundary::ConfirmEachStep,
        )
        .expect("valid policy");

        // Even a projection that blows through every bound...
        let decision = policy
            .evaluate(
                ResourceAmount::UsdCents(1_000_000),
                1_000_000,
                Confidence::Low,
            )
            .expect("compatible kind");
        assert!(matches!(decision.admission, Admission::Deny(_)));

        // ...still reports the full, untrimmed set of required criteria.
        // There is no code path — and, structurally, no parameter — by
        // which evaluate() could have dropped one to "make it fit".
        let expected: Vec<_> = many_required.required_criteria().cloned().collect();
        assert_eq!(expected.len(), 3);
        assert_eq!(decision.protected_criteria, expected);

        // And a comfortably-within-bounds projection reports the exact
        // same set — the criteria are not a function of the projection.
        let admitting_decision = policy
            .evaluate(ResourceAmount::UsdCents(1), 1, Confidence::Low)
            .expect("compatible kind");
        assert_eq!(admitting_decision.admission, Admission::Admit);
        assert_eq!(admitting_decision.protected_criteria, expected);
    }

    #[test]
    fn presets_are_deterministic_and_inspectable() {
        let a = Policy::balanced(preset_inputs()).expect("valid policy");
        let b = Policy::balanced(preset_inputs()).expect("valid policy");
        assert_eq!(
            a, b,
            "same preset name + inputs must produce an identical Policy value"
        );

        assert_eq!(
            a,
            Policy {
                policy_schema_version: POLICY_SCHEMA_VERSION.to_string(),
                name: "balanced".to_string(),
                resource: ResourceBound {
                    mode: ConstraintMode::Elastic,
                    target: ResourceAmount::UsdCents(1000),
                    elastic_ceiling: Some(ResourceAmount::UsdCents(1250)),
                    hard_ceiling: ResourceAmount::UsdCents(1500),
                },
                time: TimeBound {
                    mode: ConstraintMode::Elastic,
                    target_secs: 600,
                    elastic_ceiling_secs: Some(750),
                    hard_ceiling_secs: Some(900),
                    deadline: None,
                },
                quality_floor: quality_floor(),
                min_confidence: Confidence::Medium,
                autonomy: AutonomyBoundary::AskOnApproval,
            },
            "balanced must compile to exactly this concrete Policy value, no hidden logic"
        );
    }

    #[test]
    fn policy_schema_version_propagates_from_policy_to_decision() {
        let policy = hard_resource_policy();
        assert_eq!(policy.policy_schema_version, POLICY_SCHEMA_VERSION);

        let decision = policy
            .evaluate(ResourceAmount::UsdCents(1), 1, Confidence::Medium)
            .expect("compatible kind");
        assert_eq!(decision.policy_schema_version, POLICY_SCHEMA_VERSION);
        assert_eq!(decision.policy_schema_version, policy.policy_schema_version);
    }

    #[test]
    fn rejects_elastic_ceiling_below_target() {
        let err = Policy::validated(
            "invalid",
            ResourceBound {
                mode: ConstraintMode::Elastic,
                target: ResourceAmount::UsdCents(1000),
                elastic_ceiling: Some(ResourceAmount::UsdCents(500)),
                hard_ceiling: ResourceAmount::UsdCents(2000),
            },
            TimeBound {
                mode: ConstraintMode::Hard,
                target_secs: 600,
                elastic_ceiling_secs: None,
                hard_ceiling_secs: Some(600),
                deadline: None,
            },
            quality_floor(),
            Confidence::Medium,
            AutonomyBoundary::AskOnApproval,
        )
        .unwrap_err();
        assert_eq!(
            err,
            PolicyValidationError::ResourceElasticCeilingBelowTarget {
                target: ResourceAmount::UsdCents(1000),
                elastic: ResourceAmount::UsdCents(500),
            }
        );
    }

    #[test]
    fn rejects_hard_ceiling_below_elastic_ceiling() {
        let err = Policy::validated(
            "invalid",
            ResourceBound {
                mode: ConstraintMode::Elastic,
                target: ResourceAmount::UsdCents(1000),
                elastic_ceiling: Some(ResourceAmount::UsdCents(1500)),
                hard_ceiling: ResourceAmount::UsdCents(1200),
            },
            TimeBound {
                mode: ConstraintMode::Hard,
                target_secs: 600,
                elastic_ceiling_secs: None,
                hard_ceiling_secs: Some(600),
                deadline: None,
            },
            quality_floor(),
            Confidence::Medium,
            AutonomyBoundary::AskOnApproval,
        )
        .unwrap_err();
        assert_eq!(
            err,
            PolicyValidationError::ResourceElasticCeilingAboveHardCeiling {
                elastic: ResourceAmount::UsdCents(1500),
                hard_ceiling: ResourceAmount::UsdCents(1200),
            }
        );
    }

    #[test]
    fn rejects_target_above_hard_ceiling() {
        let err = Policy::validated(
            "invalid",
            ResourceBound {
                mode: ConstraintMode::Hard,
                target: ResourceAmount::UsdCents(2000),
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
            Confidence::Medium,
            AutonomyBoundary::AskOnApproval,
        )
        .unwrap_err();
        assert_eq!(
            err,
            PolicyValidationError::ResourceTargetExceedsHardCeiling {
                target: ResourceAmount::UsdCents(2000),
                hard_ceiling: ResourceAmount::UsdCents(1000),
            }
        );
    }

    #[test]
    fn rejects_deadline_in_the_past() {
        let now = OffsetDateTime::now_utc();
        let deadline = now - time::Duration::days(1);
        let err = Policy::validated_at(
            "invalid",
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
                deadline: Some(deadline),
            },
            quality_floor(),
            Confidence::Medium,
            AutonomyBoundary::AskOnApproval,
            now,
        )
        .unwrap_err();
        assert_eq!(err, PolicyValidationError::DeadlineInPast { deadline, now });
    }

    #[test]
    fn rejects_empty_required_criteria() {
        let no_required_criteria =
            CompletionContract::first(vec![CompletionCriterion::optional("nice to have")]);
        let err = Policy::validated(
            "invalid",
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
            no_required_criteria,
            Confidence::Medium,
            AutonomyBoundary::AskOnApproval,
        )
        .unwrap_err();
        assert_eq!(err, PolicyValidationError::EmptyRequiredCriteria);
    }

    #[test]
    fn rejects_hard_mode_missing_hard_ceiling() {
        let err = Policy::validated(
            "invalid",
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
                hard_ceiling_secs: None,
                deadline: None,
            },
            quality_floor(),
            Confidence::Medium,
            AutonomyBoundary::AskOnApproval,
        )
        .unwrap_err();
        assert_eq!(err, PolicyValidationError::TimeHardModeMissingHardCeiling);
    }

    #[test]
    fn rejects_elastic_ceiling_set_for_non_elastic_mode() {
        let err = Policy::validated(
            "invalid",
            ResourceBound {
                mode: ConstraintMode::Hard,
                target: ResourceAmount::UsdCents(1000),
                elastic_ceiling: Some(ResourceAmount::UsdCents(1200)),
                hard_ceiling: ResourceAmount::UsdCents(1500),
            },
            TimeBound {
                mode: ConstraintMode::Hard,
                target_secs: 600,
                elastic_ceiling_secs: None,
                hard_ceiling_secs: Some(600),
                deadline: None,
            },
            quality_floor(),
            Confidence::Medium,
            AutonomyBoundary::AskOnApproval,
        )
        .unwrap_err();
        assert_eq!(
            err,
            PolicyValidationError::ResourceElasticCeilingSetForMode {
                mode: ConstraintMode::Hard,
            }
        );
    }

    #[test]
    fn rejects_elastic_mode_missing_elastic_ceiling() {
        let err = Policy::validated(
            "invalid",
            ResourceBound {
                mode: ConstraintMode::Elastic,
                target: ResourceAmount::UsdCents(1000),
                elastic_ceiling: None,
                hard_ceiling: ResourceAmount::UsdCents(1500),
            },
            TimeBound {
                mode: ConstraintMode::Hard,
                target_secs: 600,
                elastic_ceiling_secs: None,
                hard_ceiling_secs: Some(600),
                deadline: None,
            },
            quality_floor(),
            Confidence::Medium,
            AutonomyBoundary::AskOnApproval,
        )
        .unwrap_err();
        assert_eq!(
            err,
            PolicyValidationError::ResourceElasticModeMissingCeiling {
                mode: ConstraintMode::Elastic,
            }
        );
    }

    #[test]
    fn rejects_resource_kind_mismatch() {
        let err = Policy::validated(
            "invalid",
            ResourceBound {
                mode: ConstraintMode::Hard,
                target: ResourceAmount::UsdCents(1000),
                elastic_ceiling: None,
                hard_ceiling: ResourceAmount::Tokens(2000),
            },
            TimeBound {
                mode: ConstraintMode::Hard,
                target_secs: 600,
                elastic_ceiling_secs: None,
                hard_ceiling_secs: Some(600),
                deadline: None,
            },
            quality_floor(),
            Confidence::Medium,
            AutonomyBoundary::AskOnApproval,
        )
        .unwrap_err();
        assert_eq!(
            err,
            PolicyValidationError::ResourceKindMismatch {
                target_kind: ResourceKind::Usd,
                hard_ceiling_kind: ResourceKind::Tokens,
            }
        );
    }

    #[test]
    fn evaluate_rejects_projected_resource_kind_mismatch() {
        let policy = hard_resource_policy();
        let err = policy
            .evaluate(ResourceAmount::Tokens(1), 1, Confidence::Medium)
            .unwrap_err();
        assert_eq!(
            err,
            PolicyEvaluationError::ProjectedResourceKindMismatch {
                policy_kind: ResourceKind::Usd,
                projected_kind: ResourceKind::Tokens,
            }
        );
    }
}
