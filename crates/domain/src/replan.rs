//! Runtime re-estimation and replan decisions (HORO-1139) — "govern the
//! run": the remaining-work estimate updates from material runtime
//! evidence, and a deterministic, hysteresis-gated decision governs
//! whether that evidence is worth acting on.
//!
//! # Scope
//!
//! This module is domain modeling only, the same discipline
//! [`crate::policy`] follows: every function here is pure (inputs in,
//! outputs out, no I/O, no clock reads except where a `now` is passed
//! explicitly) so it can be unit tested without a daemon, a ledger, or a
//! real Claude Code session. Wiring this into `crates/daemon`'s
//! `ToolInvoked` handling — deciding *when* to call these functions
//! against real per-session tool-call history — is HORO-1139's daemon
//! half, not this module's concern.
//!
//! # What "material" means here
//!
//! Claude Code's `PostToolUse` hook payload exposes a tool *name* but no
//! explicit success/failure signal (verified the same way HORO-1126
//! verified the payload exposes no cost/token data — see
//! `integrations/claude-code/README.md`). So "a tool failed" is not a
//! genuinely available signal, and this module does not pretend
//! otherwise. What IS genuinely available and used here:
//!
//! - [`possible_tool_loop`]: the same tool name invoked repeatedly,
//!   back to back, within a short window — a proxy for "the agent might
//!   be stuck retrying the same thing," not proof of failure.
//! - [`tool_call_count_is_material`]: the session's tool-call count
//!   exceeding what history says is typical for this task's bucket tier
//!   (or, absent enough bucket-specific history, a fixed absolute
//!   fallback) — a proxy for "this task is turning out bigger than
//!   estimated."
//!
//! Two categories the ticket's own text raises — a dependency/migration
//! issue, or a subagent completion/failure — are deliberately NOT
//! detected here: nothing in Claude Code's hook payloads gives this
//! integration a real signal for either. Inventing a heuristic with no
//! underlying signal would be exactly the fabrication this campaign has
//! repeatedly rejected (see `Estimate::cold_start`, `ExecutionReceipt::provider`
//! docs) — so these are left honestly undetected rather than faked.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    confidence::Confidence,
    estimate::Estimate,
    execution_plan::PlanId,
    policy::{Policy, PolicyDecision, PolicyEvaluationError},
    resource_amount::{ResourceAmount, ResourceKind},
    task_identity::TaskId,
};

/// The replan schema version every produced [`ReplanReason`],
/// [`RemainingEstimate`], and [`ReplanRecord`] is tagged with implicitly
/// via this constant — kept alongside `POLICY_SCHEMA_VERSION`/
/// `ESTIMATOR_VERSION`'s traceability pattern. Bump any time this
/// module's decision logic (material-event thresholds, the
/// [`should_replan`] formula, hysteresis semantics) changes.
pub const REPLAN_SCHEMA_VERSION: &str = "replan-v1";

// ---------------------------------------------------------------------
// Material-event detection
// ---------------------------------------------------------------------

/// Absolute tool-call-count fallback threshold used when no
/// bucket-specific historical median is available (cold task class, or
/// too few same-bucket receipts to trust a median). Chosen as a simple,
/// defensible "this is a lot of tool calls for one task" default —
/// documented here rather than silently baked into a call site, per this
/// campaign's estimator-threshold convention (see
/// `libra_governor_domain::MIN_CLASS_SAMPLES` docs for the sibling
/// precedent).
pub const ABSOLUTE_TOOL_CALL_COUNT_FALLBACK: u64 = 20;

/// A session's tool-call count is material once it exceeds this multiple
/// of the historically typical (median) count for its bucket tier.
pub const TOOL_CALL_COUNT_MATERIAL_MULTIPLIER: f64 = 2.0;

/// Number of consecutive invocations of the *same* tool name, with no
/// intervening different tool, that counts as a "possible loop" signal.
pub const DEFAULT_LOOP_STREAK_THRESHOLD: u64 = 4;

/// Whether a session's tool-call count is a material deviation from what
/// history implied. `typical` is the bucket-specific median tool-call
/// count, if enough same-bucket history exists to compute one (see
/// `libra_governor_estimator::typical_tool_call_count_bucketed`); `None`
/// falls back to [`ABSOLUTE_TOOL_CALL_COUNT_FALLBACK`].
pub fn tool_call_count_is_material(actual: u64, typical: Option<u64>) -> bool {
    match typical {
        Some(t) if t > 0 => (actual as f64) > (t as f64) * TOOL_CALL_COUNT_MATERIAL_MULTIPLIER,
        _ => actual > ABSOLUTE_TOOL_CALL_COUNT_FALLBACK,
    }
}

/// Whether a streak of consecutive same-tool invocations is long enough
/// to be a "possible loop" signal.
pub fn possible_tool_loop(same_tool_streak: u64, streak_threshold: u64) -> bool {
    same_tool_streak >= streak_threshold
}

/// Why a replan was materially warranted (HORO-1139). Structured, never
/// a free-text-only string — a caller can match on this and a test can
/// assert on it precisely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplanTriggerKind {
    /// The same tool name was invoked repeatedly, back to back, within a
    /// short window — see [`possible_tool_loop`] and module docs on why
    /// this is a proxy, not proof, of a failure/retry loop.
    PossibleToolLoop,
    /// The session's tool-call count materially exceeded what history
    /// implied — see [`tool_call_count_is_material`].
    ToolCallCountExceeded,
    /// A human or an out-of-band signal explicitly requested a replan
    /// (not detected automatically by anything in this module).
    Manual,
}

/// A structured replan trigger: a category (never a bare string) plus an
/// optional free-text detail for the human-readable message a hook or
/// statusline can render.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplanReason {
    pub trigger: ReplanTriggerKind,
    pub detail: Option<String>,
}

impl ReplanReason {
    pub fn new(trigger: ReplanTriggerKind, detail: impl Into<String>) -> Self {
        Self {
            trigger,
            detail: Some(detail.into()),
        }
    }
}

// ---------------------------------------------------------------------
// Replan tiers
// ---------------------------------------------------------------------

/// Which tier of replanning sophistication produced (or would produce) a
/// [`RemainingEstimate`]/[`ReplanRecord`] (HORO-1139).
///
/// Only [`ReplanTier::Deterministic`] is implemented in this crate.
/// [`ReplanTier::LlmAssisted`] is a documented, not-yet-implemented
/// placeholder: no MCP server and no LLM-based planner exist anywhere in
/// this codebase today (per this ticket's explicit scope boundary), so
/// nothing here ever constructs a `RemainingEstimate` or `ReplanRecord`
/// tagged `LlmAssisted`. The variant exists so a future ticket can add a
/// real LLM-assisted tier without a breaking enum change to every caller
/// that already matches on `ReplanTier` — see
/// [`crate::policy::ConstraintMode`]'s docs for the same "leave room,
/// don't fake it" pattern this ticket's brief asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplanTier {
    /// Cheap, deterministic adjustments only: widen the remaining
    /// estimate's interval and/or downgrade its confidence one step. See
    /// [`RemainingEstimate::from_bucketed`].
    Deterministic,
    /// Not yet implemented. Reserved for a future tier where the
    /// technical execution approach itself needs reasoning (per this
    /// ticket's scope boundary) — e.g. an LLM proposing a genuinely
    /// different plan, not just a wider interval on the same one.
    LlmAssisted,
}

/// Multiplicative widening [`RemainingEstimate::from_bucketed`] applies
/// to the underlying estimate's P80/P90 duration and resource bounds, as
/// the deterministic tier's cheap adjustment for a materially deviating
/// task. `1.5` is a simple, documented MVP 3.0 choice (not a calibrated
/// statistical result — same caveat [`Confidence::from_sample_count`]'s
/// docs carry for its own thresholds): a task that has already triggered
/// a material-deviation replan is assumed to carry meaningfully more
/// tail risk than the original bucketed quantiles implied.
pub const DETERMINISTIC_WIDENING_FACTOR: f64 = 1.5;

/// A recomputed "remaining work" estimate (HORO-1139): conceptually
/// "estimate again, but now conditioned on runtime evidence," reusing
/// `libra-governor-estimator`'s bucketed-quantile machinery for the base
/// numbers (see module docs) and applying the deterministic tier's cheap
/// widening/confidence-downgrade adjustment on top.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemainingEstimate {
    /// The underlying re-estimate, after any deterministic-tier
    /// adjustment has been applied (P80/P90 widened, confidence
    /// downgraded — see [`Self::from_bucketed`]).
    pub estimate: Estimate,
    /// The widening factor actually applied. `1.0` when unwidened (e.g.
    /// a manually requested replan on an otherwise healthy task might
    /// choose not to widen — [`Self::from_bucketed`] always widens
    /// today, but the field is not hardcoded so a future caller can
    /// construct an unwidened `RemainingEstimate` honestly rather than
    /// lying about the factor).
    pub widening_factor: f64,
    /// Whether `estimate.confidence` was downgraded one step from what
    /// the underlying bucketed computation produced, as part of the
    /// same deterministic adjustment.
    pub confidence_downgraded: bool,
    /// Which tier produced this remaining estimate.
    pub tier: ReplanTier,
}

impl RemainingEstimate {
    /// Applies the deterministic tier's cheap adjustment to a freshly
    /// recomputed bucketed [`Estimate`]: widen P80/P90 duration and
    /// resource bounds by [`DETERMINISTIC_WIDENING_FACTOR`], and
    /// downgrade confidence one step (never below [`Confidence::Low`]).
    /// A cold-start estimate (no numeric bounds at all) is passed
    /// through unwidened — there is nothing to widen — but still
    /// reports `tier: Deterministic` since the decision to recompute at
    /// all *was* the deterministic tier's work.
    pub fn from_bucketed(mut estimate: Estimate) -> Self {
        if estimate.cold_start {
            return Self {
                estimate,
                widening_factor: 1.0,
                confidence_downgraded: false,
                tier: ReplanTier::Deterministic,
            };
        }

        estimate.duration_p80_secs = estimate
            .duration_p80_secs
            .map(|s| widen_secs(s, DETERMINISTIC_WIDENING_FACTOR));
        estimate.duration_p90_secs = estimate
            .duration_p90_secs
            .map(|s| widen_secs(s, DETERMINISTIC_WIDENING_FACTOR));
        estimate.resource_p80 = estimate
            .resource_p80
            .map(|r| r.scaled(DETERMINISTIC_WIDENING_FACTOR));
        estimate.resource_p90 = estimate
            .resource_p90
            .map(|r| r.scaled(DETERMINISTIC_WIDENING_FACTOR));

        let downgraded = downgrade_confidence(estimate.confidence);
        let confidence_downgraded = downgraded != estimate.confidence;
        estimate.confidence = downgraded;

        Self {
            estimate,
            widening_factor: DETERMINISTIC_WIDENING_FACTOR,
            confidence_downgraded,
            tier: ReplanTier::Deterministic,
        }
    }
}

fn widen_secs(secs: u64, factor: f64) -> u64 {
    ((secs as f64) * factor).round() as u64
}

fn downgrade_confidence(confidence: Confidence) -> Confidence {
    match confidence {
        Confidence::High => Confidence::Medium,
        Confidence::Medium => Confidence::Low,
        Confidence::Low => Confidence::Low,
    }
}

// ---------------------------------------------------------------------
// The should_replan cost/benefit gate
// ---------------------------------------------------------------------

/// Why a [`ReplanCostBenefit`] could not be evaluated: its
/// [`ResourceAmount`]s did not all share the same [`ResourceKind`], which
/// would make summing/comparing them meaningless (same rationale as
/// [`crate::policy::PolicyEvaluationError::ProjectedResourceKindMismatch`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ReplanDecisionError {
    #[error(
        "replan cost/benefit inputs mix resource kinds: expected {expected:?}, found {found:?}"
    )]
    ResourceKindMismatch {
        expected: ResourceKind,
        found: ResourceKind,
    },
}

/// The raw inputs to the replan decision gate (HORO-1139): the ticket's
/// own formula — "re-plan only when the expected benefit of replanning
/// exceeds its own cost, switching cost, and delay" (see `PRODUCT.md`'s
/// North Star) — made concrete and testable.
///
/// `delay_cost` is the delay a replan itself incurs, expressed
/// commensurably with the other fields as a [`ResourceAmount`] (the
/// caller converts wall-clock delay into the same resource kind the rest
/// of the decision is denominated in — e.g. a USD-per-second burn rate,
/// or simply `ResourceAmount::UsdCents(0)` when delay has no cost in the
/// caller's context). This keeps the gate itself unit-agnostic rather
/// than silently assuming USD.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplanCostBenefit {
    /// The expected benefit of replanning — e.g. avoided overrun from
    /// catching a deviation early.
    pub expected_benefit: ResourceAmount,
    /// The cost of the replanning computation itself (for the
    /// deterministic tier: effectively the cost of a local re-estimate;
    /// for a future LLM-assisted tier: whatever that reasoning costs).
    pub replan_cost: ResourceAmount,
    /// The cost of switching to a new plan (re-orienting, discarding any
    /// work that assumed the old plan's shape).
    pub switching_cost: ResourceAmount,
    /// The cost-equivalent of the delay a replan introduces.
    pub delay_cost: ResourceAmount,
    /// The minimum net gain required to bother replanning at all — a
    /// hysteresis floor distinct from "net gain is merely positive," so
    /// a replan that barely clears zero (and would likely thrash right
    /// back) is still refused. See [`crate::replan::ReplanHysteresisConfig`]
    /// for the complementary time/count-based hysteresis controls.
    pub min_gain: ResourceAmount,
}

/// The full, inspectable result of evaluating a [`ReplanCostBenefit`]
/// (HORO-1139): every value that went into the boolean, not just the
/// boolean — so it can be surfaced to a user or asserted on precisely in
/// a test, mirroring [`crate::policy::PolicyDecision`]'s "explain the
/// verdict" discipline.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ReplanAssumptions {
    pub kind: ResourceKind,
    pub expected_benefit: f64,
    pub replan_cost: f64,
    pub switching_cost: f64,
    pub delay_cost: f64,
    /// `replan_cost + switching_cost + delay_cost`.
    pub total_cost: f64,
    /// `expected_benefit - total_cost`.
    pub net_gain: f64,
    pub min_gain: f64,
    /// `net_gain >= min_gain`.
    pub decision: bool,
}

/// Evaluates whether replanning is worth it: `net_gain =
/// expected_benefit - (replan_cost + switching_cost + delay_cost)`,
/// replan iff `net_gain >= min_gain`. Every [`ResourceAmount`] in
/// `inputs` must share the same [`ResourceKind`] — see
/// [`ReplanDecisionError::ResourceKindMismatch`].
pub fn should_replan(inputs: &ReplanCostBenefit) -> Result<ReplanAssumptions, ReplanDecisionError> {
    let kind = inputs.expected_benefit.kind();
    for amount in [
        &inputs.replan_cost,
        &inputs.switching_cost,
        &inputs.delay_cost,
        &inputs.min_gain,
    ] {
        if amount.kind() != kind {
            return Err(ReplanDecisionError::ResourceKindMismatch {
                expected: kind,
                found: amount.kind(),
            });
        }
    }

    let expected_benefit = inputs.expected_benefit.as_f64();
    let replan_cost = inputs.replan_cost.as_f64();
    let switching_cost = inputs.switching_cost.as_f64();
    let delay_cost = inputs.delay_cost.as_f64();
    let min_gain = inputs.min_gain.as_f64();

    let total_cost = replan_cost + switching_cost + delay_cost;
    let net_gain = expected_benefit - total_cost;
    let decision = net_gain >= min_gain;

    Ok(ReplanAssumptions {
        kind,
        expected_benefit,
        replan_cost,
        switching_cost,
        delay_cost,
        total_cost,
        net_gain,
        min_gain,
        decision,
    })
}

/// Reconstructs a [`ResourceAmount`] of `kind` from a raw `f64` value.
/// Used only to fold a replan cost into a cumulative projected total
/// before handing it to [`Policy::evaluate`] — see
/// [`evaluate_replan_cost_against_policy`]. Mirrors the same
/// kind-preserving rounding [`ResourceAmount::scaled`] already documents
/// (whole cents, whole tokens, `QuotaPercent` saturated at 100.0).
fn rebuild_amount(kind: ResourceKind, value: f64) -> ResourceAmount {
    match kind {
        ResourceKind::Usd => ResourceAmount::UsdCents(value.round() as i64),
        ResourceKind::Tokens => ResourceAmount::Tokens(value.max(0.0).round() as u64),
        ResourceKind::QuotaPercent => ResourceAmount::QuotaPercent(value.clamp(0.0, 100.0) as f32),
    }
}

/// Evaluates a candidate replan's own cost against the task's [`Policy`]
/// (HORO-1139): folds `replan_cost` into `cumulative_resource_so_far`
/// and re-runs [`Policy::evaluate`] against the resulting total — "the
/// replan cost itself is part of task economics," per this ticket's
/// design brief, not a side channel exempt from the same admission rules
/// real work is judged against.
///
/// `cumulative_resource_so_far` and `replan_cost` must share the same
/// [`ResourceKind`] as `policy`'s resource bound, else
/// [`PolicyEvaluationError::ProjectedResourceKindMismatch`] (surfaced by
/// the inner [`Policy::evaluate`] call).
pub fn evaluate_replan_cost_against_policy(
    policy: &Policy,
    cumulative_resource_so_far: ResourceAmount,
    cumulative_duration_so_far_secs: u64,
    replan_cost: ResourceAmount,
    replan_delay_secs: u64,
    estimate_confidence: Confidence,
) -> Result<PolicyDecision, PolicyEvaluationError> {
    let kind = cumulative_resource_so_far.kind();
    let projected_resource = if replan_cost.kind() == kind {
        rebuild_amount(
            kind,
            cumulative_resource_so_far.as_f64() + replan_cost.as_f64(),
        )
    } else {
        // Kind mismatch here is caught by the inner `Policy::evaluate`
        // call below via its own `projected_resource.kind()` check
        // (comparing against the policy's kind), which already returns a
        // named, non-panicking error — no need to duplicate that check
        // here with a different error shape.
        replan_cost
    };
    let projected_duration = cumulative_duration_so_far_secs.saturating_add(replan_delay_secs);

    policy.evaluate(projected_resource, projected_duration, estimate_confidence)
}

// ---------------------------------------------------------------------
// Hysteresis / anti-thrashing
// ---------------------------------------------------------------------

/// Configurable anti-thrashing controls (HORO-1139): a cooldown period
/// since the last auto-replan, and a maximum number of automatic
/// replans per task before further material events must escalate to a
/// human instead of silently replanning again.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ReplanHysteresisConfig {
    /// Minimum wall-clock time since the last auto-replan before another
    /// one is allowed.
    pub cooldown_secs: u64,
    /// Maximum number of automatic (non-escalated) replans a single task
    /// may accumulate. The event that would be the `max + 1`th automatic
    /// replan instead produces [`HysteresisOutcome::EscalateApprovalNeeded`].
    pub max_auto_replans: u32,
}

impl Default for ReplanHysteresisConfig {
    /// `300` seconds (5 minutes) cooldown, `3` automatic replans before
    /// escalation — simple, documented MVP 3.0 defaults (not calibrated
    /// against real usage data yet), matching this crate's convention of
    /// picking a defensible round number and saying so plainly (see
    /// `Confidence::from_sample_count` docs for the sibling precedent).
    fn default() -> Self {
        Self {
            cooldown_secs: 300,
            max_auto_replans: 3,
        }
    }
}

/// Per-task hysteresis state (HORO-1139): how many automatic replans
/// this task has already had, and when the most recent one happened.
/// `Default` is a fresh task that has never been replanned.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct ReplanHysteresisState {
    pub auto_replan_count: u32,
    pub last_replan_at: Option<OffsetDateTime>,
}

/// The result of checking a candidate replan against
/// [`ReplanHysteresisConfig`]/[`ReplanHysteresisState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HysteresisOutcome {
    /// Neither the cooldown nor the max-replan-count applies: replanning
    /// (subject to [`should_replan`] separately) is allowed.
    Allow,
    /// Too soon since the last auto-replan.
    SuppressedCooldown,
    /// This task has already used up its automatic-replan budget; the
    /// next material event must ask a human instead of silently
    /// replanning again.
    EscalateApprovalNeeded,
}

/// Evaluates hysteresis for a candidate auto-replan at `now`. Checked
/// max-count-first: once a task has exhausted its automatic-replan
/// budget, every subsequent material event escalates regardless of how
/// long it has been since the last replan — the cooldown only throttles
/// *within* the still-automatic budget, it does not reset it.
pub fn evaluate_hysteresis(
    config: &ReplanHysteresisConfig,
    state: &ReplanHysteresisState,
    now: OffsetDateTime,
) -> HysteresisOutcome {
    if state.auto_replan_count >= config.max_auto_replans {
        return HysteresisOutcome::EscalateApprovalNeeded;
    }
    if let Some(last) = state.last_replan_at {
        let elapsed_secs = (now - last).whole_seconds().max(0) as u64;
        if elapsed_secs < config.cooldown_secs {
            return HysteresisOutcome::SuppressedCooldown;
        }
    }
    HysteresisOutcome::Allow
}

// ---------------------------------------------------------------------
// Replan linkage / record
// ---------------------------------------------------------------------

/// Identifier for one [`ReplanRecord`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ReplanId(pub Uuid);

impl ReplanId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ReplanId {
    fn default() -> Self {
        Self::new()
    }
}

/// A durable record of one replan (HORO-1139): links the prior
/// [`ExecutionPlan`](crate::ExecutionPlan) to the new one it produced,
/// carries the structured reason, the tier that handled it, and the
/// recomputed remaining estimate. Persisted in the ledger (see
/// `libra-governor-ledger`'s `replan_events` table) so a task's replan
/// history is queryable, not just visible in the moment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplanRecord {
    pub id: ReplanId,
    pub task_id: TaskId,
    pub prior_plan_id: PlanId,
    pub new_plan_id: PlanId,
    pub reason: ReplanReason,
    pub remaining_estimate: RemainingEstimate,
    pub created_at: OffsetDateTime,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        completion_contract::{CompletionContract, CompletionCriterion},
        policy::{AutonomyBoundary, ConstraintMode, PolicyPresetInputs, ResourceBound, TimeBound},
        task_features::BucketTier,
    };

    // -- material-event detection --------------------------------------

    #[test]
    fn tool_call_count_material_uses_bucket_typical_when_available() {
        assert!(!tool_call_count_is_material(10, Some(6)));
        assert!(tool_call_count_is_material(13, Some(6)), "13 > 2x6");
        assert!(
            !tool_call_count_is_material(12, Some(6)),
            "exactly 2x is not > 2x"
        );
    }

    #[test]
    fn tool_call_count_material_falls_back_to_absolute_threshold_when_no_typical() {
        assert!(!tool_call_count_is_material(
            ABSOLUTE_TOOL_CALL_COUNT_FALLBACK,
            None
        ));
        assert!(tool_call_count_is_material(
            ABSOLUTE_TOOL_CALL_COUNT_FALLBACK + 1,
            None
        ));
        assert!(!tool_call_count_is_material(1, Some(0)));
    }

    #[test]
    fn possible_tool_loop_triggers_only_at_or_above_the_streak_threshold() {
        assert!(!possible_tool_loop(3, DEFAULT_LOOP_STREAK_THRESHOLD));
        assert!(possible_tool_loop(4, DEFAULT_LOOP_STREAK_THRESHOLD));
        assert!(possible_tool_loop(10, DEFAULT_LOOP_STREAK_THRESHOLD));
    }

    #[test]
    fn trivial_events_are_correctly_ignored() {
        // A handful of tool calls, no streak, with plenty of history to
        // compare against: neither detector fires.
        assert!(!tool_call_count_is_material(5, Some(10)));
        assert!(!possible_tool_loop(1, DEFAULT_LOOP_STREAK_THRESHOLD));
    }

    // -- RemainingEstimate::from_bucketed --------------------------------

    fn computed_estimate() -> Estimate {
        Estimate {
            duration_p50_secs: Some(30),
            duration_p80_secs: Some(50),
            duration_p90_secs: Some(60),
            resource_p50: Some(ResourceAmount::UsdCents(100)),
            resource_p80: Some(ResourceAmount::UsdCents(150)),
            resource_p90: Some(ResourceAmount::UsdCents(200)),
            confidence: Confidence::High,
            sample_count: 25,
            cold_start: false,
            estimator_version: crate::estimate::ESTIMATOR_VERSION.to_string(),
            reason: None,
            feature_schema_version: crate::task_features::FEATURE_SCHEMA_VERSION.to_string(),
            bucket_tier: BucketTier::Repo,
        }
    }

    #[test]
    fn remaining_estimate_widens_p80_p90_and_downgrades_confidence() {
        let remaining = RemainingEstimate::from_bucketed(computed_estimate());
        assert_eq!(remaining.tier, ReplanTier::Deterministic);
        assert_eq!(remaining.widening_factor, DETERMINISTIC_WIDENING_FACTOR);
        assert!(remaining.confidence_downgraded);
        assert_eq!(remaining.estimate.confidence, Confidence::Medium);
        // P50 is left alone -- only the tail (P80/P90) is widened.
        assert_eq!(remaining.estimate.duration_p50_secs, Some(30));
        assert_eq!(remaining.estimate.duration_p80_secs, Some(75));
        assert_eq!(remaining.estimate.duration_p90_secs, Some(90));
        assert_eq!(
            remaining.estimate.resource_p80,
            Some(ResourceAmount::UsdCents(225))
        );
        assert_eq!(
            remaining.estimate.resource_p90,
            Some(ResourceAmount::UsdCents(300))
        );
    }

    #[test]
    fn remaining_estimate_never_downgrades_confidence_below_low() {
        let mut estimate = computed_estimate();
        estimate.confidence = Confidence::Low;
        let remaining = RemainingEstimate::from_bucketed(estimate);
        assert_eq!(remaining.estimate.confidence, Confidence::Low);
        assert!(!remaining.confidence_downgraded);
    }

    #[test]
    fn remaining_estimate_passes_a_cold_start_estimate_through_unwidened() {
        let remaining = RemainingEstimate::from_bucketed(Estimate::cold_start());
        assert!(remaining.estimate.cold_start);
        assert_eq!(remaining.widening_factor, 1.0);
        assert!(!remaining.confidence_downgraded);
        assert_eq!(remaining.tier, ReplanTier::Deterministic);
    }

    // -- should_replan ----------------------------------------------------

    #[test]
    fn should_replan_says_yes_when_benefit_clearly_exceeds_cost_and_min_gain() {
        let inputs = ReplanCostBenefit {
            expected_benefit: ResourceAmount::UsdCents(1000),
            replan_cost: ResourceAmount::UsdCents(10),
            switching_cost: ResourceAmount::UsdCents(20),
            delay_cost: ResourceAmount::UsdCents(5),
            min_gain: ResourceAmount::UsdCents(100),
        };
        let assumptions = should_replan(&inputs).expect("compatible kinds");
        assert_eq!(assumptions.total_cost, 35.0);
        assert_eq!(assumptions.net_gain, 965.0);
        assert!(assumptions.decision);
    }

    #[test]
    fn should_replan_says_no_when_continuing_is_cheaper_than_replanning() {
        // The ticket's explicitly required case: a real material event
        // occurred, but the expected benefit of replanning does not
        // clear replan_cost + switching_cost + delay_cost, so the gate
        // must refuse even though *something* changed.
        let inputs = ReplanCostBenefit {
            expected_benefit: ResourceAmount::UsdCents(50),
            replan_cost: ResourceAmount::UsdCents(30),
            switching_cost: ResourceAmount::UsdCents(15),
            delay_cost: ResourceAmount::UsdCents(10),
            min_gain: ResourceAmount::UsdCents(0),
        };
        let assumptions = should_replan(&inputs).expect("compatible kinds");
        assert_eq!(assumptions.total_cost, 55.0);
        assert_eq!(assumptions.net_gain, -5.0);
        assert!(!assumptions.decision, "cost exceeds benefit: must refuse");
    }

    #[test]
    fn should_replan_says_no_when_net_gain_is_positive_but_below_min_gain_floor() {
        // Positive net gain alone is not sufficient -- it must also
        // clear the hysteresis-style min_gain floor, or a replan that
        // barely breaks even would thrash right back.
        let inputs = ReplanCostBenefit {
            expected_benefit: ResourceAmount::UsdCents(100),
            replan_cost: ResourceAmount::UsdCents(50),
            switching_cost: ResourceAmount::UsdCents(10),
            delay_cost: ResourceAmount::UsdCents(5),
            min_gain: ResourceAmount::UsdCents(50),
        };
        let assumptions = should_replan(&inputs).expect("compatible kinds");
        assert_eq!(assumptions.net_gain, 35.0);
        assert!(!assumptions.decision, "35 < min_gain of 50");
    }

    #[test]
    fn should_replan_net_gain_exactly_at_min_gain_is_a_yes() {
        let inputs = ReplanCostBenefit {
            expected_benefit: ResourceAmount::UsdCents(100),
            replan_cost: ResourceAmount::UsdCents(0),
            switching_cost: ResourceAmount::UsdCents(0),
            delay_cost: ResourceAmount::UsdCents(0),
            min_gain: ResourceAmount::UsdCents(100),
        };
        let assumptions = should_replan(&inputs).expect("compatible kinds");
        assert_eq!(assumptions.net_gain, 100.0);
        assert!(assumptions.decision);
    }

    #[test]
    fn should_replan_rejects_mismatched_resource_kinds() {
        let inputs = ReplanCostBenefit {
            expected_benefit: ResourceAmount::UsdCents(1000),
            replan_cost: ResourceAmount::Tokens(10),
            switching_cost: ResourceAmount::UsdCents(20),
            delay_cost: ResourceAmount::UsdCents(5),
            min_gain: ResourceAmount::UsdCents(100),
        };
        let err = should_replan(&inputs).unwrap_err();
        assert_eq!(
            err,
            ReplanDecisionError::ResourceKindMismatch {
                expected: ResourceKind::Usd,
                found: ResourceKind::Tokens,
            }
        );
    }

    // -- evaluate_replan_cost_against_policy -------------------------------

    fn quality_floor() -> CompletionContract {
        CompletionContract::first(vec![CompletionCriterion::required("tests pass")])
    }

    fn hard_budget_policy() -> Policy {
        Policy::validated(
            "hard-budget",
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
    fn replan_cost_that_fits_under_the_hard_ceiling_is_admitted() {
        let policy = hard_budget_policy();
        let decision = evaluate_replan_cost_against_policy(
            &policy,
            ResourceAmount::UsdCents(900),
            500,
            ResourceAmount::UsdCents(50),
            10,
            Confidence::Medium,
        )
        .expect("compatible kind");
        assert_eq!(decision.admission, crate::policy::Admission::Admit);
    }

    #[test]
    fn replan_cost_that_pushes_past_the_hard_ceiling_is_denied() {
        let policy = hard_budget_policy();
        let decision = evaluate_replan_cost_against_policy(
            &policy,
            ResourceAmount::UsdCents(980),
            500,
            ResourceAmount::UsdCents(50),
            10,
            Confidence::Medium,
        )
        .expect("compatible kind");
        assert!(matches!(
            decision.admission,
            crate::policy::Admission::Deny(_)
        ));
    }

    #[test]
    fn replan_cost_evaluation_never_alters_protected_criteria() {
        let policy = Policy::balanced(PolicyPresetInputs {
            resource_target: ResourceAmount::UsdCents(1000),
            time_target_secs: 600,
            quality_floor: quality_floor(),
        })
        .expect("valid policy");

        let before = policy
            .evaluate(ResourceAmount::UsdCents(500), 300, Confidence::Medium)
            .expect("compatible kind");
        let after_replan_cost = evaluate_replan_cost_against_policy(
            &policy,
            ResourceAmount::UsdCents(500),
            300,
            ResourceAmount::UsdCents(2000),
            120,
            Confidence::Medium,
        )
        .expect("compatible kind");

        // Even a replan cost that blows the budget (and is Denied) still
        // reports the exact same required criteria as the original,
        // comfortably-admitted decision -- there is no code path here
        // that could have dropped one to "make it fit".
        assert_eq!(
            before.protected_criteria,
            after_replan_cost.protected_criteria
        );
        assert_eq!(
            after_replan_cost.protected_criteria,
            policy
                .quality_floor
                .required_criteria()
                .cloned()
                .collect::<Vec<_>>()
        );
    }

    // -- hysteresis ---------------------------------------------------------

    fn now() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(10_000)
    }

    #[test]
    fn hysteresis_allows_the_first_replan_with_no_prior_state() {
        let config = ReplanHysteresisConfig::default();
        let state = ReplanHysteresisState::default();
        assert_eq!(
            evaluate_hysteresis(&config, &state, now()),
            HysteresisOutcome::Allow
        );
    }

    #[test]
    fn hysteresis_suppresses_a_second_replan_inside_the_cooldown_window() {
        let config = ReplanHysteresisConfig {
            cooldown_secs: 300,
            max_auto_replans: 3,
        };
        let state = ReplanHysteresisState {
            auto_replan_count: 1,
            last_replan_at: Some(now() - time::Duration::seconds(100)),
        };
        assert_eq!(
            evaluate_hysteresis(&config, &state, now()),
            HysteresisOutcome::SuppressedCooldown
        );
    }

    #[test]
    fn hysteresis_allows_again_once_the_cooldown_has_elapsed() {
        let config = ReplanHysteresisConfig {
            cooldown_secs: 300,
            max_auto_replans: 3,
        };
        let state = ReplanHysteresisState {
            auto_replan_count: 1,
            last_replan_at: Some(now() - time::Duration::seconds(301)),
        };
        assert_eq!(
            evaluate_hysteresis(&config, &state, now()),
            HysteresisOutcome::Allow
        );
    }

    #[test]
    fn hysteresis_escalates_once_max_auto_replans_is_reached_even_past_cooldown() {
        let config = ReplanHysteresisConfig {
            cooldown_secs: 300,
            max_auto_replans: 3,
        };
        let state = ReplanHysteresisState {
            auto_replan_count: 3,
            last_replan_at: Some(now() - time::Duration::seconds(10_000)),
        };
        assert_eq!(
            evaluate_hysteresis(&config, &state, now()),
            HysteresisOutcome::EscalateApprovalNeeded,
            "max-count must win over an elapsed cooldown -- it does not reset the budget"
        );
    }

    // -- ReplanRecord round-trip ---------------------------------------------

    #[test]
    fn replan_record_round_trips_through_json_and_links_prior_and_new_plan_ids() {
        let prior = PlanId::new();
        let new_plan = PlanId::new();
        let record = ReplanRecord {
            id: ReplanId::new(),
            task_id: TaskId::new(),
            prior_plan_id: prior,
            new_plan_id: new_plan,
            reason: ReplanReason::new(
                ReplanTriggerKind::ToolCallCountExceeded,
                "17 tool calls vs typical 6",
            ),
            remaining_estimate: RemainingEstimate::from_bucketed(computed_estimate()),
            created_at: now(),
        };
        let json = serde_json::to_string(&record).unwrap();
        let round_tripped: ReplanRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(record, round_tripped);
        assert_eq!(round_tripped.prior_plan_id, prior);
        assert_eq!(round_tripped.new_plan_id, new_plan);
        assert_eq!(
            round_tripped.reason.trigger,
            ReplanTriggerKind::ToolCallCountExceeded
        );
        assert_ne!(record.prior_plan_id, record.new_plan_id);
    }
}
