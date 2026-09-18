//! [`Reservation`], [`TaskBudget`], and the Completion Reserve
//! computation (HORO-1141).
//!
//! # Why a reserve is a column, not a reservation row
//!
//! The ticket's atomic reservation ledger formula is
//! `available = hard_limit - settled_spend - active_reservations -
//! completion_reserve`. `completion_reserve` is a *standing protected
//! amount* carried on [`TaskBudget`], not a reservation that gets
//! individually reserved/settled/released like ordinary work: it is the
//! floor beneath which ordinary (optional) reservations may never dip.
//! A [`Reservation`] instead records, at the moment it draws down that
//! floor for genuinely required work
//! ([`Reservation::drawn_from_reserve`]), how much of the reserve it
//! temporarily consumed — restored on settle/release/expire. See
//! `libra-governor-ledger`'s `reservation` module for the transactional
//! mechanics that keep this invariant atomic under concurrent access.
//!
//! # Determinism
//!
//! [`completion_reserve_for`] is a pure function: estimate and policy in,
//! amount out. No RNG, no clock, no MCP/LLM call — the reserve a task
//! gets is reproducible from its own recorded inputs, matching this
//! repo's "Policy and ledger decisions remain deterministic even if
//! MCP/LLM behavior is absent" acceptance criterion structurally, not by
//! runtime convention.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    completion_contract::CompletionContract, estimate::Estimate, execution_plan::PlanId,
    policy::Policy, resource_amount::ResourceAmount, task_identity::TaskId,
};

/// Traceability tag every produced [`Reservation`]/[`TaskBudget`] is
/// tagged with, following the same convention as
/// [`crate::POLICY_SCHEMA_VERSION`]/[`crate::REPLAN_SCHEMA_VERSION`].
pub const RESERVATION_SCHEMA_VERSION: &str = "reservation-v1";

/// Identifier for one [`Reservation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ReservationId(pub Uuid);

impl ReservationId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ReservationId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ReservationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// What a reservation is FOR (HORO-1141).
///
/// There is deliberately no `CompletionReserve` variant: the standing
/// Completion Reserve is a column on [`TaskBudget`], not a row here — see
/// module docs. The evidence that a given reservation consumed protected
/// capacity is [`Reservation::drawn_from_reserve`], not a class tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReservationClass {
    /// Work covered by a required [`crate::CompletionCriterion`]. May
    /// draw against the Completion Reserve once ordinary (non-reserve)
    /// headroom is exhausted.
    RequiredWork,
    /// Optimization / nice-to-have work. Can never touch the Completion
    /// Reserve — the enforcement half of "Completion Reserve has higher
    /// priority than optional activity."
    OptionalWork,
}

/// The lifecycle state of one [`Reservation`] (HORO-1141).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReservationState {
    /// Holding capacity; not yet settled, released, or expired.
    Active,
    /// Closed with an actual cost — reported, or the conservative
    /// reserved-amount fallback when no usage figure was reported (see
    /// [`Reservation::usage_known`]).
    Settled,
    /// Closed unspent and fully refunded (e.g. superseded by a replan).
    Released,
    /// Reclaimed by reconciliation after `expires_at` — a holder that
    /// crashed or otherwise vanished without settling.
    Expired,
}

/// One reservation of resource capacity against a task's
/// [`TaskBudget`] (HORO-1141).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reservation {
    pub id: ReservationId,
    pub task_id: TaskId,
    /// Which session/subagent holds this reservation. Recorded so a race
    /// is attributable — it is NOT an authorization token: only the
    /// ledger itself, inside an atomic transaction validated against the
    /// task's own recorded budget, can create, settle, or release a
    /// reservation row. No caller-supplied `session_id` can forge a
    /// spend or credit by itself.
    pub session_id: String,
    /// The plan this envelope was reserved for, if any.
    pub plan_id: Option<PlanId>,
    pub class: ReservationClass,
    pub amount: ResourceAmount,
    /// How much of `amount` came out of the protected Completion Reserve
    /// AT RESERVE TIME. Not a live claim on the reserve — settle/
    /// release/expire restore it (fully or partially); this field keeps
    /// its historical value as receipt evidence.
    pub drawn_from_reserve: ResourceAmount,
    pub state: ReservationState,
    /// `Some` iff `state == Settled`.
    pub settled_amount: Option<ResourceAmount>,
    /// `Some(false)` when settlement had no reported usage figure to
    /// compare against (the conservative fallback settled at the full
    /// reserved amount). `None` until settled.
    pub usage_known: Option<bool>,
    /// Caller-supplied replay key, unique per task — the structural half
    /// of idempotent reserve retries: a replayed reserve with the same
    /// key returns the existing row rather than creating a second one.
    pub idempotency_key: String,
    pub created_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
    pub settled_at: Option<OffsetDateTime>,
    pub released_at: Option<OffsetDateTime>,
}

impl Reservation {
    /// `settled_amount - amount` when the actual cost exceeded what was
    /// reserved. Derived, never stored, so the two numbers can never
    /// disagree.
    pub fn overrun(&self) -> Option<ResourceAmount> {
        let settled = self.settled_amount?;
        let delta = settled.as_f64() - self.amount.as_f64();
        if delta > 0.0 {
            Some(ResourceAmount::from_kind_f64(settled.kind(), delta))
        } else {
            None
        }
    }

    /// `amount - settled_amount` when the actual cost came in under what
    /// was reserved.
    pub fn refunded(&self) -> Option<ResourceAmount> {
        let settled = self.settled_amount?;
        let delta = self.amount.as_f64() - settled.as_f64();
        if delta > 0.0 {
            Some(ResourceAmount::from_kind_f64(settled.kind(), delta))
        } else {
            None
        }
    }

    /// How much of [`Self::drawn_from_reserve`] is still outstanding
    /// against the Completion Reserve given this row's current state —
    /// the invariant a test can check without re-deriving ledger
    /// bookkeeping: `Active` outstands all of it, `Settled` outstands
    /// `max(0, drawn - refunded)`, `Released`/`Expired` outstand none
    /// (fully restored).
    pub fn outstanding_draw(&self) -> ResourceAmount {
        match self.state {
            ReservationState::Active => self.drawn_from_reserve,
            ReservationState::Settled => {
                let refunded = self.refunded().map(|r| r.as_f64()).unwrap_or(0.0);
                let remaining = (self.drawn_from_reserve.as_f64() - refunded).max(0.0);
                ResourceAmount::from_kind_f64(self.drawn_from_reserve.kind(), remaining)
            }
            ReservationState::Released | ReservationState::Expired => {
                ResourceAmount::from_kind_f64(self.drawn_from_reserve.kind(), 0.0)
            }
        }
    }
}

/// Where a [`TaskBudget`]'s Completion Reserve amount was computed from
/// (HORO-1141). Honest provenance, matching [`Estimate::cold_start`]'s
/// `cold_start`/`reason` convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionReserveBasis {
    /// Computed from the estimate's P80 resource quantile. Requires
    /// local receipt history carrying a real, kind-matching resource
    /// figure.
    EstimateP80,
    /// Computed from the policy's own resource target — the basis used
    /// whenever no kind-matching estimate quantile is available (e.g. a
    /// cold-start estimate).
    PolicyTarget,
}

/// The persisted per-task resource envelope: the hard limit, settled/
/// active spend implied by the ledger, and the live protected Completion
/// Reserve (HORO-1141).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskBudget {
    pub task_id: TaskId,
    /// Fixed at admission by the policy's own resource kind. Every
    /// reservation and settlement against this task must match it.
    pub resource_kind: crate::resource_amount::ResourceKind,
    /// Copied from `policy.resource.hard_ceiling` at admission — one
    /// source of truth, no separate limit configuration.
    pub hard_limit: ResourceAmount,
    pub initial_completion_reserve: ResourceAmount,
    /// The live, protected reserve. Shrinks when `RequiredWork` draws
    /// against it; restored on refund/release/expiry.
    pub completion_reserve: ResourceAmount,
    pub completion_reserve_basis: CompletionReserveBasis,
    /// The exact [`Policy`] in force at admission, persisted so an
    /// admission decision stays reproducible and reconciliation after a
    /// restart knows the limit without re-reading daemon configuration.
    pub policy: Policy,
    pub reservation_schema_version: String,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

/// The result of [`completion_reserve_for`]: the computed amount plus the
/// provenance/inputs that produced it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionReserveEstimate {
    pub amount: ResourceAmount,
    pub basis: CompletionReserveBasis,
    /// The fraction of `basis`'s amount reserved, after clamping.
    pub fraction: f64,
    pub required_criteria_count: usize,
}

/// The floor fraction of the resource basis reserved for completion work,
/// regardless of how few required criteria a contract declares.
pub const COMPLETION_RESERVE_BASE_FRACTION: f64 = 0.20;
/// Additional fraction reserved per required completion criterion — more
/// required verification work implies more protected capacity is owed.
pub const COMPLETION_RESERVE_PER_REQUIRED_CRITERION: f64 = 0.05;
/// The reserve can never claim more than this fraction of its basis —
/// leaving room for the task's actual exploration/implementation work
/// even under a contract with many required criteria.
pub const COMPLETION_RESERVE_MAX_FRACTION: f64 = 0.50;

/// Computes the Completion Reserve for a task's [`CompletionContract`]
/// against its [`Policy`] (and, when available, its [`Estimate`]).
///
/// Deterministic and pure: no RNG, no clock, no MCP/LLM input (see module
/// docs). Basis selection prefers the estimate's P80 resource quantile
/// when one exists and its [`crate::ResourceKind`] matches the policy's
/// resource kind; otherwise (cold-start estimate, or a kind mismatch) it
/// falls back to the policy's own resource target — the only resource
/// figure that always exists. The reserved fraction grows with the
/// number of required completion criteria (more required verification
/// work implies more protected capacity is owed), clamped at
/// [`COMPLETION_RESERVE_MAX_FRACTION`], and the resulting amount is
/// clamped again so it never exceeds the policy's hard ceiling.
pub fn completion_reserve_for(
    contract: &CompletionContract,
    estimate: Option<&Estimate>,
    policy: &Policy,
) -> CompletionReserveEstimate {
    let policy_kind = policy.resource.target.kind();
    let (basis_amount, basis) = estimate
        .and_then(|e| e.resource_p80)
        .filter(|amount| amount.kind() == policy_kind)
        .map(|amount| (amount, CompletionReserveBasis::EstimateP80))
        .unwrap_or((policy.resource.target, CompletionReserveBasis::PolicyTarget));

    let required_criteria_count = contract.required_criteria().count();
    let fraction = (COMPLETION_RESERVE_BASE_FRACTION
        + COMPLETION_RESERVE_PER_REQUIRED_CRITERION * required_criteria_count as f64)
        .min(COMPLETION_RESERVE_MAX_FRACTION);

    let raw = basis_amount.scaled(fraction);
    let amount = if raw.as_f64() > policy.resource.hard_ceiling.as_f64() {
        policy.resource.hard_ceiling
    } else {
        raw
    };

    CompletionReserveEstimate {
        amount,
        basis,
        fraction,
        required_criteria_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        completion_contract::CompletionCriterion,
        confidence::Confidence,
        policy::{AutonomyBoundary, ConstraintMode, PolicyPresetInputs},
    };

    fn contract_with_required(count: usize) -> CompletionContract {
        let criteria = (0..count)
            .map(|i| CompletionCriterion::required(format!("criterion {i}")))
            .collect();
        CompletionContract::first(criteria)
    }

    fn balanced_policy() -> Policy {
        Policy::balanced(PolicyPresetInputs {
            resource_target: ResourceAmount::Tokens(10_000),
            time_target_secs: 600,
            quality_floor: contract_with_required(1),
        })
        .unwrap()
    }

    #[test]
    fn falls_back_to_policy_target_on_cold_start() {
        let contract = contract_with_required(1);
        let policy = balanced_policy();
        let result = completion_reserve_for(&contract, None, &policy);
        assert_eq!(result.basis, CompletionReserveBasis::PolicyTarget);
        assert_eq!(result.fraction, 0.25);
        assert_eq!(result.amount, ResourceAmount::Tokens(2500));
    }

    #[test]
    fn uses_estimate_p80_when_kind_matches() {
        let contract = contract_with_required(1);
        let policy = balanced_policy();
        let mut estimate = crate::estimate::Estimate::cold_start();
        estimate.resource_p80 = Some(ResourceAmount::Tokens(8_000));
        let result = completion_reserve_for(&contract, Some(&estimate), &policy);
        assert_eq!(result.basis, CompletionReserveBasis::EstimateP80);
        assert_eq!(result.amount, ResourceAmount::Tokens(2000));
    }

    #[test]
    fn falls_back_to_policy_target_on_kind_mismatch() {
        let contract = contract_with_required(1);
        let policy = balanced_policy();
        let mut estimate = crate::estimate::Estimate::cold_start();
        estimate.resource_p80 = Some(ResourceAmount::UsdCents(500));
        let result = completion_reserve_for(&contract, Some(&estimate), &policy);
        assert_eq!(result.basis, CompletionReserveBasis::PolicyTarget);
    }

    #[test]
    fn fraction_grows_with_required_criteria_and_clamps_at_max() {
        let policy = balanced_policy();

        let zero = completion_reserve_for(&contract_with_required(0), None, &policy);
        assert_eq!(zero.fraction, COMPLETION_RESERVE_BASE_FRACTION);

        let five = completion_reserve_for(&contract_with_required(5), None, &policy);
        assert_eq!(five.fraction, 0.20 + 0.05 * 5.0);

        let many = completion_reserve_for(&contract_with_required(20), None, &policy);
        assert_eq!(many.fraction, COMPLETION_RESERVE_MAX_FRACTION);
    }

    #[test]
    fn amount_never_exceeds_hard_ceiling() {
        let contract = contract_with_required(20);
        let policy = Policy::validated(
            "tight",
            crate::policy::ResourceBound {
                mode: ConstraintMode::Hard,
                target: ResourceAmount::Tokens(1000),
                elastic_ceiling: None,
                hard_ceiling: ResourceAmount::Tokens(1000),
            },
            crate::policy::TimeBound {
                mode: ConstraintMode::Hard,
                target_secs: 60,
                elastic_ceiling_secs: None,
                hard_ceiling_secs: Some(60),
                deadline: None,
            },
            contract.clone(),
            Confidence::Low,
            AutonomyBoundary::AskOnApproval,
        )
        .unwrap();

        let result = completion_reserve_for(&contract, None, &policy);
        assert!(result.amount.as_f64() <= policy.resource.hard_ceiling.as_f64());
    }

    #[test]
    fn outstanding_draw_matches_state() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let base = Reservation {
            id: ReservationId::new(),
            task_id: TaskId::new(),
            session_id: "sess-1".to_string(),
            plan_id: None,
            class: ReservationClass::RequiredWork,
            amount: ResourceAmount::Tokens(500),
            drawn_from_reserve: ResourceAmount::Tokens(200),
            state: ReservationState::Active,
            settled_amount: None,
            usage_known: None,
            idempotency_key: "key-1".to_string(),
            created_at: now,
            expires_at: now,
            settled_at: None,
            released_at: None,
        };
        assert_eq!(base.outstanding_draw(), ResourceAmount::Tokens(200));

        let mut settled_full_refund = base.clone();
        settled_full_refund.state = ReservationState::Settled;
        settled_full_refund.settled_amount = Some(ResourceAmount::Tokens(0));
        assert_eq!(
            settled_full_refund.outstanding_draw(),
            ResourceAmount::Tokens(0)
        );

        let mut settled_partial = base.clone();
        settled_partial.state = ReservationState::Settled;
        settled_partial.settled_amount = Some(ResourceAmount::Tokens(400));
        // refunded = 500 - 400 = 100; outstanding = max(0, 200 - 100) = 100
        assert_eq!(
            settled_partial.outstanding_draw(),
            ResourceAmount::Tokens(100)
        );

        let mut released = base.clone();
        released.state = ReservationState::Released;
        assert_eq!(released.outstanding_draw(), ResourceAmount::Tokens(0));

        let mut expired = base;
        expired.state = ReservationState::Expired;
        assert_eq!(expired.outstanding_draw(), ResourceAmount::Tokens(0));
    }

    #[test]
    fn overrun_and_refunded_are_derived_correctly() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let mut reservation = Reservation {
            id: ReservationId::new(),
            task_id: TaskId::new(),
            session_id: "sess-1".to_string(),
            plan_id: None,
            class: ReservationClass::RequiredWork,
            amount: ResourceAmount::Tokens(500),
            drawn_from_reserve: ResourceAmount::Tokens(0),
            state: ReservationState::Settled,
            settled_amount: Some(ResourceAmount::Tokens(800)),
            usage_known: Some(true),
            idempotency_key: "key-1".to_string(),
            created_at: now,
            expires_at: now,
            settled_at: Some(now),
            released_at: None,
        };
        assert_eq!(reservation.overrun(), Some(ResourceAmount::Tokens(300)));
        assert_eq!(reservation.refunded(), None);

        reservation.settled_amount = Some(ResourceAmount::Tokens(300));
        assert_eq!(reservation.overrun(), None);
        assert_eq!(reservation.refunded(), Some(ResourceAmount::Tokens(200)));
    }
}
