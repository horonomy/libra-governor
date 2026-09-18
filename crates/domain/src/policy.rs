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
