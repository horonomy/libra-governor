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

impl ResourceBound {
    /// Shifts `target`, `elastic_ceiling`, and `hard_ceiling` all upward
    /// by `granted` (HORO-1727's bounded renewal mechanism) — never just
    /// `hard_ceiling`, because shifting only the hard ceiling would leave
    /// an `Elastic`/`Approval`-mode task stuck at `ApprovalRequired`
    /// forever even after real capacity was granted.
    ///
    /// Returns `self` unchanged (not merely numerically equal — the same
    /// value, byte-for-byte) whenever `granted` is zero or its
    /// [`ResourceKind`] does not match this bound's own kind. The latter
    /// should never happen in practice (a task's resource kind is fixed
    /// at admission and every renewal grant against it is validated to
    /// match — see [`Policy::validate_resource`]), so it is treated as
    /// "no granted capacity" defensively rather than silently mixing
    /// units or panicking.
    pub fn extended_by(&self, granted: ResourceAmount) -> ResourceBound {
        if granted.kind() != self.target.kind() || granted.as_f64() == 0.0 {
            return self.clone();
        }
        let add = granted.as_f64();
        let shift = |amount: ResourceAmount| {
            ResourceAmount::from_kind_f64(amount.kind(), amount.as_f64() + add)
        };
        ResourceBound {
            mode: self.mode,
            target: shift(self.target),
            elastic_ceiling: self.elastic_ceiling.map(shift),
            hard_ceiling: shift(self.hard_ceiling),
        }
    }
}

/// The outer bound on how much a task's [`ResourceBound`] may ever be
/// extended via renewal grants (HORO-1727). `Policy::renewal: None` —
/// the default — disables renewals entirely for a task; every existing
/// policy/task behaves exactly as before with zero config changes (see
/// [`Policy::renewal`]'s own field docs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RenewalBound {
    /// The absolute ceiling `effective_hard_limit` (the immutable
    /// `hard_ceiling` plus every granted renewal) may never exceed.
    /// Must share [`ResourceBound::hard_ceiling`]'s [`ResourceKind`] and
    /// be `>=` it — validated by [`Policy::validated`] (see
    /// [`PolicyValidationError::RenewalLifetimeCeilingKindMismatch`],
    /// [`PolicyValidationError::RenewalLifetimeCeilingBelowHardCeiling`]).
    pub lifetime_ceiling: ResourceAmount,
    /// The maximum number of renewal grants this task may ever receive,
    /// lifetime (not per-window).
    pub max_renewals: u32,
}

/// A turn-scoped resource ceiling, separate from and additive to the
/// task's own lifetime [`ResourceBound`] (HORO-1727 Decision B).
///
/// # Why a second, separate bound rather than reshaping the lifetime one
///
/// The lifetime `ResourceBound` enforces `used() + reserved() + requested
/// <= hard_limit` over a task's *whole life*, frozen into a reservation at
/// first preflight and separately re-enforced by the gateway (ADR-0003).
/// That promise is load-bearing for other callers and is explicitly out
/// of scope to weaken here. A single real Claude Code turn (one
/// Stop-to-Stop receipt window) can still blow through a sane *per-turn*
/// amount while the task's lifetime envelope has plenty of room left —
/// nothing in the lifetime check catches that, because it only ever sees
/// the running total. `TurnBound` is a second, independent gate checked
/// only at preflight against this one turn's own projected request, never
/// against committed lifetime spend.
///
/// # Fields
///
/// `cold_start_request` is what [`Policy::turn_fallback_request`] returns
/// in place of `resource.target` when no local estimate history exists
/// yet (mirrors `resource.target`'s own role as the policy's one
/// always-available resource figure). `ceiling` is the hard limit this
/// turn's own projected request may never exceed — see
/// [`Policy::evaluate_turn`]. There is deliberately no elastic band and no
/// `ApprovalRequired` outcome: a turn either fits under the ceiling or it
/// does not, because by the time a single turn is projected to exceed a
/// sane per-turn ceiling, the real problem (an unbounded loop, a runaway
/// tool-call chain) is not one more unit of pre-authorized spend would
/// fix — see `docs/adr/0021-turn-scoped-resource-ceiling.md` for why a
/// ceiling that reuses the per-class p80 estimate was rejected (it could
/// never produce a `Deny` at all).
///
/// Both amounts must share [`ResourceBound::target`]'s own
/// [`ResourceKind`] — validated by [`Policy::with_turn_bound`]
/// ([`PolicyValidationError::TurnColdStartKindMismatch`],
/// [`PolicyValidationError::TurnCeilingKindMismatch`]) — and `ceiling`
/// must be `>= cold_start_request`
/// ([`PolicyValidationError::TurnCeilingBelowColdStartRequest`]): a cold
/// start that could never itself be admitted would make every
/// never-before-seen task's very first preflight a guaranteed `Deny`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TurnBound {
    pub cold_start_request: ResourceAmount,
    pub ceiling: ResourceAmount,
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
    /// The outer bound on renewal grants against this task's
    /// [`ResourceBound`] (HORO-1727). `None` — the default on every
    /// existing policy, since `#[serde(default)]` fills it in for any
    /// `policy_json` serialized before this field existed — disables
    /// renewals entirely: `grant_renewal` refuses every request with
    /// [`crate::RenewalRefusal::RenewalsDisabled`], and every read site
    /// that applies [`ResourceBound::extended_by`] receives a granted
    /// amount of zero, which `extended_by` returns its input unchanged
    /// for. A task must opt in explicitly; nothing is renewable by
    /// default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renewal: Option<RenewalBound>,
    /// The optional turn-scoped ceiling (HORO-1727 Decision B) — see
    /// [`TurnBound`] docs for why this is a second, separate gate rather
    /// than a reshaping of [`Self::resource`]. `None` — the default, and
    /// the value every existing `Policy` gets via `#[serde(default)]` for
    /// any `policy_json` serialized before this field existed — disables
    /// the turn check entirely: [`Policy::evaluate_turn`] always returns
    /// [`ConstraintOutcome::Admit`] and [`Self::turn_fallback_request`]
    /// falls back to [`ResourceBound::target`], both byte-identical to
    /// this policy's pre-HORO-1727 behavior. Attach one via
    /// [`Self::with_turn_bound`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<TurnBound>,
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
    #[error("resource amount {amount:?} is out of QuotaPercent's documented 0.0..=100.0 domain")]
    ResourceQuotaPercentOutOfRange { amount: ResourceAmount },
    #[error(
        "renewal lifetime ceiling kind {lifetime_kind:?} does not match resource kind {resource_kind:?}"
    )]
    RenewalLifetimeCeilingKindMismatch {
        resource_kind: ResourceKind,
        lifetime_kind: ResourceKind,
    },
    #[error(
        "renewal lifetime ceiling {lifetime_ceiling:?} is below hard ceiling {hard_ceiling:?}"
    )]
    RenewalLifetimeCeilingBelowHardCeiling {
        lifetime_ceiling: ResourceAmount,
        hard_ceiling: ResourceAmount,
    },
    /// `QuotaPercent`'s `0.0..=100.0` domain saturates under
    /// [`ResourceBound::extended_by`]'s addition (see
    /// [`ResourceAmount::from_kind_f64`]), while the ledger's raw `REAL`
    /// `effective_hard_limit` does not — a renewal would silently
    /// desynchronize the policy bound from the ledger's own accounting.
    /// Renewals are refused outright for this kind rather than built on
    /// an arithmetic mismatch.
    #[error(
        "renewal is not supported for resource kind {kind:?} (QuotaPercent saturates at 100.0)"
    )]
    RenewalUnsupportedForResourceKind { kind: ResourceKind },
    /// (HORO-1727 Decision B.) `TurnBound::cold_start_request`'s
    /// [`ResourceKind`] does not match the policy's own resource kind.
    #[error(
        "turn bound cold_start_request kind {cold_start_kind:?} does not match resource kind \
         {resource_kind:?}"
    )]
    TurnColdStartKindMismatch {
        resource_kind: ResourceKind,
        cold_start_kind: ResourceKind,
    },
    /// (HORO-1727 Decision B.) `TurnBound::ceiling`'s [`ResourceKind`]
    /// does not match the policy's own resource kind.
    #[error(
        "turn bound ceiling kind {ceiling_kind:?} does not match resource kind {resource_kind:?}"
    )]
    TurnCeilingKindMismatch {
        resource_kind: ResourceKind,
        ceiling_kind: ResourceKind,
    },
    /// (HORO-1727 Decision B.) A `ceiling` below `cold_start_request`
    /// would make a never-before-seen task's very first preflight a
    /// guaranteed `Deny` — see [`TurnBound`] docs.
    #[error(
        "turn bound ceiling {ceiling:?} is below its own cold_start_request {cold_start_request:?}"
    )]
    TurnCeilingBelowColdStartRequest {
        cold_start_request: ResourceAmount,
        ceiling: ResourceAmount,
    },
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
            renewal: None,
            turn: None,
        })
    }

    /// Attaches a [`RenewalBound`] to an already-validated policy
    /// (HORO-1727), opt-in and separate from [`Self::validated`]/
    /// [`Self::validated_at`] so every existing construction call site
    /// (including every preset) is unaffected and keeps `renewal: None`
    /// unless it explicitly calls this.
    pub fn with_renewal(mut self, renewal: RenewalBound) -> Result<Self, PolicyValidationError> {
        Self::validate_renewal(&self.resource, &renewal)?;
        self.renewal = Some(renewal);
        Ok(self)
    }

    /// Validates a candidate [`RenewalBound`] against the policy's own
    /// [`ResourceBound`]: same [`ResourceKind`] as `hard_ceiling`, a
    /// `lifetime_ceiling` no smaller than `hard_ceiling`, and never
    /// `QuotaPercent` (see
    /// [`PolicyValidationError::RenewalUnsupportedForResourceKind`]'s
    /// docs for why).
    fn validate_renewal(
        resource: &ResourceBound,
        renewal: &RenewalBound,
    ) -> Result<(), PolicyValidationError> {
        let resource_kind = resource.hard_ceiling.kind();
        if resource_kind == ResourceKind::QuotaPercent {
            return Err(PolicyValidationError::RenewalUnsupportedForResourceKind {
                kind: resource_kind,
            });
        }
        let lifetime_kind = renewal.lifetime_ceiling.kind();
        if lifetime_kind != resource_kind {
            return Err(PolicyValidationError::RenewalLifetimeCeilingKindMismatch {
                resource_kind,
                lifetime_kind,
            });
        }
        if renewal.lifetime_ceiling.as_f64() < resource.hard_ceiling.as_f64() {
            return Err(
                PolicyValidationError::RenewalLifetimeCeilingBelowHardCeiling {
                    lifetime_ceiling: renewal.lifetime_ceiling,
                    hard_ceiling: resource.hard_ceiling,
                },
            );
        }
        Ok(())
    }

    /// Attaches a [`TurnBound`] to an already-validated policy (HORO-1727
    /// Decision B), opt-in and separate from [`Self::validated`]/
    /// [`Self::validated_at`] so every existing construction call site
    /// (including every preset) is unaffected and keeps `turn: None`
    /// unless it explicitly calls this.
    pub fn with_turn_bound(mut self, turn: TurnBound) -> Result<Self, PolicyValidationError> {
        Self::validate_turn(&self.resource, &turn)?;
        self.turn = Some(turn);
        Ok(self)
    }

    /// Validates a candidate [`TurnBound`] against the policy's own
    /// [`ResourceBound`]: both of `turn`'s amounts share
    /// [`ResourceBound::target`]'s [`ResourceKind`], and `ceiling >=
    /// cold_start_request`.
    fn validate_turn(
        resource: &ResourceBound,
        turn: &TurnBound,
    ) -> Result<(), PolicyValidationError> {
        let resource_kind = resource.target.kind();
        let cold_start_kind = turn.cold_start_request.kind();
        if cold_start_kind != resource_kind {
            return Err(PolicyValidationError::TurnColdStartKindMismatch {
                resource_kind,
                cold_start_kind,
            });
        }
        let ceiling_kind = turn.ceiling.kind();
        if ceiling_kind != resource_kind {
            return Err(PolicyValidationError::TurnCeilingKindMismatch {
                resource_kind,
                ceiling_kind,
            });
        }
        if turn.ceiling.as_f64() < turn.cold_start_request.as_f64() {
            return Err(PolicyValidationError::TurnCeilingBelowColdStartRequest {
                cold_start_request: turn.cold_start_request,
                ceiling: turn.ceiling,
            });
        }
        Ok(())
    }

    /// The per-turn amount to request in place of a real estimate quantile
    /// when none is available yet (HORO-1727 Decision B) — the turn-scoped
    /// analogue of [`ResourceBound::target`]'s own "always-available
    /// figure" role. Returns [`TurnBound::cold_start_request`] when a turn
    /// bound is configured, else falls back to [`ResourceBound::target`] —
    /// byte-identical to every call site's pre-HORO-1727 behavior when
    /// `self.turn` is `None`.
    pub fn turn_fallback_request(&self) -> ResourceAmount {
        self.turn
            .map(|t| t.cold_start_request)
            .unwrap_or(self.resource.target)
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

        Self::validate_quota_percent_range(resource.target)?;
        Self::validate_quota_percent_range(resource.hard_ceiling)?;
        if let Some(elastic) = resource.elastic_ceiling {
            Self::validate_quota_percent_range(elastic)?;
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

    /// Rejects a [`ResourceAmount::QuotaPercent`] outside its own
    /// documented `0.0..=100.0` domain. A no-op for `UsdCents`/`Tokens`,
    /// which have no such bound.
    fn validate_quota_percent_range(amount: ResourceAmount) -> Result<(), PolicyValidationError> {
        if let ResourceAmount::QuotaPercent(p) = amount {
            if !(0.0..=100.0).contains(&p) {
                return Err(PolicyValidationError::ResourceQuotaPercentOutOfRange { amount });
            }
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
    /// The policy's `resource` bound is in [`ConstraintMode::Elastic`]
    /// but carries no `elastic_ceiling`. [`Policy::validated`] never
    /// produces this — it can only happen if a `Policy` was built some
    /// other way (a struct literal, or deserialized from untrusted/
    /// hand-edited data) that bypassed validation. Returned rather than
    /// panicking, since a stored `Policy` read back via `Deserialize` is
    /// exactly the kind of "untrusted until checked" input this crate's
    /// error types exist for.
    #[error("resource bound is in Elastic mode but carries no elastic ceiling (policy did not go through Policy::validated)")]
    InconsistentResourceBound,
    /// The policy's `time` bound is in a mode that requires a ceiling
    /// (`Hard` needs `hard_ceiling_secs`, `Elastic` needs
    /// `elastic_ceiling_secs`) that isn't set. Same non-panicking
    /// rationale as [`Self::InconsistentResourceBound`].
    #[error("time bound is in {mode:?} mode but is missing the ceiling that mode requires (policy did not go through Policy::validated)")]
    InconsistentTimeBound { mode: ConstraintMode },
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
    /// An external Policy Webhook provider rejected an
    /// `ApprovalRequired` admission (HORO-1174). Additive variant — see
    /// `crate::external_approval::apply_external_approval` and its
    /// `deny_reason_deserializes_the_old_variants_after_the_additive_change`
    /// regression test, which pins that old serialized `admission_json`
    /// rows (predating this variant) still deserialize.
    ExternalPolicyRejected { provider_id: String, reason: String },
    /// This turn's own projected request exceeded the policy's
    /// [`TurnBound::ceiling`] (HORO-1727 Decision B). Additive variant,
    /// same deserialization-compatibility discipline as
    /// [`Self::ExternalPolicyRejected`] — see
    /// `resource_exceeds_turn_ceiling_is_additive_and_old_deny_reason_json_still_deserializes`.
    ResourceExceedsTurnCeiling {
        projected: ResourceAmount,
        turn_ceiling: ResourceAmount,
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
    /// The outcome of the turn-scoped ceiling check (HORO-1727 Decision
    /// B), if any — `None` when the policy carries no [`TurnBound`]
    /// (`Policy::turn: None`), which is every policy before this field
    /// existed and every policy that never opts in via
    /// [`Policy::with_turn_bound`]. `#[serde(default)]` so a
    /// `PolicyDecision` serialized before this field existed still
    /// deserializes. Folded into [`Self::admission`] by
    /// [`Self::with_turn_outcome`] — this field is the raw per-constraint
    /// outcome (mirroring [`Self::resource_outcome`]/
    /// [`Self::time_outcome`]), kept for rendering/explain surfaces that
    /// want to show the turn check on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_outcome: Option<ConstraintOutcome>,
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

        let resource_outcome = Self::evaluate_resource(&self.resource, projected_resource)?;
        let time_outcome = Self::evaluate_time(&self.time, projected_duration_secs)?;
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
            turn_outcome: None,
        })
    }

    /// Evaluates the resource constraint alone, independent of time or
    /// confidence (HORO-1670). Pure visibility change from the original
    /// `fn evaluate_resource` — no behavior change. Exists so counterfactual
    /// policy replay can compose per-dimension verdicts using this
    /// already-tested logic without duplicating it, and without being
    /// forced to call [`Self::evaluate`] (which demands both a resource
    /// *and* a time projection, and would force a forbidden fabricated
    /// zero when only one dimension has replay evidence).
    pub fn evaluate_resource(
        bound: &ResourceBound,
        projected: ResourceAmount,
    ) -> Result<ConstraintOutcome, PolicyEvaluationError> {
        let p = projected.as_f64();
        let target = bound.target.as_f64();
        let hard = bound.hard_ceiling.as_f64();

        let outcome = match bound.mode {
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
                    .ok_or(PolicyEvaluationError::InconsistentResourceBound)?;
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
        };
        Ok(outcome)
    }

    /// Evaluates the time constraint alone. See [`Self::evaluate_resource`]
    /// docs — same pure-visibility-change rationale (HORO-1670).
    pub fn evaluate_time(
        bound: &TimeBound,
        projected_secs: u64,
    ) -> Result<ConstraintOutcome, PolicyEvaluationError> {
        let outcome = match bound.mode {
            ConstraintMode::Hard => {
                let hard = bound.hard_ceiling_secs.ok_or(
                    PolicyEvaluationError::InconsistentTimeBound {
                        mode: ConstraintMode::Hard,
                    },
                )?;
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
                let elastic = bound.elastic_ceiling_secs.ok_or(
                    PolicyEvaluationError::InconsistentTimeBound {
                        mode: ConstraintMode::Elastic,
                    },
                )?;
                if projected_secs <= bound.target_secs || projected_secs <= elastic {
                    ConstraintOutcome::Admit
                } else if let Some(hard_ceiling_secs) = bound.hard_ceiling_secs {
                    if projected_secs <= hard_ceiling_secs {
                        ConstraintOutcome::ApprovalRequired(ApprovalRequest::Time {
                            projected_secs,
                            target_secs: bound.target_secs,
                            elastic_ceiling_secs: Some(elastic),
                            hard_ceiling_secs: bound.hard_ceiling_secs,
                        })
                    } else {
                        ConstraintOutcome::Deny(DenyReason::TimeExceedsHardCeiling {
                            projected_secs,
                            hard_ceiling_secs,
                        })
                    }
                } else {
                    ConstraintOutcome::ApprovalRequired(ApprovalRequest::Time {
                        projected_secs,
                        target_secs: bound.target_secs,
                        elastic_ceiling_secs: Some(elastic),
                        hard_ceiling_secs: None,
                    })
                }
            }
            ConstraintMode::Approval => {
                if projected_secs <= bound.target_secs {
                    ConstraintOutcome::Admit
                } else if let Some(hard_ceiling_secs) = bound.hard_ceiling_secs {
                    if projected_secs <= hard_ceiling_secs {
                        ConstraintOutcome::ApprovalRequired(ApprovalRequest::Time {
                            projected_secs,
                            target_secs: bound.target_secs,
                            elastic_ceiling_secs: None,
                            hard_ceiling_secs: bound.hard_ceiling_secs,
                        })
                    } else {
                        ConstraintOutcome::Deny(DenyReason::TimeExceedsHardCeiling {
                            projected_secs,
                            hard_ceiling_secs,
                        })
                    }
                } else {
                    ConstraintOutcome::ApprovalRequired(ApprovalRequest::Time {
                        projected_secs,
                        target_secs: bound.target_secs,
                        elastic_ceiling_secs: None,
                        hard_ceiling_secs: None,
                    })
                }
            }
        };
        Ok(outcome)
    }

    /// Evaluates the turn-scoped ceiling alone (HORO-1727 Decision B),
    /// independent of the lifetime resource/time/confidence check —
    /// `projected_turn` must be THIS turn's own projected request, never
    /// committed lifetime spend (see `crates/daemon/src/server.rs`'s
    /// `requested` local — not `projected`, which adds committed spend on
    /// top).
    ///
    /// `Admit` when no [`TurnBound`] is configured at all (`self.turn ==
    /// None`) — the turn check is fully opt-in. There is no elastic band
    /// and no [`ConstraintOutcome::ApprovalRequired`] here — see
    /// [`TurnBound`] docs for why.
    pub fn evaluate_turn(
        &self,
        projected_turn: ResourceAmount,
    ) -> Result<ConstraintOutcome, PolicyEvaluationError> {
        let Some(turn) = self.turn else {
            return Ok(ConstraintOutcome::Admit);
        };
        let turn_kind = turn.ceiling.kind();
        if projected_turn.kind() != turn_kind {
            return Err(PolicyEvaluationError::ProjectedResourceKindMismatch {
                policy_kind: turn_kind,
                projected_kind: projected_turn.kind(),
            });
        }
        if projected_turn.as_f64() > turn.ceiling.as_f64() {
            Ok(ConstraintOutcome::Deny(
                DenyReason::ResourceExceedsTurnCeiling {
                    projected: projected_turn,
                    turn_ceiling: turn.ceiling,
                },
            ))
        } else {
            Ok(ConstraintOutcome::Admit)
        }
    }
}

impl PolicyDecision {
    /// Folds a turn-scoped [`ConstraintOutcome`] (from
    /// [`Policy::evaluate_turn`]) into this already-computed decision
    /// (HORO-1727 Decision B).
    ///
    /// A turn `Deny` always wins: it turns an `Admit` or
    /// `ApprovalRequired` admission into `Deny`, and appends its reason
    /// onto an existing `Deny` rather than replacing it (so the lifetime
    /// check's own reasons are never silently dropped). A turn `Admit`
    /// leaves `self.admission` completely untouched — it can never
    /// upgrade an existing `ApprovalRequired`/`Deny` into `Admit`. There is
    /// no `ApprovalRequired` turn outcome to fold (see [`TurnBound`] docs).
    /// [`Self::turn_outcome`] is set to `Some(outcome)` unconditionally, so
    /// a rendering surface can always show what the turn check itself
    /// decided, independent of how it was folded.
    pub fn with_turn_outcome(mut self, outcome: ConstraintOutcome) -> Self {
        if let ConstraintOutcome::Deny(ref reason) = outcome {
            match &mut self.admission {
                Admission::Deny(reasons) => reasons.push(reason.clone()),
                Admission::Admit | Admission::ApprovalRequired(_) => {
                    self.admission = Admission::Deny(vec![reason.clone()]);
                }
            }
        }
        self.turn_outcome = Some(outcome);
        self
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
    /// elastic band (target..=1.25x, hard ceiling at 1.5x), ask before
    /// crossing into either band's approval-required zone.
    ///
    /// `min_confidence` is [`Confidence::Low`] — not [`Confidence::Medium`]
    /// like [`Self::deadline_first`]/[`Self::strict_budget`] — deliberately
    /// (HORO-1146 gate finding #1). `balanced` is the shipped *default*
    /// policy every fresh install and every never-before-seen
    /// repo/prompt combination is admitted under
    /// (`crates/daemon::default_admission_policy`), and a cold-start
    /// estimate is always [`Confidence::Low`] (no local receipt history
    /// yet — see [`crate::confidence::Confidence::from_evidence`]).
    /// Requiring `Medium` here meant every first-ever preflight denied
    /// outright on the confidence floor alone, before any other
    /// constraint was even considered — not a fabricated confidence
    /// bump, just admitting that a resource/time band this elastic does
    /// not need a `Medium`-confidence estimate to pre-authorize
    /// spending into. A policy whose bands are tighter (`deadline_first`,
    /// `strict_budget`) still requires real evidence before admitting.
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
            Confidence::Low,
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

/// The human-readable preset names [`preset_by_name`] accepts — the
/// single source of truth for "which named presets exist" (HORO-1670).
/// `crates/daemon::config_file::resolve_policy` and counterfactual policy
/// replay both resolve a preset name through this list rather than each
/// keeping their own copy of the match arms, so the two can never
/// silently drift apart.
pub const NAMED_PRESETS: [&str; 4] = ["balanced", "deadline_first", "cost_first", "strict_budget"];

/// Why [`preset_by_name`] could not produce a [`Policy`] — distinct from
/// [`PolicyValidationError`] (a known preset name whose compiled-out
/// `Policy` was still somehow invalid, which the four shipped presets
/// never produce in practice) versus an unrecognized name entirely.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PresetError {
    #[error("unknown policy preset {name:?} (known presets: {known:?})")]
    UnknownPreset {
        name: String,
        known: &'static [&'static str],
    },
    #[error(transparent)]
    Invalid(#[from] PolicyValidationError),
}

/// Compiles a named preset ([`NAMED_PRESETS`]) against task-specific
/// [`PolicyPresetInputs`] — the single source of truth both the daemon's
/// own config resolution and counterfactual policy replay call, so a
/// recorded policy can always be reproduced from `(preset name, inputs)`
/// without a second, independently-maintained match arm (HORO-1670).
pub fn preset_by_name(name: &str, inputs: PolicyPresetInputs) -> Result<Policy, PresetError> {
    let policy = match name {
        "balanced" => Policy::balanced(inputs),
        "deadline_first" => Policy::deadline_first(inputs),
        "cost_first" => Policy::cost_first(inputs),
        "strict_budget" => Policy::strict_budget(inputs),
        other => {
            return Err(PresetError::UnknownPreset {
                name: other.to_string(),
                known: &NAMED_PRESETS,
            })
        }
    };
    Ok(policy?)
}

impl PolicyPresetInputs {
    /// Recovers the task-specific inputs a recorded [`Policy`] was
    /// compiled from (HORO-1670) — the inverse of [`preset_by_name`].
    /// Exact for all four shipped presets: each passes `resource_target`
    /// straight through to [`ResourceBound::target`] and
    /// `time_target_secs` straight through to [`TimeBound::target_secs`]
    /// with no transformation (see each preset's doc comment) — recovering
    /// a counterfactual's inputs from a historical policy is therefore
    /// lossless, never a reconstruction/approximation.
    pub fn from_recorded_policy(policy: &Policy) -> Self {
        PolicyPresetInputs {
            resource_target: policy.resource.target,
            time_target_secs: policy.time.target_secs,
            quality_floor: policy.quality_floor.clone(),
        }
    }
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
                min_confidence: Confidence::Low,
                autonomy: AutonomyBoundary::AskOnApproval,
                renewal: None,
                turn: None,
            },
            "balanced must compile to exactly this concrete Policy value, no hidden logic"
        );
    }

    #[test]
    fn balanced_admits_a_cold_start_low_confidence_estimate_on_confidence_alone() {
        // HORO-1146 gate finding #1 regression: a brand-new install (or
        // any never-before-seen repo/prompt combination) always produces
        // a cold-start `Confidence::Low` estimate. Under the real,
        // shipped `balanced` preset, that must not deny admission solely
        // because of the confidence floor — it may still legitimately
        // deny for a real reason (e.g. the projection exceeds the hard
        // ceiling), just never for confidence alone.
        let policy = Policy::balanced(preset_inputs()).expect("valid policy");
        let inputs = preset_inputs();

        let decision = policy
            .evaluate(
                inputs.resource_target,
                inputs.time_target_secs,
                Confidence::Low,
            )
            .expect("compatible kind");

        assert!(
            decision.confidence_ok,
            "cold-start Low confidence must satisfy balanced's confidence floor"
        );
        assert_eq!(
            decision.admission,
            Admission::Admit,
            "an on-target, cold-start-confidence projection must cleanly admit under balanced"
        );

        // A genuinely over-budget cold-start projection must still deny
        // — just for the real resource reason, never
        // `ConfidenceBelowThreshold`.
        let over_hard_ceiling = policy.resource.hard_ceiling.scaled(2.0);
        let denied = policy
            .evaluate(over_hard_ceiling, inputs.time_target_secs, Confidence::Low)
            .expect("compatible kind");
        match denied.admission {
            Admission::Deny(reasons) => assert!(
                reasons
                    .iter()
                    .all(|r| !matches!(r, DenyReason::ConfidenceBelowThreshold { .. })),
                "a real budget-exhaustion deny must not also cite confidence: {reasons:?}"
            ),
            other => panic!("expected Deny for a projection past the hard ceiling, got {other:?}"),
        }
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

    #[test]
    fn tokens_resource_preset_and_evaluate_round_trip() {
        // Subscription/quota-metered resources must be representable
        // without a fake USD conversion — Tokens is one such kind.
        let inputs = PolicyPresetInputs {
            resource_target: ResourceAmount::Tokens(10_000),
            time_target_secs: 600,
            quality_floor: quality_floor(),
        };
        let policy = Policy::balanced(inputs).expect("valid policy");
        assert_eq!(policy.resource.target, ResourceAmount::Tokens(10_000));
        assert_eq!(
            policy.resource.elastic_ceiling,
            Some(ResourceAmount::Tokens(12_500))
        );
        assert_eq!(policy.resource.hard_ceiling, ResourceAmount::Tokens(15_000));

        let decision = policy
            .evaluate(ResourceAmount::Tokens(11_000), 600, Confidence::Medium)
            .expect("compatible kind");
        assert_eq!(decision.admission, Admission::Admit);

        let over_hard = policy
            .evaluate(ResourceAmount::Tokens(20_000), 600, Confidence::Medium)
            .expect("compatible kind");
        assert!(matches!(over_hard.admission, Admission::Deny(_)));
    }

    #[test]
    fn quota_percent_resource_preset_and_evaluate_round_trip() {
        // A subscription plan that only exposes a quota percentage, with
        // no per-request price at all, must also be representable.
        let inputs = PolicyPresetInputs {
            resource_target: ResourceAmount::QuotaPercent(40.0),
            time_target_secs: 600,
            quality_floor: quality_floor(),
        };
        let policy = Policy::balanced(inputs).expect("valid policy");
        assert_eq!(policy.resource.target, ResourceAmount::QuotaPercent(40.0));
        assert_eq!(
            policy.resource.elastic_ceiling,
            Some(ResourceAmount::QuotaPercent(50.0))
        );
        assert_eq!(
            policy.resource.hard_ceiling,
            ResourceAmount::QuotaPercent(60.0)
        );

        let decision = policy
            .evaluate(ResourceAmount::QuotaPercent(45.0), 600, Confidence::Medium)
            .expect("compatible kind");
        assert_eq!(decision.admission, Admission::Admit);
    }

    #[test]
    fn quota_percent_preset_saturates_at_100_instead_of_producing_an_out_of_range_ceiling() {
        // deadline_first's 3x multiplier on an 80% target would be 240%
        // without saturation — not representable, and not rejected as
        // invalid either, since the saturated value is exactly what the
        // policy means ("as much of the quota as exists").
        let inputs = PolicyPresetInputs {
            resource_target: ResourceAmount::QuotaPercent(80.0),
            time_target_secs: 600,
            quality_floor: quality_floor(),
        };
        let policy = Policy::deadline_first(inputs).expect("valid policy");
        assert_eq!(
            policy.resource.hard_ceiling,
            ResourceAmount::QuotaPercent(100.0)
        );
    }

    #[test]
    fn rejects_quota_percent_out_of_range() {
        let err = Policy::validated(
            "invalid",
            ResourceBound {
                mode: ConstraintMode::Hard,
                target: ResourceAmount::QuotaPercent(40.0),
                elastic_ceiling: None,
                hard_ceiling: ResourceAmount::QuotaPercent(150.0),
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
            PolicyValidationError::ResourceQuotaPercentOutOfRange {
                amount: ResourceAmount::QuotaPercent(150.0),
            }
        );
    }

    #[test]
    fn evaluate_returns_an_error_rather_than_panicking_on_a_policy_that_bypassed_validation() {
        // Policy is `Deserialize`, and every field is `pub` — a stored
        // (or hand-edited) Policy can reach evaluate() without ever
        // going through Policy::validated. evaluate() must fail safely,
        // not panic, on an internally-inconsistent policy like this one
        // (Elastic mode, no elastic_ceiling).
        let json = serde_json::json!({
            "policy_schema_version": POLICY_SCHEMA_VERSION,
            "name": "hand-edited",
            "resource": {
                "mode": "elastic",
                "target": {"kind": "usd_cents", "amount": 1000},
                "elastic_ceiling": null,
                "hard_ceiling": {"kind": "usd_cents", "amount": 2000}
            },
            "time": {
                "mode": "hard",
                "target_secs": 600,
                "elastic_ceiling_secs": null,
                "hard_ceiling_secs": null,
                "deadline": null
            },
            "quality_floor": {
                "revision": 1,
                "criteria": [{"description": "tests pass", "required": true}]
            },
            "min_confidence": "medium",
            "autonomy": "ask_on_approval"
        });
        let policy: Policy = serde_json::from_value(json).expect("deserializes structurally");

        let resource_err = policy
            .evaluate(ResourceAmount::UsdCents(1), 1, Confidence::Medium)
            .unwrap_err();
        assert_eq!(
            resource_err,
            PolicyEvaluationError::InconsistentResourceBound
        );
    }

    #[test]
    fn evaluate_returns_an_error_for_a_hard_time_bound_missing_its_ceiling() {
        let json = serde_json::json!({
            "policy_schema_version": POLICY_SCHEMA_VERSION,
            "name": "hand-edited",
            "resource": {
                "mode": "hard",
                "target": {"kind": "usd_cents", "amount": 1000},
                "elastic_ceiling": null,
                "hard_ceiling": {"kind": "usd_cents", "amount": 1000}
            },
            "time": {
                "mode": "hard",
                "target_secs": 600,
                "elastic_ceiling_secs": null,
                "hard_ceiling_secs": null,
                "deadline": null
            },
            "quality_floor": {
                "revision": 1,
                "criteria": [{"description": "tests pass", "required": true}]
            },
            "min_confidence": "medium",
            "autonomy": "ask_on_approval"
        });
        let policy: Policy = serde_json::from_value(json).expect("deserializes structurally");

        let time_err = policy
            .evaluate(ResourceAmount::UsdCents(1), 1, Confidence::Medium)
            .unwrap_err();
        assert_eq!(
            time_err,
            PolicyEvaluationError::InconsistentTimeBound {
                mode: ConstraintMode::Hard
            }
        );
    }

    // -- HORO-1727: bounded renewal mechanism -----------------------------

    #[test]
    fn extended_by_zero_is_a_byte_identical_no_op() {
        let bound = elastic_resource_policy().resource;
        let extended = bound.extended_by(ResourceAmount::UsdCents(0));
        assert_eq!(
            extended, bound,
            "extended_by(0) must return the input unchanged, not merely numerically equal"
        );
    }

    #[test]
    fn extended_by_zero_amount_different_kind_is_still_a_no_op() {
        // A granted amount of zero tokens against a USD-denominated bound:
        // the kind mismatch guard and the zero-amount guard must agree.
        let bound = elastic_resource_policy().resource;
        let extended = bound.extended_by(ResourceAmount::Tokens(0));
        assert_eq!(extended, bound);
    }

    #[test]
    fn extended_by_shifts_target_elastic_and_hard_ceiling_together() {
        let bound = elastic_resource_policy().resource;
        let extended = bound.extended_by(ResourceAmount::UsdCents(500));
        assert_eq!(extended.target, ResourceAmount::UsdCents(1500));
        assert_eq!(
            extended.elastic_ceiling,
            Some(ResourceAmount::UsdCents(2000))
        );
        assert_eq!(extended.hard_ceiling, ResourceAmount::UsdCents(2500));
        assert_eq!(extended.mode, bound.mode);
    }

    #[test]
    fn extended_by_mismatched_kind_is_a_no_op() {
        let bound = elastic_resource_policy().resource;
        let extended = bound.extended_by(ResourceAmount::Tokens(500));
        assert_eq!(extended, bound);
    }

    #[test]
    fn policy_json_with_no_renewal_set_serializes_with_no_renewal_key() {
        let policy = hard_resource_policy();
        assert!(policy.renewal.is_none());
        let json = serde_json::to_value(&policy).expect("serializes");
        assert!(
            json.get("renewal").is_none(),
            "a policy with no renewal bound must not serialize a `renewal` key at all \
             (policy_json byte-identity for every existing config): got {json}"
        );
    }

    #[test]
    fn with_renewal_validates_lifetime_ceiling_kind_matches_resource() {
        let policy = hard_resource_policy();
        let err = policy
            .with_renewal(RenewalBound {
                lifetime_ceiling: ResourceAmount::Tokens(10_000),
                max_renewals: 3,
            })
            .unwrap_err();
        assert_eq!(
            err,
            PolicyValidationError::RenewalLifetimeCeilingKindMismatch {
                resource_kind: ResourceKind::Usd,
                lifetime_kind: ResourceKind::Tokens,
            }
        );
    }

    #[test]
    fn with_renewal_validates_lifetime_ceiling_at_least_hard_ceiling() {
        let policy = hard_resource_policy();
        let err = policy
            .with_renewal(RenewalBound {
                lifetime_ceiling: ResourceAmount::UsdCents(500),
                max_renewals: 3,
            })
            .unwrap_err();
        assert_eq!(
            err,
            PolicyValidationError::RenewalLifetimeCeilingBelowHardCeiling {
                lifetime_ceiling: ResourceAmount::UsdCents(500),
                hard_ceiling: ResourceAmount::UsdCents(1000),
            }
        );
    }

    #[test]
    fn with_renewal_rejects_quota_percent() {
        let policy = Policy::validated(
            "test-quota-percent",
            ResourceBound {
                mode: ConstraintMode::Hard,
                target: ResourceAmount::QuotaPercent(50.0),
                elastic_ceiling: None,
                hard_ceiling: ResourceAmount::QuotaPercent(100.0),
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
        .expect("valid policy");
        let err = policy
            .with_renewal(RenewalBound {
                lifetime_ceiling: ResourceAmount::QuotaPercent(100.0),
                max_renewals: 3,
            })
            .unwrap_err();
        assert_eq!(
            err,
            PolicyValidationError::RenewalUnsupportedForResourceKind {
                kind: ResourceKind::QuotaPercent,
            }
        );
    }

    #[test]
    fn with_renewal_accepts_a_valid_bound() {
        let policy = hard_resource_policy()
            .with_renewal(RenewalBound {
                lifetime_ceiling: ResourceAmount::UsdCents(5_000),
                max_renewals: 3,
            })
            .expect("valid renewal bound");
        assert_eq!(
            policy.renewal,
            Some(RenewalBound {
                lifetime_ceiling: ResourceAmount::UsdCents(5_000),
                max_renewals: 3,
            })
        );
    }

    // -- HORO-1727 Decision B: turn-scoped resource ceiling --------------

    /// A realistic turn-scoped policy, at the actual magnitude the real
    /// default ships at (not a token-sized toy): cold-start request
    /// 120,000, ceiling 400,000, on top of a resource target large enough
    /// that the lifetime check never interferes with what these tests are
    /// about.
    fn turn_scoped_policy() -> Policy {
        Policy::validated(
            "test-turn-scoped",
            ResourceBound {
                mode: ConstraintMode::Hard,
                target: ResourceAmount::Tokens(1_000_000),
                elastic_ceiling: None,
                hard_ceiling: ResourceAmount::Tokens(1_000_000),
            },
            TimeBound {
                mode: ConstraintMode::Hard,
                target_secs: 3600,
                elastic_ceiling_secs: None,
                hard_ceiling_secs: Some(3600),
                deadline: None,
            },
            quality_floor(),
            Confidence::Low,
            AutonomyBoundary::AskOnApproval,
        )
        .expect("valid policy")
        .with_turn_bound(TurnBound {
            cold_start_request: ResourceAmount::Tokens(120_000),
            ceiling: ResourceAmount::Tokens(400_000),
        })
        .expect("valid turn bound")
    }

    #[test]
    fn no_turn_bound_configured_evaluate_turn_always_admits() {
        // `Policy::turn: None` — every policy before this ticket, and
        // every policy that never opts in — must behave exactly as
        // before: the turn check is fully opt-in.
        let policy = hard_resource_policy();
        assert!(policy.turn.is_none());
        let outcome = policy
            .evaluate_turn(ResourceAmount::UsdCents(1_000_000_000))
            .expect("compatible kind");
        assert_eq!(outcome, ConstraintOutcome::Admit);
    }

    #[test]
    fn no_turn_bound_configured_turn_fallback_request_is_resource_target() {
        let policy = hard_resource_policy();
        assert_eq!(policy.turn_fallback_request(), policy.resource.target);
    }

    #[test]
    fn turn_fallback_request_prefers_cold_start_request_when_configured() {
        let policy = turn_scoped_policy();
        assert_eq!(
            policy.turn_fallback_request(),
            ResourceAmount::Tokens(120_000)
        );
        assert_ne!(policy.turn_fallback_request(), policy.resource.target);
    }

    /// AC5 (cold-start): with no estimate history, a fresh task's
    /// `requested` figure is `turn_fallback_request()` —
    /// `cold_start_request` — and that figure admits under the turn
    /// ceiling.
    #[test]
    fn ac5_cold_start_request_admits_under_the_turn_ceiling() {
        let policy = turn_scoped_policy();
        let cold_start_requested = policy.turn_fallback_request();
        let outcome = policy
            .evaluate_turn(cold_start_requested)
            .expect("compatible kind");
        assert_eq!(
            outcome,
            ConstraintOutcome::Admit,
            "a cold-start task's very first preflight must admit under the turn ceiling"
        );
    }

    /// AC3: a task whose measured per-turn history sits at the documented
    /// p80 (120,700 fresh tokens, per docs/adr/0021) is admitted.
    #[test]
    fn ac3_a_measured_p80_projection_admits_under_the_turn_ceiling() {
        let policy = turn_scoped_policy();
        let measured_p80 = ResourceAmount::Tokens(120_700);
        let outcome = policy.evaluate_turn(measured_p80).expect("compatible kind");
        assert_eq!(outcome, ConstraintOutcome::Admit);
    }

    /// AC4: a projection that genuinely exceeds the ceiling denies, with
    /// an exact boundary: the ceiling itself still admits, one unit past
    /// it denies.
    #[test]
    fn ac4_boundary_exactly_at_ceiling_admits_one_past_denies() {
        let policy = turn_scoped_policy();

        let at_ceiling = policy
            .evaluate_turn(ResourceAmount::Tokens(400_000))
            .expect("compatible kind");
        assert_eq!(
            at_ceiling,
            ConstraintOutcome::Admit,
            "the ceiling itself is still admitted, not denied"
        );

        let one_past = policy
            .evaluate_turn(ResourceAmount::Tokens(400_001))
            .expect("compatible kind");
        assert_eq!(
            one_past,
            ConstraintOutcome::Deny(DenyReason::ResourceExceedsTurnCeiling {
                projected: ResourceAmount::Tokens(400_001),
                turn_ceiling: ResourceAmount::Tokens(400_000),
            })
        );

        let far_past = policy
            .evaluate_turn(ResourceAmount::Tokens(5_000_000))
            .expect("compatible kind");
        assert!(matches!(
            far_past,
            ConstraintOutcome::Deny(DenyReason::ResourceExceedsTurnCeiling { .. })
        ));
    }

    #[test]
    fn evaluate_turn_rejects_mismatched_resource_kind() {
        let policy = turn_scoped_policy();
        let err = policy
            .evaluate_turn(ResourceAmount::UsdCents(1))
            .unwrap_err();
        assert_eq!(
            err,
            PolicyEvaluationError::ProjectedResourceKindMismatch {
                policy_kind: ResourceKind::Tokens,
                projected_kind: ResourceKind::Usd,
            }
        );
    }

    #[test]
    fn with_turn_bound_rejects_cold_start_kind_mismatch() {
        let policy = hard_resource_policy();
        let err = policy
            .with_turn_bound(TurnBound {
                cold_start_request: ResourceAmount::Tokens(1_000),
                ceiling: ResourceAmount::UsdCents(2_000),
            })
            .unwrap_err();
        assert_eq!(
            err,
            PolicyValidationError::TurnColdStartKindMismatch {
                resource_kind: ResourceKind::Usd,
                cold_start_kind: ResourceKind::Tokens,
            }
        );
    }

    #[test]
    fn with_turn_bound_rejects_ceiling_kind_mismatch() {
        let policy = hard_resource_policy();
        let err = policy
            .with_turn_bound(TurnBound {
                cold_start_request: ResourceAmount::UsdCents(500),
                ceiling: ResourceAmount::Tokens(2_000),
            })
            .unwrap_err();
        assert_eq!(
            err,
            PolicyValidationError::TurnCeilingKindMismatch {
                resource_kind: ResourceKind::Usd,
                ceiling_kind: ResourceKind::Tokens,
            }
        );
    }

    #[test]
    fn with_turn_bound_rejects_ceiling_below_cold_start_request() {
        let policy = hard_resource_policy();
        let err = policy
            .with_turn_bound(TurnBound {
                cold_start_request: ResourceAmount::UsdCents(2_000),
                ceiling: ResourceAmount::UsdCents(1_000),
            })
            .unwrap_err();
        assert_eq!(
            err,
            PolicyValidationError::TurnCeilingBelowColdStartRequest {
                cold_start_request: ResourceAmount::UsdCents(2_000),
                ceiling: ResourceAmount::UsdCents(1_000),
            }
        );
    }

    #[test]
    fn with_turn_bound_accepts_ceiling_equal_to_cold_start_request() {
        // No band requirement here, unlike the lifetime elastic bound:
        // ceiling == cold_start_request is a valid (if maximally strict)
        // turn bound.
        let policy = hard_resource_policy()
            .with_turn_bound(TurnBound {
                cold_start_request: ResourceAmount::UsdCents(1_000),
                ceiling: ResourceAmount::UsdCents(1_000),
            })
            .expect("ceiling == cold_start_request must be accepted");
        assert_eq!(
            policy.turn,
            Some(TurnBound {
                cold_start_request: ResourceAmount::UsdCents(1_000),
                ceiling: ResourceAmount::UsdCents(1_000),
            })
        );
    }

    // -- fold logic: PolicyDecision::with_turn_outcome --------------------

    #[test]
    fn fold_turn_deny_wins_over_lifetime_admit() {
        let policy = hard_resource_policy();
        let decision = policy
            .evaluate(ResourceAmount::UsdCents(1), 1, Confidence::Medium)
            .expect("compatible kind");
        assert_eq!(decision.admission, Admission::Admit);

        let turn_deny = ConstraintOutcome::Deny(DenyReason::ResourceExceedsTurnCeiling {
            projected: ResourceAmount::UsdCents(999_999),
            turn_ceiling: ResourceAmount::UsdCents(1_000),
        });
        let folded = decision.with_turn_outcome(turn_deny.clone());
        assert_eq!(
            folded.admission,
            Admission::Deny(vec![DenyReason::ResourceExceedsTurnCeiling {
                projected: ResourceAmount::UsdCents(999_999),
                turn_ceiling: ResourceAmount::UsdCents(1_000),
            }])
        );
        assert_eq!(folded.turn_outcome, Some(turn_deny));
    }

    #[test]
    fn fold_turn_deny_wins_over_lifetime_approval_required() {
        let policy = elastic_resource_policy();
        let decision = policy
            .evaluate(ResourceAmount::UsdCents(1501), 901, Confidence::Medium)
            .expect("compatible kind");
        assert!(matches!(decision.admission, Admission::ApprovalRequired(_)));

        let turn_deny_reason = DenyReason::ResourceExceedsTurnCeiling {
            projected: ResourceAmount::UsdCents(50_000),
            turn_ceiling: ResourceAmount::UsdCents(10_000),
        };
        let folded = decision.with_turn_outcome(ConstraintOutcome::Deny(turn_deny_reason.clone()));
        assert_eq!(folded.admission, Admission::Deny(vec![turn_deny_reason]));
    }

    #[test]
    fn fold_turn_deny_appends_onto_an_existing_lifetime_deny_rather_than_replacing_it() {
        let policy = hard_resource_policy();
        let decision = policy
            .evaluate(ResourceAmount::UsdCents(1001), 601, Confidence::Medium)
            .expect("compatible kind");
        let lifetime_reasons = match &decision.admission {
            Admission::Deny(reasons) => reasons.clone(),
            other => panic!("expected Deny, got {other:?}"),
        };
        assert_eq!(lifetime_reasons.len(), 2, "resource and time both denied");

        let turn_deny_reason = DenyReason::ResourceExceedsTurnCeiling {
            projected: ResourceAmount::UsdCents(50_000),
            turn_ceiling: ResourceAmount::UsdCents(10_000),
        };
        let folded = decision.with_turn_outcome(ConstraintOutcome::Deny(turn_deny_reason.clone()));
        match folded.admission {
            Admission::Deny(reasons) => {
                assert_eq!(
                    reasons.len(),
                    3,
                    "the turn reason must be appended, not replace the lifetime reasons: {reasons:?}"
                );
                assert!(reasons.contains(&turn_deny_reason));
                for reason in lifetime_reasons {
                    assert!(
                        reasons.contains(&reason),
                        "an existing lifetime deny reason was dropped: {reason:?}"
                    );
                }
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn fold_turn_admit_leaves_an_existing_lifetime_deny_as_deny() {
        let policy = hard_resource_policy();
        let decision = policy
            .evaluate(ResourceAmount::UsdCents(1001), 601, Confidence::Medium)
            .expect("compatible kind");
        assert!(matches!(decision.admission, Admission::Deny(_)));
        let before = decision.admission.clone();

        let folded = decision.with_turn_outcome(ConstraintOutcome::Admit);
        assert_eq!(
            folded.admission, before,
            "a turn Admit must never change an existing Deny's reasons"
        );
        assert_eq!(folded.turn_outcome, Some(ConstraintOutcome::Admit));
    }

    #[test]
    fn fold_turn_admit_leaves_lifetime_admit_as_admit() {
        // No turn bound configured at all (`turn: None` behavior, the
        // common case today): `evaluate_turn` itself returns `Admit`, and
        // folding that in must leave the decision completely unchanged
        // from today's pre-HORO-1727 behavior.
        let policy = hard_resource_policy();
        let decision = policy
            .evaluate(ResourceAmount::UsdCents(1), 1, Confidence::Medium)
            .expect("compatible kind");
        assert_eq!(decision.admission, Admission::Admit);

        let turn_outcome = policy
            .evaluate_turn(ResourceAmount::UsdCents(1))
            .expect("compatible kind");
        assert_eq!(turn_outcome, ConstraintOutcome::Admit);
        let folded = decision.clone().with_turn_outcome(turn_outcome);
        assert_eq!(folded.admission, decision.admission);
        assert_eq!(folded.turn_outcome, Some(ConstraintOutcome::Admit));
    }

    // -- backward compatibility -------------------------------------------

    #[test]
    fn policy_without_turn_serializes_byte_identically_to_before() {
        // Mirrors `policy_json_with_no_renewal_set_serializes_with_no_renewal_key`:
        // a `Policy` with no turn bound must not serialize a `turn` key at
        // all, so every existing persisted `policy_json` stays
        // byte-identical.
        let policy = hard_resource_policy();
        assert!(policy.turn.is_none());
        let json = serde_json::to_value(&policy).expect("serializes");
        assert!(
            json.get("turn").is_none(),
            "a policy with no turn bound must not serialize a `turn` key at all: got {json}"
        );
    }

    #[test]
    fn old_policy_json_with_no_turn_field_still_deserializes() {
        // A `policy_json` row persisted before this field existed has no
        // `turn` key at all — `#[serde(default)]` must fill in `None`.
        let json = serde_json::json!({
            "policy_schema_version": POLICY_SCHEMA_VERSION,
            "name": "pre-turn-ticket",
            "resource": {
                "mode": "hard",
                "target": {"kind": "usd_cents", "amount": 1000},
                "elastic_ceiling": null,
                "hard_ceiling": {"kind": "usd_cents", "amount": 1000}
            },
            "time": {
                "mode": "hard",
                "target_secs": 600,
                "elastic_ceiling_secs": null,
                "hard_ceiling_secs": 600,
                "deadline": null
            },
            "quality_floor": {
                "revision": 1,
                "criteria": [{"description": "tests pass", "required": true}]
            },
            "min_confidence": "medium",
            "autonomy": "ask_on_approval"
        });
        let policy: Policy = serde_json::from_value(json).expect("deserializes structurally");
        assert!(policy.turn.is_none());
        assert_eq!(policy.turn_fallback_request(), policy.resource.target);
    }

    #[test]
    fn old_policy_decision_json_with_no_turn_outcome_still_deserializes() {
        let policy = hard_resource_policy();
        let decision = policy
            .evaluate(ResourceAmount::UsdCents(1), 1, Confidence::Medium)
            .expect("compatible kind");
        let mut json = serde_json::to_value(&decision).expect("serializes");
        // Simulate a pre-this-ticket `PolicyDecision` row: no `turn_outcome`
        // key at all.
        json.as_object_mut().unwrap().remove("turn_outcome");
        let reparsed: PolicyDecision = serde_json::from_value(json).expect("deserializes");
        assert!(reparsed.turn_outcome.is_none());
        assert_eq!(reparsed.admission, decision.admission);
    }

    #[test]
    fn resource_exceeds_turn_ceiling_is_additive_and_old_deny_reason_json_still_deserializes() {
        // Mirrors `external_approval.rs`'s own
        // `deny_reason_deserializes_the_old_variants_after_the_additive_change`:
        // the new variant must not disturb deserialization of an old,
        // pre-existing `DenyReason` JSON shape.
        let old = r#"{"ResourceExceedsHardCeiling":{"projected":{"kind":"tokens","amount":5000},"hard_ceiling":{"kind":"tokens","amount":2000}}}"#;
        let parsed: DenyReason = serde_json::from_str(old).unwrap();
        assert_eq!(
            parsed,
            DenyReason::ResourceExceedsHardCeiling {
                projected: ResourceAmount::Tokens(5000),
                hard_ceiling: ResourceAmount::Tokens(2000),
            }
        );

        // And the new variant itself round-trips.
        let new = DenyReason::ResourceExceedsTurnCeiling {
            projected: ResourceAmount::Tokens(400_001),
            turn_ceiling: ResourceAmount::Tokens(400_000),
        };
        let json = serde_json::to_string(&new).unwrap();
        let round_tripped: DenyReason = serde_json::from_str(&json).unwrap();
        assert_eq!(round_tripped, new);
    }
}
