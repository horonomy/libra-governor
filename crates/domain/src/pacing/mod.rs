//! Deterministic SUSTAIN/BURST pacing simulation (HORO-1765).
//!
//! This module answers one question — "given this set of quota windows,
//! in-flight holds, and a candidate task's estimated remaining need, what
//! is the earliest instant it is safe to start that task?" — as a pure,
//! replayable function of evidence, never as a live admission gate.
//!
//! # The HORO-1727 seam (deliberate, hard requirement)
//!
//! Everything in this module is serialize-only data and pure functions.
//! There is no `apply`, no daemon/ledger consumer, and no feature flag
//! gating a live path — because there is no live path. Wiring a real
//! consumer onto this output is explicitly out of scope until HORO-1727
//! resolves the live-admission architecture question; see
//! `domain/tests/pacing_not_wired_live.rs`, which fails the build if any
//! of `daemon/src`, `gateway/src`, or `ledger/src` references this
//! module, and `docs/adr/0016-sustain-burst-pacing.md` for the follow-up
//! ticket that will add the live consumer.
//!
//! # Why this reuses [`crate::quota_window`] and [`crate::progressive`]
//! rather than re-deriving them
//!
//! [`crate::quota_window::QuotaWindow::evaluate`] already answers "is
//! this window blocking, and when does that change" as a pure function
//! of evidence — the forecast in [`forecast`] injects a synthetic,
//! never-persisted [`crate::quota_window::OutstandingHold`] (via the
//! `pub(crate)` [`crate::quota_window::OutstandingHold::projected`] seam)
//! and reads the same `blocking`/`relief` answer back, rather than
//! re-implementing sliding/fixed/bucket arithmetic a second time.
//! Likewise, a task's remaining need is read from the *frozen*
//! [`crate::progressive::RemainingWorkEstimate`] a caller already
//! computed — the same way `crate::replay` consumes it — never from
//! `libra_governor_estimator::remaining_bucketed` directly, which would
//! pull a future-data-leaking estimator dependency into this crate.

pub mod forecast;
pub mod ready;
pub mod simulate;
pub mod step;

use std::collections::{BTreeSet, HashMap};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::business_context::Priority;
use crate::economic_attribution::PrincipalId;
use crate::progressive::RemainingWorkEstimate;
use crate::quota_window::{IndeterminateReason, QuotaWindowId, WorkingHours};

/// A caller's pacing preference for a [`Scenario`](step::Scenario). Carries
/// only scheduling knobs — never a [`crate::Policy`] or
/// [`crate::CompletionContract`] by value, both of which every consumer
/// (forecast, step) takes by reference instead, so a preference can never
/// smuggle in a second, divergent copy of either.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum PacingPreference {
    Sustain {
        horizon_secs: u64,
        working_hours: Option<WorkingHours>,
        continuity_reserve_bp: u16,
    },
    Burst {
        #[serde(with = "crate::economic_event::occurred_at_wire")]
        target_end: OffsetDateTime,
        max_fanout: u16,
    },
}

/// Traceability tag every persisted pacing artifact is tagged with,
/// following this crate's existing `*_SCHEMA_VERSION` convention.
pub const PACING_SCHEMA_VERSION: &str = "pacing-v1";

/// Upper bound on how many relief hops [`forecast::earliest_safe_admit`]
/// will walk before giving up and reporting
/// [`UnavailableReason::ProbeBudgetExhausted`]. A real schedule resolves
/// in a handful of hops; 64 is generous headroom while still bounding
/// worst-case work for a pathological input.
pub const MAX_PROBE_STEPS: usize = 64;

/// Caps enforced at [`Scenario::validated`] construction so a
/// replay/simulation run always has a bounded cost — see
/// [`ScenarioError::TooManyTasks`] and friends.
pub const MAX_SCENARIO_TASKS: usize = 4096;
pub const MAX_SCENARIO_EVENTS: usize = 65536;
pub const MAX_SCENARIO_WINDOWS: usize = 256;

/// Identifies one [`SimTask`] within a single [`Scenario`]. Deliberately a
/// small sequential integer, not a [`Uuid`] — a scenario's task set is
/// fixed at construction and never grows, so there is no need for a
/// globally-unique identifier, and a small integer keeps the
/// deterministic synthetic-id derivation in [`forecast`] simple to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SimTaskId(pub u32);

/// One task in a pacing simulation. Carries only what the forecast and
/// step functions need to decide *when* to start it — never a policy or
/// completion contract of its own; those are supplied once, for the
/// whole simulation, by the caller (see [`forecast::ForecastRequest`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimTask {
    pub id: SimTaskId,
    pub principal: PrincipalId,
    /// Other tasks in the same [`Scenario`] this task cannot start before
    /// completing. Validated acyclic at [`Scenario::validated`] — never
    /// at use time.
    pub depends_on: Vec<SimTaskId>,
    /// Recorded metadata only, consistent with
    /// [`crate::business_context`]'s "priority is never a decision
    /// input" rule elsewhere in this crate — here it *does* drive ready
    /// order (see [`ready::ready_order`]), which is the one place this
    /// module treats it as more than a label, by explicit design
    /// (HORO-1765 AC1/AC3).
    pub priority: Priority,
    /// An optional absolute deadline used only to break ready-order ties
    /// among tasks of equal priority (see [`ready::ready_order`]) —
    /// never a `Policy::time.deadline` substitute and never consulted by
    /// [`forecast::earliest_safe_admit`] itself.
    pub deadline: Option<OffsetDateTime>,
    /// The frozen remaining-work estimate this task's need is derived
    /// from. Never recomputed or re-conditioned inside this module — see
    /// module docs.
    pub estimate: RemainingWorkEstimate,
}

/// Errors constructing a [`Scenario`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScenarioError {
    #[error("scenario has {0} tasks, exceeding the cap of {MAX_SCENARIO_TASKS}")]
    TooManyTasks(usize),
    #[error("scenario has {0} events, exceeding the cap of {MAX_SCENARIO_EVENTS}")]
    TooManyEvents(usize),
    #[error("scenario has {0} windows, exceeding the cap of {MAX_SCENARIO_WINDOWS}")]
    TooManyWindows(usize),
    #[error("duplicate task id {0:?}")]
    DuplicateTaskId(SimTaskId),
    #[error("task {0:?} depends on unknown task {1:?}")]
    UnknownDependency(SimTaskId, SimTaskId),
    #[error("task dependency graph contains a cycle reachable from {0:?}")]
    DependencyCycle(SimTaskId),
}

/// A validated, acyclic set of [`SimTask`]s. Construct only via
/// [`Self::validated`] (Kahn's algorithm) so a cyclic or
/// dangling-dependency task set can never reach [`forecast`] or
/// [`crate::pacing::ready::ready_order`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskSet {
    tasks: Vec<SimTask>,
}

impl TaskSet {
    pub fn validated(tasks: Vec<SimTask>) -> Result<Self, ScenarioError> {
        if tasks.len() > MAX_SCENARIO_TASKS {
            return Err(ScenarioError::TooManyTasks(tasks.len()));
        }
        let mut seen = BTreeSet::new();
        let mut indegree: HashMap<SimTaskId, usize> = HashMap::new();
        let mut dependents: HashMap<SimTaskId, Vec<SimTaskId>> = HashMap::new();
        for task in &tasks {
            if !seen.insert(task.id) {
                return Err(ScenarioError::DuplicateTaskId(task.id));
            }
        }
        for task in &tasks {
            indegree.entry(task.id).or_insert(0);
            for dep in &task.depends_on {
                if !seen.contains(dep) {
                    return Err(ScenarioError::UnknownDependency(task.id, *dep));
                }
                *indegree.entry(task.id).or_insert(0) += 1;
                dependents.entry(*dep).or_default().push(task.id);
            }
        }
        // Kahn's algorithm: repeatedly remove zero-indegree nodes. If any
        // node is never removed, the remaining graph contains a cycle.
        let mut queue: Vec<SimTaskId> = indegree
            .iter()
            .filter(|(_, deg)| **deg == 0)
            .map(|(id, _)| *id)
            .collect();
        queue.sort();
        let mut removed = 0usize;
        let mut cursor = 0usize;
        while cursor < queue.len() {
            let id = queue[cursor];
            cursor += 1;
            removed += 1;
            if let Some(deps) = dependents.get(&id) {
                let mut next_ready: Vec<SimTaskId> = Vec::new();
                for dep in deps {
                    let entry = indegree
                        .get_mut(dep)
                        .expect("dependent id was validated above");
                    *entry -= 1;
                    if *entry == 0 {
                        next_ready.push(*dep);
                    }
                }
                next_ready.sort();
                queue.extend(next_ready);
            }
        }
        if removed != tasks.len() {
            // Report the smallest id still carrying positive indegree —
            // deterministic, not "whichever HashMap iteration hit first".
            let stuck = indegree
                .iter()
                .filter(|(_, deg)| **deg > 0)
                .map(|(id, _)| *id)
                .min()
                .expect(
                    "removed < tasks.len() implies at least one positive-indegree node remains",
                );
            return Err(ScenarioError::DependencyCycle(stuck));
        }
        Ok(Self { tasks })
    }

    pub fn tasks(&self) -> &[SimTask] {
        &self.tasks
    }

    pub fn get(&self, id: SimTaskId) -> Option<&SimTask> {
        self.tasks.iter().find(|t| t.id == id)
    }
}

/// One input event into the pacing state machine (`step`/`simulate`,
/// added in HORO-1765's second PR). Defined here (not in `step.rs`) so
/// [`Proposal`]/[`NextAdmit`] consumers in this PR can already reference
/// the event shape a replay will eventually carry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PacingEvent {
    Spend {
        at: OffsetDateTime,
        seq: u64,
        task: SimTaskId,
        amount: crate::quota_window::QuotaAmount,
    },
    TaskCompleted {
        at: OffsetDateTime,
        seq: u64,
        task: SimTaskId,
    },
    EstimateRevised {
        at: OffsetDateTime,
        seq: u64,
        task: SimTaskId,
        // Boxed: `RemainingWorkEstimate` is the largest field by far among
        // this enum's variants (clippy::large_enum_variant) — boxing
        // keeps every other variant's match arm from paying for a
        // multi-hundred-byte `PacingEvent` on the stack.
        estimate: Box<RemainingWorkEstimate>,
    },
    ModeChanged {
        at: OffsetDateTime,
        seq: u64,
    },
    SnapshotIngested {
        at: OffsetDateTime,
        seq: u64,
        snapshot: crate::quota_window::ProviderSnapshot,
    },
}

impl PacingEvent {
    pub fn at(&self) -> OffsetDateTime {
        match self {
            PacingEvent::Spend { at, .. }
            | PacingEvent::TaskCompleted { at, .. }
            | PacingEvent::EstimateRevised { at, .. }
            | PacingEvent::ModeChanged { at, .. }
            | PacingEvent::SnapshotIngested { at, .. } => *at,
        }
    }

    pub fn seq(&self) -> u64 {
        match self {
            PacingEvent::Spend { seq, .. }
            | PacingEvent::TaskCompleted { seq, .. }
            | PacingEvent::EstimateRevised { seq, .. }
            | PacingEvent::ModeChanged { seq, .. }
            | PacingEvent::SnapshotIngested { seq, .. } => *seq,
        }
    }
}

/// Why [`forecast::earliest_safe_admit`] could not compute a concrete
/// admit instant — every variant names the concrete window/reason so a
/// caller never has to parse a string to find out what was missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    /// A window's own [`crate::quota_window::BlockingStatus`] was
    /// `Indeterminate` (stale/missing/undisclosed snapshot, unsupported
    /// schema, expired entitlement — see
    /// [`crate::quota_window::IndeterminateReason`]).
    WindowIndeterminate(QuotaWindowId, IndeterminateReason),
    /// This window's unit has no corresponding figure on the task's
    /// [`RemainingWorkEstimate`] — e.g. a `Requests`-unit window when the
    /// estimate only carries a `Tokens` quantile. Treating the task as
    /// free of this window (as an earlier bug class did) would silently
    /// admit past a real constraint; this is the honest refusal instead.
    NoEstimateInWindowUnit(QuotaWindowId),
    EstimateInsufficient,
    BelowMinConfidence,
    /// This window's current relief is a provider's own claim
    /// ([`crate::quota_window::Relief::ProviderDeclared`]), never Libra's
    /// own computed guarantee — never treated as a safe admit instant.
    ProviderDeclaredReliefOnly,
    /// This window's relief could not be computed at all
    /// ([`crate::quota_window::Relief::Unknown`]).
    ReliefUnknown,
    /// The probe walked past [`super::MAX_PROBE_STEPS`] relief hops
    /// without reaching an all-`NotBlocking` instant.
    ProbeBudgetExhausted,
    /// The forecast horizon (`Policy::time.deadline`, when set) was
    /// reached before an admit instant was found.
    BeyondHorizon,
    /// The task's need alone (before any other candidate's holds are
    /// even considered) exceeds this window's limit/capacity — no
    /// amount of waiting can ever admit it, so the probe refuses
    /// immediately rather than spending [`super::MAX_PROBE_STEPS`]
    /// relief hops discovering the same thing 64 times.
    NeedExceedsWindowLimit(QuotaWindowId),
    /// This window's relief is
    /// [`crate::quota_window::Relief::AfterOutstandingHoldsSettle`], but
    /// the caller supplied no projected completion instant after the
    /// current probe time for any of its outstanding holds — there is
    /// nothing left to wait for, so no further probing can help.
    NoProjectedRelief(QuotaWindowId),
}

/// Which window is binding the computed admit instant, so a caller can
/// explain *why* a task waits without re-deriving it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NextAdmit {
    Now,
    At {
        #[serde(with = "crate::economic_event::occurred_at_wire")]
        at: OffsetDateTime,
        limiting: QuotaWindowId,
    },
    Unavailable(UnavailableReason),
}

/// One proposal the pacing state machine emits for a probe/step.
/// Deliberately has **no** cancel/stop variant: AC5 (backoff must never
/// cancel in-flight work) is a type-level guarantee here, not just a
/// runtime check a reviewer has to re-verify by reading `step.rs` — see
/// `step.rs`'s own tests (added in this ticket's second PR) for the
/// structural assertion this enables.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Proposal {
    Start {
        task: SimTaskId,
        #[serde(with = "crate::economic_event::occurred_at_wire")]
        at: OffsetDateTime,
        hold: crate::quota_window::QuotaAmount,
        limiting: Option<QuotaWindowId>,
    },
    Hold {
        next: NextAdmit,
    },
}

/// Deterministic synthetic id derivation for a value this module injects
/// into [`crate::quota_window`] evidence but never persists anywhere.
/// XORs a small tag into an existing random (v4) UUID — [`QuotaWindowId`]
/// here — rather than [`Uuid::from_u128`] of a small sequential integer:
/// a small sequential `u128` looks like (and in a large replay, can
/// collide with) another small sequential synthetic id drawn from the
/// same counter space, which is exactly the class of bug a prior session
/// in this campaign hit three times. Folding a tag into an already
/// 122-bit-random source keeps every derived id far from any other
/// small-integer-seeded id without inventing a second random source
/// (determinism forbids [`Uuid::new_v4`] here).
pub(crate) fn synthetic_uuid(source: Uuid, tag: u8) -> Uuid {
    let mut bytes = *source.as_bytes();
    bytes[0] ^= tag;
    bytes[15] ^= tag;
    Uuid::from_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progressive::{
        Feasibility, NoSpendBasis, ProgressEvidence, RemainingDuration, RemainingResource,
        SpendSoFar,
    };
    use crate::regime::RegimeBasis;
    use crate::task_features::BucketTier;
    use crate::Confidence;

    pub(crate) fn test_estimate() -> RemainingWorkEstimate {
        RemainingWorkEstimate {
            schema_version: "remaining-work-v1".to_string(),
            estimator_version: "test".to_string(),
            duration: RemainingDuration::Insufficient {
                conditional_n: 0,
                required: 5,
                elapsed_secs: 0,
            },
            resource: RemainingResource::Insufficient {
                conditional_n: 0,
                required: 5,
            },
            feasibility: Feasibility::Insufficient {
                conditional_n: 0,
                required: 5,
            },
            confidence: Confidence::Low,
            regime: RegimeBasis::default(),
            bucket_tier: BucketTier::Global,
            evidence: ProgressEvidence {
                elapsed_secs: 0,
                spend_so_far: SpendSoFar::NoBasis {
                    reason: NoSpendBasis::NoAccount,
                },
                tool_calls_total: 0,
                tool_calls_since_last_replan: 0,
                same_tool_streak: 0,
                plan_revision: 0,
                auto_replan_count: 0,
                active_lease_count: 0,
                child_account_count: 0,
                gateway_request_count: 0,
                observed_at: OffsetDateTime::UNIX_EPOCH,
            },
        }
    }
    fn task(id: u32, deps: Vec<u32>) -> SimTask {
        SimTask {
            id: SimTaskId(id),
            principal: PrincipalId("p".to_string()),
            depends_on: deps.into_iter().map(SimTaskId).collect(),
            priority: Priority::Normal,
            deadline: None,
            estimate: test_estimate(),
        }
    }

    #[test]
    fn task_set_accepts_a_dag() {
        let set = TaskSet::validated(vec![task(1, vec![]), task(2, vec![1]), task(3, vec![1, 2])]);
        assert!(set.is_ok());
    }

    #[test]
    fn task_set_rejects_a_cycle() {
        let set = TaskSet::validated(vec![task(1, vec![2]), task(2, vec![1])]);
        assert_eq!(set, Err(ScenarioError::DependencyCycle(SimTaskId(1))));
    }

    #[test]
    fn task_set_rejects_duplicate_ids() {
        let set = TaskSet::validated(vec![task(1, vec![]), task(1, vec![])]);
        assert_eq!(set, Err(ScenarioError::DuplicateTaskId(SimTaskId(1))));
    }

    #[test]
    fn task_set_rejects_unknown_dependency() {
        let set = TaskSet::validated(vec![task(1, vec![99])]);
        assert_eq!(
            set,
            Err(ScenarioError::UnknownDependency(
                SimTaskId(1),
                SimTaskId(99)
            ))
        );
    }

    #[test]
    fn synthetic_uuid_differs_from_source_and_is_deterministic() {
        let id = QuotaWindowId::new();
        let a = synthetic_uuid(id.0, 7);
        let b = synthetic_uuid(id.0, 7);
        assert_eq!(a, b);
        assert_ne!(a, id.0);
    }
}
