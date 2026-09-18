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
    /// [`PolicyValidationError::ElasticCeilingNotAllowedForMode`]).
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
/// [`ResourceKind`] — validated by [`Policy::validate`]
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
    TimeElasticCeilingBelowTarget {
        target_secs: u64,
        elastic_secs: u64,
    },
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
                return Err(PolicyValidationError::TimeElasticModeMissingCeiling { mode: time.mode })
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
