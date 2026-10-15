//! The `Strategy` trait and this PR's concrete strategies (HORO-1768
//! PR-A).
//!
//! `Strategy` is a trait, never an enum, specifically so PR-B's
//! negative-control mutants (`Unguarded`, `ForeignSession`,
//! `ForgetfulBaseline`) can be added as new implementations without
//! modifying this crate's core — see the ticket brief.

use time::OffsetDateTime;

use libra_governor_domain::pacing::step::{step as pacing_step, PacerState, Scenario};
use libra_governor_domain::pacing::{
    PacingEvent, PacingPreference, Proposal, SimTask, SimTaskId, TaskSet,
};
use libra_governor_domain::{
    AutonomyBoundary, BucketTier, CompletionContract, CompletionCriterion, Confidence,
    ConstraintMode, EntitlementSource, Feasibility, NoSpendBasis, Policy, PrincipalId, Priority,
    ProgressEvidence, QuotaAmount, QuotaScope, QuotaSubject, QuotaUnit, QuotaWindow, QuotaWindowId,
    RegimeBasis, RemainingDuration, RemainingResource, RemainingWorkEstimate, ResourceAmount,
    ResourceBound, ResourceKind, SpendSoFar, TimeBound, TruthStrength, WindowKind,
};

use crate::trace::{window_uuid, TracePreference, TracePriority, TraceTask, TraceUnit, WindowSpec};

/// Whether a strategy could, in principle, be wired against a live
/// admission path today. `Paced` is always [`Capability::SimulationOnly`]
/// — `pacing` has no live consumer yet (HORO-1727's seam; see
/// `domain/tests/pacing_not_wired_live.rs`) — while `StaticBudget` makes
/// no call into `pacing` at all and is therefore
/// [`Capability::LiveCapable`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    LiveCapable,
    SimulationOnly,
}

/// A world-level fact a strategy may be told about, pushed by
/// [`crate::drive::run_trace`] after it applies the corresponding real
/// effect to the [`crate::world::World`]. A strategy is free to ignore
/// any variant — see [`Paced`]'s docs for why it ignores `ForeignSpend`
/// and `QuotaSnapshotTold` by construction (not merely by choosing not to
/// read them).
#[derive(Debug, Clone, PartialEq)]
pub enum DriveEvent {
    TaskCompleted {
        task: u32,
        at: OffsetDateTime,
    },
    Spend {
        task: u32,
        at: OffsetDateTime,
        amount: u64,
    },
    ForeignSpend {
        window: String,
        at: OffsetDateTime,
        amount: u64,
    },
    QuotaSnapshotTold {
        window: String,
        at: OffsetDateTime,
        remaining: u64,
    },
    PreferenceChanged {
        at: OffsetDateTime,
        preference: TracePreference,
    },
}

/// A pluggable admission strategy the driver replays a frozen trace
/// against. `decide` is the only required method; every other hook has a
/// no-op default so a simple strategy (like `StaticBudget`) need not
/// implement machinery it does not use.
pub trait Strategy {
    fn name(&self) -> &str;
    fn capability(&self) -> Capability;

    /// Decides which of `ready` (task ids whose dependencies are all
    /// satisfied and which are neither active nor completed — the one
    /// fact every strategy is told) to start at `now`. Returning a task
    /// id the driver cannot actually start (already active/completed) is
    /// simply ignored by the driver, never a panic — see
    /// `crate::drive::run_trace`.
    fn decide(&mut self, now: OffsetDateTime, ready: &[u32]) -> Vec<u32>;

    /// Informs the strategy of a world-level event. Default: ignored.
    fn notify(&mut self, _event: &DriveEvent) {}

    /// The daemon process restarted: every strategy wipes its own
    /// *ephemeral* in-memory belief, while preserving whatever it tracks
    /// that is really durable ground truth (reloadable from a real
    /// daemon's ledger, not lost by a crash). Default: no-op (a strategy
    /// with no state has nothing to lose). `StaticBudget` overrides
    /// nothing here (its only state is "which tasks are active", which
    /// the driver itself tracks via the world, not the strategy) —
    /// `Paced` does, via `PacerState::restart_reset` (HORO-1881), which
    /// keeps durable fields like `completed` and clears genuinely
    /// ephemeral ones like `active`/`backoff_x100` (t5's scenario).
    fn on_restart(&mut self) {}

    /// The next instant at which this strategy's own admission decision
    /// could change with **no further external event** — a quota
    /// window's sliding bound aging a hold out, or a refill-bucket tick
    /// — distinct from any instant a trace's own `ExogenousEvent` or
    /// task completion already forces the driver to visit regardless.
    /// Default: `None` (no strategy-internal wake instant). `StaticBudget`
    /// never overrides this — its admission is purely concurrency-gated
    /// and only ever changes on `TaskCompleted`, which the driver already
    /// revisits unconditionally.
    ///
    /// PR-B finding: `Paced::decide` only ever reported `Proposal::Start`
    /// task ids, silently discarding every `Proposal::Hold { next }` it
    /// received from `pacing::step` (see that match arm below). The
    /// driver, in turn, only ever scheduled one `Poll` at the trace
    /// epoch and otherwise only re-asked the strategy after an event
    /// already in its queue — never at a `Hold`'s own wake instant. A
    /// `Paced` run that holds a ready task purely because of a
    /// sliding-window bound (not because of a future task completion or
    /// fixture event) could therefore starve indefinitely: nothing ever
    /// told the driver when to look again. Confirmed fixed on
    /// `t1_multi_window` (now completes 4/4, was 2/4) — see
    /// `tests/world_consistency.rs`'s dedicated regression test. This
    /// method plus `crate::drive::run_trace`'s new poll-scheduling make
    /// that wake instant visible to the driver without changing
    /// `Strategy::decide`'s signature or any other PR-A behavior. (A
    /// further, distinct desync this same investigation found —
    /// `notify` discarding `Proposal::Start` — is documented at
    /// `drain_pending_starts`; `next_wake` alone does not fix it. A third
    /// desync this investigation found, `on_restart` wiping `completed`,
    /// was fixed separately under HORO-1881 — see
    /// `PacerState::restart_reset`'s docs and `on_restart` below.)
    fn next_wake(&self, _now: OffsetDateTime) -> Option<OffsetDateTime> {
        None
    }

    /// Drains any task this strategy already internally decided to
    /// start as a side effect of a prior `notify` call, but had no way
    /// to report until now — processed by the driver exactly like
    /// `decide`'s own return value (claims opened, completion
    /// scheduled). Default: empty.
    ///
    /// PR-B finding, same root cause class as `next_wake` above but more
    /// severe: `pacing::step::step` always runs its admission pass
    /// (`try_admit`) on *every* event, including `TaskCompleted`/`Spend`/
    /// `ModeChanged` — not only the `ModeChanged` tick `Paced::decide`
    /// sends. `Paced::notify` calls `self.apply(...)` for
    /// `TaskCompleted`/`Spend`/`PreferenceChanged` and discarded the
    /// returned `Vec<Proposal>` entirely. When that discarded batch
    /// contained a `Proposal::Start`, `pacing::step`'s own `try_admit`
    /// had *already* mutated `PacerState::active` to include it — so the
    /// strategy's internal belief is "this task is running" while the
    /// real `World` was never told to start it at all. The task then
    /// never appears ready to `try_admit` again (it is already active,
    /// per that same internal state), so it is silently never started —
    /// permanently, not just delayed. Confirmed fixed on
    /// `t4_delayed_settlement` (now completes 2/2, was 1/2) — see
    /// `tests/world_consistency.rs`'s dedicated regression test. x0's own
    /// incompleteness is separately legitimate (a real `PastTargetEnd`
    /// cutoff — see `verify.rs`), not an instance of this bug. t6's own
    /// incompleteness IS a real AC1 false throttle at the one instant
    /// it is observed (`verify.rs` confirms real headroom existed) —
    /// but PR-B found that observation is also the LAST one task 2 ever
    /// gets: `Paced::next_wake` never schedules a future recheck after
    /// the overrun backoff, so what t6's own fixture description calls
    /// "throttle (never cancel)" is, in this strategy's actual observed
    /// behavior, a permanent cancel in practice — see
    /// `tests/verify_fixture_claims.rs`'s own doc comment for detail and
    /// disclosure status (not fixed here — unlike `on_restart`'s former
    /// `completed`-wipe, HORO-1881 scoped only that one finding; this t6
    /// finding remains open and undisclosed-by-fix).
    fn drain_pending_starts(&mut self) -> Vec<u32> {
        Vec::new()
    }

    /// The principal this strategy claims to act only on behalf of, if
    /// it scopes itself to one. Default `None`: a strategy that makes no
    /// identity claim at all (every strategy in this file) cannot be
    /// checked for violating one — `None` is "unscoped", never "trusted
    /// to be honest about its scope". PR-B's `verify::verify` reads this
    /// once, before `drive::run_trace` consumes the strategy, and checks
    /// every task it actually starts (via the ground-truth
    /// `trace::TraceTask::principal`, never via anything the strategy
    /// itself reports) against this claim — see
    /// `mutants::ForeignSession`, the one strategy in this crate that
    /// both claims a principal here and then violates it in `decide`.
    fn owning_principal(&self) -> Option<&str> {
        None
    }
}

/// The simplest strategy: admits up to `max_concurrent` ready tasks at a
/// time, with no awareness of quota windows at all. Marked
/// [`Capability::LiveCapable`] because it makes no call into `pacing` —
/// it could be wired against live ready-task data today without
/// reopening HORO-1727's seam.
pub struct StaticBudget {
    max_concurrent: usize,
    active: usize,
}

impl StaticBudget {
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            max_concurrent,
            active: 0,
        }
    }
}

impl Strategy for StaticBudget {
    fn name(&self) -> &str {
        "static_budget"
    }

    fn capability(&self) -> Capability {
        Capability::LiveCapable
    }

    fn decide(&mut self, _now: OffsetDateTime, ready: &[u32]) -> Vec<u32> {
        let mut started = Vec::new();
        for &task in ready {
            if self.active >= self.max_concurrent {
                break;
            }
            self.active += 1;
            started.push(task);
        }
        started
    }

    fn notify(&mut self, event: &DriveEvent) {
        if let DriveEvent::TaskCompleted { .. } = event {
            self.active = self.active.saturating_sub(1);
        }
    }
}

/// Replays `pacing::step` against the trace's own task/window shape.
/// Always [`Capability::SimulationOnly`] — `pacing` has no live consumer
/// (HORO-1727), and this strategy's whole purpose is to exercise that
/// module, never to wire it live.
///
/// By construction this strategy can never see
/// [`DriveEvent::ForeignSpend`] or [`DriveEvent::QuotaSnapshotTold`]:
/// `pacing::PacingEvent` has no variant that could carry either (a
/// third party's spend against a shared window, or a possibly-dishonest
/// told quota figure) — `pacing`'s own scope note says it "does not
/// re-read external ledger evidence". `notify` below simply has no match
/// arm for them. That blindness is the honest default this PR freezes;
/// whether it is a *problem* for a given fixture is PR-B's verifier's
/// question, not something this strategy should quietly work around.
pub struct Paced {
    scenario_tasks: TaskSet,
    windows: Vec<QuotaWindow>,
    policy: Policy,
    preference: PacingPreference,
    state: PacerState,
    seq: u64,
    /// Task ids `pacing::step`'s own admission pass started as a side
    /// effect of a `notify`-triggered event (never `decide`'s own
    /// `ModeChanged` tick — that path already reports its own starts
    /// directly via `decide`'s return value). See
    /// `Strategy::drain_pending_starts`'s docs for why this exists.
    pending_starts: Vec<u32>,
}

impl Paced {
    pub fn new(
        tasks: &[TraceTask],
        windows: &[WindowSpec],
        initial_preference: &TracePreference,
    ) -> Self {
        let sim_tasks: Vec<SimTask> = tasks.iter().map(to_sim_task).collect();
        let scenario_tasks =
            TaskSet::validated(sim_tasks).expect("fixture task set must already be acyclic");
        let quota_windows = windows.iter().map(to_quota_window).collect();
        Self {
            scenario_tasks,
            windows: quota_windows,
            policy: default_policy(),
            preference: to_pacing_preference(initial_preference),
            state: PacerState::default(),
            seq: 0,
            pending_starts: Vec::new(),
        }
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn apply(&mut self, event: PacingEvent) -> Vec<Proposal> {
        let scenario = Scenario {
            tasks: &self.scenario_tasks,
            windows: &self.windows,
            contract: None,
            preference: self.preference.clone(),
        };
        let (new_state, proposals) = pacing_step(&self.state, &event, &self.policy, &scenario);
        self.state = new_state;
        proposals
    }

    /// Records every `Proposal::Start` this `apply()` call produced into
    /// `pending_starts`, for `drain_pending_starts` to surface to the
    /// driver later. Only ever called from `notify`'s event-specific
    /// arms — `decide` reports its own `apply()` call's starts directly
    /// via its return value instead (see `Strategy::drain_pending_starts`'s
    /// docs for why both paths exist).
    fn stash_pending_starts(&mut self, proposals: Vec<Proposal>) {
        self.pending_starts
            .extend(proposals.into_iter().filter_map(|p| match p {
                Proposal::Start { task, .. } => Some(task.0),
                Proposal::Hold { .. } => None,
            }));
    }
}

impl Strategy for Paced {
    fn name(&self) -> &str {
        "paced"
    }

    fn capability(&self) -> Capability {
        Capability::SimulationOnly
    }

    fn decide(&mut self, now: OffsetDateTime, ready: &[u32]) -> Vec<u32> {
        let seq = self.next_seq();
        // `step` re-runs admission on every event regardless of its
        // kind — a `ModeChanged` marker at `now` is the same "drive
        // admission forward" tick `pacing::simulate` itself synthesizes
        // for a pending timer (see `simulate.rs`'s own docs). `ready` is
        // not otherwise consulted here: `try_admit` derives readiness
        // itself from `scenario.tasks` and `state.completed`, which this
        // struct's own `TaskSet`/`PacerState` already track identically.
        let _ = ready;
        let proposals = self.apply(PacingEvent::ModeChanged { at: now, seq });
        proposals
            .into_iter()
            .filter_map(|p| match p {
                Proposal::Start { task, .. } => Some(task.0),
                Proposal::Hold { .. } => None,
            })
            .collect()
    }

    fn notify(&mut self, event: &DriveEvent) {
        match event {
            DriveEvent::TaskCompleted { task, at } => {
                let seq = self.next_seq();
                let proposals = self.apply(PacingEvent::TaskCompleted {
                    at: *at,
                    seq,
                    task: SimTaskId(*task),
                });
                self.stash_pending_starts(proposals);
            }
            DriveEvent::Spend { task, at, amount } => {
                let unit = self
                    .scenario_tasks
                    .get(SimTaskId(*task))
                    .map(|t| estimate_unit(&t.estimate))
                    .unwrap_or(QuotaUnit::Tokens);
                let seq = self.next_seq();
                let proposals = self.apply(PacingEvent::Spend {
                    at: *at,
                    seq,
                    task: SimTaskId(*task),
                    amount: QuotaAmount::new(unit, *amount),
                });
                self.stash_pending_starts(proposals);
            }
            DriveEvent::PreferenceChanged { at, preference } => {
                self.preference = to_pacing_preference(preference);
                let seq = self.next_seq();
                let proposals = self.apply(PacingEvent::ModeChanged { at: *at, seq });
                self.stash_pending_starts(proposals);
            }
            // By design: see this struct's own docs on why `pacing` has
            // no event shape for either variant.
            DriveEvent::ForeignSpend { .. } | DriveEvent::QuotaSnapshotTold { .. } => {}
        }
    }

    fn on_restart(&mut self) {
        // The t5 scenario: a daemon restart wipes every active hold,
        // backoff ratio, and pending timer — genuinely ephemeral
        // in-process scheduling state — but keeps `completed` and the
        // other durable ground truth `PacerState::restart_reset`
        // preserves (HORO-1881; see that method's own docs for the full
        // field-by-field reasoning). Using `PacerState::default()` here
        // instead used to also wipe `completed`, which permanently
        // stranded any task whose dependency had already really
        // completed before the restart — see `restart_reset`'s docs and
        // `tests/world_consistency.rs`'s t5 coverage.
        // `scenario_tasks`/`windows`/`policy` survive untouched (they are
        // the frozen scenario definition, read from durable config, not
        // runtime state).
        self.state = self.state.restart_reset();
        self.seq = 0;
    }

    /// `PacerState::pending_timer` already tracks exactly this — the
    /// earliest instant `try_admit` found a held task that could become
    /// admissible with no new event, recomputed on every `step` call —
    /// so this is a read of existing state, not new bookkeeping. See
    /// the trait docs above for why the driver needs this exposed.
    fn next_wake(&self, _now: OffsetDateTime) -> Option<OffsetDateTime> {
        self.state.pending_timer()
    }

    fn drain_pending_starts(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.pending_starts)
    }
}

fn estimate_unit(estimate: &RemainingWorkEstimate) -> QuotaUnit {
    match &estimate.resource {
        RemainingResource::Quantiles { kind, .. } => match kind {
            ResourceKind::Tokens => QuotaUnit::Tokens,
            ResourceKind::Usd => QuotaUnit::UsdCents,
            ResourceKind::QuotaPercent => QuotaUnit::Requests,
        },
        _ => QuotaUnit::Tokens,
    }
}

fn trace_unit_to_quota_unit(unit: TraceUnit) -> QuotaUnit {
    match unit {
        TraceUnit::Tokens => QuotaUnit::Tokens,
        TraceUnit::Requests => QuotaUnit::Requests,
        TraceUnit::UsdCents => QuotaUnit::UsdCents,
    }
}

fn trace_unit_to_resource_kind(unit: TraceUnit) -> ResourceKind {
    match unit {
        TraceUnit::Tokens => ResourceKind::Tokens,
        TraceUnit::Requests => ResourceKind::QuotaPercent,
        TraceUnit::UsdCents => ResourceKind::Usd,
    }
}

fn trace_priority_to_priority(p: TracePriority) -> Priority {
    match p {
        TracePriority::High => Priority::High,
        TracePriority::Normal => Priority::Normal,
        TracePriority::Low => Priority::Low,
    }
}

fn to_pacing_preference(pref: &TracePreference) -> PacingPreference {
    match pref {
        TracePreference::Sustain {
            horizon_secs,
            continuity_reserve_bp,
        } => PacingPreference::Sustain {
            horizon_secs: *horizon_secs,
            working_hours: None,
            continuity_reserve_bp: *continuity_reserve_bp,
        },
        TracePreference::Burst {
            target_end_offset_secs,
            max_fanout,
        } => PacingPreference::Burst {
            target_end: crate::trace::TRACE_EPOCH
                + time::Duration::seconds(
                    i64::try_from(*target_end_offset_secs).unwrap_or(i64::MAX),
                ),
            max_fanout: *max_fanout,
        },
    }
}

fn to_quota_window(spec: &WindowSpec) -> QuotaWindow {
    let kind = match spec.kind {
        crate::trace::WindowSpecKind::Sliding { length_secs, limit } => {
            WindowKind::Sliding { length_secs, limit }
        }
        crate::trace::WindowSpecKind::RefillBucket {
            capacity,
            refill_amount,
            refill_period_secs,
        } => WindowKind::RefillBucket {
            capacity,
            refill_amount,
            refill_period_secs,
            anchored_at: crate::trace::TRACE_EPOCH,
            level_at_anchor: capacity,
        },
    };
    QuotaWindow::validated(
        QuotaWindowId(window_uuid(&spec.name)),
        QuotaScope {
            subject: QuotaSubject::Principal(PrincipalId("fixture".to_string())),
            source: EntitlementSource::OperatorConfigured,
            confidence: Confidence::High,
            observed_at: crate::trace::TRACE_EPOCH,
            valid_until: None,
        },
        trace_unit_to_quota_unit(spec.unit),
        kind,
    )
    .expect("fixture window specs must already be valid")
}

/// Builds a [`RemainingWorkEstimate`] with the task's relative duration
/// and spend as its p50/p80/p90 quantiles (a frozen trace carries no
/// distribution, just one real figure — this collapses the quantile
/// spread to that single point). Mirrors `pacing::step`'s own test
/// helpers (`task_with_tokens`), which this crate cannot reuse directly:
/// they live under `#[cfg(test)] pub(crate)` in `domain`, unreachable
/// from an external crate.
fn to_sim_task(task: &TraceTask) -> SimTask {
    let kind = trace_unit_to_resource_kind(task.unit);
    let amount = |v: u64| match task.unit {
        TraceUnit::Tokens => ResourceAmount::Tokens(v),
        TraceUnit::UsdCents => ResourceAmount::UsdCents(i64::try_from(v).unwrap_or(i64::MAX)),
        TraceUnit::Requests => ResourceAmount::QuotaPercent(v as f32),
    };
    let estimate = RemainingWorkEstimate {
        schema_version: "remaining-work-v1".to_string(),
        estimator_version: "pacing-gate-fixture".to_string(),
        duration: RemainingDuration::Quantiles {
            p50_secs: task.relative_duration_secs,
            p80_secs: task.relative_duration_secs,
            p90_secs: task.relative_duration_secs,
            conditional_n: 20,
        },
        resource: RemainingResource::Quantiles {
            kind,
            p50: amount(task.relative_spend),
            p80: amount(task.relative_spend),
            p90: amount(task.relative_spend),
            conditional_n: 20,
            weakest_truth: TruthStrength::Metered,
        },
        feasibility: Feasibility::Insufficient {
            conditional_n: 0,
            required: 5,
        },
        confidence: Confidence::High,
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
            observed_at: crate::trace::TRACE_EPOCH,
        },
    };
    SimTask {
        id: SimTaskId(task.id),
        principal: PrincipalId(task.principal.clone()),
        depends_on: task.depends_on.iter().copied().map(SimTaskId).collect(),
        priority: trace_priority_to_priority(task.priority),
        deadline: None,
        estimate,
    }
}

/// A permissive default `Policy` every fixture shares — wide resource/
/// time ceilings and `Confidence::Low` so a fixture's own windows and
/// pacing preference are what actually constrains admission, not an
/// incidental policy limit. Mirrors (but cannot reuse — see
/// `to_sim_task`'s docs) `pacing::forecast::tests_support::policy`.
pub fn default_policy() -> Policy {
    Policy::validated(
        "pacing-gate-fixture",
        ResourceBound {
            mode: ConstraintMode::Elastic,
            target: ResourceAmount::Tokens(10_000_000),
            elastic_ceiling: Some(ResourceAmount::Tokens(20_000_000)),
            hard_ceiling: ResourceAmount::Tokens(30_000_000),
        },
        TimeBound {
            mode: ConstraintMode::Elastic,
            target_secs: 86_400,
            elastic_ceiling_secs: Some(172_800),
            hard_ceiling_secs: Some(259_200),
            deadline: None,
        },
        CompletionContract::first(vec![CompletionCriterion::required("fixture floor")]),
        Confidence::Low,
        AutonomyBoundary::AskOnApproval,
    )
    .expect("fixture default policy must be valid")
}

/// Exposed so `tests/` can build world windows that agree byte-for-byte
/// with what `Paced` constructs, without duplicating the mapping.
pub fn trace_window_unit(unit: TraceUnit) -> QuotaUnit {
    trace_unit_to_quota_unit(unit)
}
