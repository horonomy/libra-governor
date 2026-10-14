//! Independent verifier (HORO-1768 PR-B).
//!
//! # AC4 coverage ("at least one invalid/corrupted snapshot, concurrent
//! reservation race and lost-response case correctly detected")
//!
//! This crate does not itself implement AC4 — it is a simulation harness
//! for pacing admission, not a ledger/quota-source test suite. HORO-1768's
//! own dependency notes claimed AC4 is mostly already covered elsewhere;
//! verified here (reading each file on current `main`, not assumed) by
//! exact test name, per sub-case:
//!
//! - **Invalid/corrupted snapshot**: `crates/quota-source/src/response.rs`
//!   (`#[cfg(test)]`) — `a_malformed_body_is_unavailable`,
//!   `a_unit_mismatch_is_unavailable`,
//!   `an_unsupported_schema_version_is_unavailable`; `crates/quota-source/src/fixture.rs`
//!   (`#[cfg(test)]`) — `rejects_a_fixture_with_an_unexpected_field`,
//!   `rejects_a_fixture_provenance_with_an_unexpected_field`,
//!   `rejects_a_subject_mismatch_from_a_fixture`,
//!   `rejects_an_unparseable_timestamp`,
//!   `rejects_an_unsupported_schema_version`,
//!   `rejects_a_unit_mismatched_fixture`, `rejects_a_stale_fixture`.
//! - **Concurrent reservation race**: `crates/ledger/tests/shared_pool_adversarial.rs`
//!   — `concurrent_contention_varied_parameters_never_oversubscribes`,
//!   `concurrent_same_principal_same_key_race_grants_exactly_once`,
//!   `concurrent_cross_principal_idempotency_key_race_never_merges`,
//!   `concurrent_duplicate_settlement_never_double_counts`;
//!   `crates/ledger/tests/reservation_concurrency.rs` —
//!   `concurrent_optional_reservations_never_double_spend_the_shared_envelope`,
//!   `concurrent_required_work_reservations_never_over_draw_the_completion_reserve`;
//!   `crates/daemon/tests/launch_gate_integration.rs` —
//!   `concurrent_admits_never_collectively_exceed_capacity`.
//! - **Lost-response case**: `crates/daemon/tests/launch_gate_integration.rs`
//!   — `retrying_admit_after_the_original_hold_expired_never_resolves_to_start`
//!   and `retrying_admit_after_the_original_hold_was_released_never_resolves_to_start`
//!   (both explicitly model "the original Start response was lost in
//!   transit"); `crates/ledger/tests/reservation_concurrency.rs` —
//!   `a_reservation_left_active_by_a_crashed_process_is_reclaimed_on_reconciliation`
//!   (a crashed-process variant of the same class).
//!
//! `crates/domain/src/quota_window/tests.rs` was also checked: it covers
//! DST/timezone-ambiguous window-boundary math (e.g.
//! `daily_reset_spans_23_hours_across_spring_forward`), real robustness
//! coverage but NOT one of AC4's three named sub-cases — noted here so
//! this citation list is not silently missing a file the ticket's own
//! dependency notes named, not claimed as AC4 evidence.
//!
//! Conclusion: HORO-1768's own claim holds — AC4 is genuinely covered by
//! existing, already-merged tests; no new regression test was needed for
//! AC4 itself. The one new regression test this PR *does* add elsewhere
//! (`crates/ledger/tests/shared_pool_adversarial.rs`'s
//! `stale_out_of_order_snapshot_incorrectly_overwrites_a_newer_more_restrictive_reading`,
//! `#[ignore]`d, HORO-1880) is a DIFFERENT, PR-B-session-discovered
//! finding — a stale-but-well-formed snapshot silently overwriting a
//! newer one via unconditional upsert — distinct from AC4's "invalid/
//! corrupted snapshot" sub-case (which is about malformed/schema-invalid
//! data, already covered above).
//!
//! [`verify`] computes every AC1 metric **structurally**, from
//! [`crate::world::World`]'s timestamped, ground-truth event log
//! ([`crate::world::World::log`]) plus the frozen [`crate::trace::TraceSpec`]
//! itself — never from a strategy's own self-report (a strategy is not
//! even available to this function; see [`StrategyMeta`] below for the
//! one piece of strategy metadata this module needs, captured by the
//! caller *before* `drive::run_trace` consumes the strategy).
//!
//! # Occupancy reconstruction
//!
//! [`crate::world::World`] does not itself expose historical occupancy —
//! only the final [`crate::world::WorldWindow`] state. This module
//! rebuilds an occupancy timeline from [`crate::drive::DriveOutcome::log`]
//! (the driver's own replay log, which is a superset of `World::log`:
//! every world-mutating fact `World::log` carries, interleaved with the
//! [`crate::drive::DriveLogEntry::Offered`] decision points `World::log`
//! has no record of at all), in the exact order `drive::run_trace`
//! applied them, mirroring `drive.rs`'s own "every window sharing a
//! task's unit sees the claim" rule (`drive.rs`'s `for window in
//! &spec.windows { if window.unit == task_spec.unit { ... } }`).
//!
//! Two different cuts through the same timeline answer two different
//! questions:
//!
//! - **Hard-limit check** (AC2): processes a `Started` entry's own claim
//!   *after* checking it against whatever is already open at that exact
//!   point, including any sibling task started earlier in the very same
//!   decision batch — this is exactly what `World::open_claim` does, in
//!   the same order `drive::run_trace` calls it.
//! - **False-throttle check** (AC1): evaluates each `Offered` entry's
//!   held tasks (`ready` minus `started`) against occupancy AFTER this
//!   same batch's own starts have been applied — "given everything the
//!   strategy actually, really did this round, was there still headroom
//!   for this held task?". Checking against PRE-batch state instead
//!   would over-count: on `x0_unguarded`, all 5 tasks are ready at once
//!   but the window only ever admits one of them at a time — judged
//!   against empty pre-batch occupancy, all 4 held tasks would
//!   individually "fit", even though at most one of them ever genuinely
//!   could, given the one the strategy already admitted alongside them.
//!   Post-batch state asks the right question instead.
//!
//! # Time-bracketed, not entry-counted
//!
//! An `Offered` entry is logged at *every* poll where `ready` is
//! non-empty, so the same held task can appear in many consecutive
//! entries, sometimes at the same instant. Counting entries would make
//! every duration-shaped metric below change with poll granularity alone
//! (the exact bug this module's own regression guards against — see
//! `tests/verify_metrics.rs`). Every metric here is instead computed as
//! a **time interval** per task (first-seen to last-seen, or first-seen
//! to started), and every *count* is a count of **distinct tasks**
//! affected, never of log entries.

use std::collections::{BTreeMap, BTreeSet};

use time::OffsetDateTime;

use crate::drive::{DriveLogEntry, DriveOutcome};
use crate::strategy::{Capability, Strategy};
use crate::trace::{TraceSpec, TraceTask, TraceUnit, WindowSpec, WindowSpecKind, TRACE_EPOCH};

/// The one piece of strategy metadata `verify` needs, captured from a
/// `&dyn Strategy` *before* the strategy is boxed/moved into
/// `drive::run_trace` (which consumes it) — never recovered from the
/// consumed strategy afterward, and never anything the strategy reports
/// about its own behavior during the run (only its fixed, declared
/// identity/capability).
#[derive(Debug, Clone)]
pub struct StrategyMeta {
    pub name: String,
    pub capability: Capability,
    pub owning_principal: Option<String>,
}

impl StrategyMeta {
    pub fn of(strategy: &dyn Strategy) -> Self {
        Self {
            name: strategy.name().to_string(),
            capability: strategy.capability(),
            owning_principal: strategy.owning_principal().map(str::to_string),
        }
    }
}

/// Per-task outcome, independent of whether the task ever completed.
#[derive(Debug, Clone)]
pub struct TaskOutcome {
    pub task: u32,
    pub principal: String,
    pub completed: bool,
    pub settled: Option<u64>,
    /// First instant this task appeared in some `Offered.ready` list (or
    /// `None` if it never did — e.g. a dependency never completed).
    pub first_ready_at: Option<OffsetDateTime>,
    pub started_at: Option<OffsetDateTime>,
    /// Was this task EVER observed ready-but-not-started while the
    /// world's ground truth shows every window sharing its unit had
    /// headroom for it? One bool per task, not one per `Offered` sighting
    /// — see module docs on time-bracketing, not entry-counting.
    pub false_throttled: bool,
    /// `[first_held_at, last_held_at]` bracketing every consecutive
    /// false-throttle-eligible sighting — the interval this task's
    /// `false_throttle_secs` contribution is computed from.
    pub false_throttle_first_at: Option<OffsetDateTime>,
    pub false_throttle_last_at: Option<OffsetDateTime>,
}

impl TaskOutcome {
    /// Wall-clock seconds this task waited between first becoming ready
    /// and actually starting — `None` if it never started at all (a
    /// separate, more severe finding than "waited a while": see
    /// `VerifyReport::tasks_completed` vs. `tasks_total`).
    pub fn involuntary_wait_secs(&self) -> Option<i64> {
        match (self.first_ready_at, self.started_at) {
            (Some(ready), Some(started)) if started > ready => {
                Some((started - ready).whole_seconds())
            }
            (Some(_), Some(_)) => Some(0),
            _ => None,
        }
    }

    /// Seconds this task spent in a confirmed false-throttle state —
    /// `0` if it was never false-throttled.
    pub fn false_throttle_secs(&self) -> i64 {
        match (self.false_throttle_first_at, self.false_throttle_last_at) {
            (Some(first), Some(last)) if last > first => (last - first).whole_seconds(),
            (Some(_), Some(_)) => 0,
            _ => 0,
        }
    }
}

/// One structurally-confirmed violation this verifier found, named by
/// the task and instant it occurred, never by anything the strategy
/// itself reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub task: u32,
    pub at: OffsetDateTime,
    pub detail: String,
}

/// The full result of verifying one (fixture, strategy) run — every
/// field computed from `World`/driver-log ground truth, per this
/// module's own docs.
#[derive(Debug, Clone)]
pub struct VerifyReport {
    pub fixture: String,
    pub strategy: StrategyMeta,
    pub tasks_total: usize,
    pub tasks_completed: usize,
    pub volume_settled: u64,
    pub involuntary_wait_secs: i64,
    pub false_throttle_task_count: usize,
    pub false_throttle_secs: i64,
    /// This crate's own deliberate definition (AC1 does not specify a
    /// formula): for each window, `max(0, limit - true_occupancy at the
    /// end of the trace)`, counted as waste only for a window that had
    /// at least one false-throttled task sharing its unit — i.e.
    /// capacity that went unused AND that some ready task could have
    /// used instead of being declined. A window nobody ever wanted more
    /// of is not "waste" by this definition.
    pub quota_waste: u64,
    /// First-order only: for each task with at least one dependent, the
    /// seconds that task itself spent false-throttled (every dependent
    /// inherits at least that much delay). Does not compound across
    /// multiple hops of a dependency chain — a disclosed simplification,
    /// not a claim of full cascade accounting.
    pub downstream_cooldown_secs: i64,
    /// `tasks_completed as f64 / tasks_total as f64` — a basic
    /// completion-floor proxy for AC1's "completion-contract compliance"
    /// term. This is NOT a read of
    /// `libra_governor_domain::CompletionContract` (that construct lives
    /// inside `Paced`'s own policy/scenario and is never exposed to the
    /// driver or `World` at all) — just the fraction of the trace's own
    /// declared tasks that the ground-truth world confirms completed.
    pub completion_rate: f64,
    pub hard_limit_violations: Vec<Violation>,
    pub principal_violations: Vec<Violation>,
    pub per_task: Vec<TaskOutcome>,
}

/// Running per-window occupancy state, mirroring `World`'s own
/// `true_occupancy` bookkeeping (`settled` sum + `open` claims sum), with
/// one deliberate correction: `World` itself never ages a `Sliding`
/// window's settled spend out at all (`WorldWindow::true_occupancy` is a
/// permanent cumulative sum, regardless of window kind — a known
/// simplification of PR-A's ground-truth model, not something this
/// verifier should blindly inherit). A real sliding window's settled
/// spend DOES expire after `length_secs`; checking a fixture's hard
/// limit against `World`'s own never-decaying sum would falsely flag
/// every honest strategy the moment cumulative settled spend across the
/// whole trace exceeds the limit, even when nothing was ever
/// concurrently over-committed (confirmed empirically: an early version
/// of this check flagged `StaticBudget::new(1)` on `x0_unguarded` after
/// its second task settled, well after the first had already completed
/// and freed its hold). This type therefore keeps settled spend
/// timestamped and re-applies each window's own declared decay at query
/// time, rather than reconstructing `World`'s literal (non-decaying)
/// arithmetic.
#[derive(Default, Clone)]
struct Occupancy {
    /// Timestamped settled/foreign-spend contributions per window name,
    /// in application order — aged out per [`window_limit`]'s sibling,
    /// [`occupancy_of`], according to that window's own declared kind.
    settled: BTreeMap<String, Vec<(OffsetDateTime, u64)>>,
    /// Current open-claim `(unit, amount)` per task id — the amount is
    /// identical across every window sharing that task's unit, mirroring
    /// `World::open_claim`'s own "every window of the unit sees the same
    /// amount" rule. Open claims never decay by age — they are live,
    /// current commitments, not historical spend.
    open: BTreeMap<u32, (TraceUnit, u64)>,
}

impl Occupancy {
    fn matching_windows(spec: &TraceSpec, unit: TraceUnit) -> Vec<&WindowSpec> {
        spec.windows.iter().filter(|w| w.unit == unit).collect()
    }

    fn apply_start(&mut self, _spec: &TraceSpec, task: &TraceTask) {
        self.open.insert(task.id, (task.unit, task.relative_spend));
    }

    fn apply_spend(&mut self, _spec: &TraceSpec, task: &TraceTask, amount: u64) {
        let prior = self.open.get(&task.id).map(|(_, a)| *a).unwrap_or(0);
        self.open.insert(task.id, (task.unit, prior.max(amount)));
    }

    fn apply_completed(
        &mut self,
        spec: &TraceSpec,
        task: &TraceTask,
        at: OffsetDateTime,
        settled: u64,
    ) {
        self.open.remove(&task.id);
        for w in Self::matching_windows(spec, task.unit) {
            self.settled
                .entry(w.name.clone())
                .or_default()
                .push((at, settled));
        }
    }

    fn apply_foreign_spend(&mut self, window: &str, at: OffsetDateTime, amount: u64) {
        self.settled
            .entry(window.to_string())
            .or_default()
            .push((at, amount));
    }

    /// True occupancy of `window` as of `at`: every still-open claim
    /// whose unit matches this window (no decay — live commitments),
    /// plus settled/foreign spend aged per the window's own declared
    /// kind (see this type's own docs for why `Sliding` must decay here
    /// even though `World` itself never does).
    fn occupancy_of(&self, window: &WindowSpec, at: OffsetDateTime) -> u64 {
        let open_sum: u64 = self
            .open
            .values()
            .filter(|(unit, _)| *unit == window.unit)
            .map(|(_, amount)| *amount)
            .sum();
        let settled_sum: u64 = match self.settled.get(&window.name) {
            Some(entries) => match window.kind {
                WindowSpecKind::Sliding { length_secs, .. } => {
                    let horizon = at
                        - time::Duration::seconds(i64::try_from(length_secs).unwrap_or(i64::MAX));
                    entries
                        .iter()
                        .filter(|(t, _)| *t > horizon && *t <= at)
                        .map(|(_, a)| *a)
                        .sum()
                }
                // Conservative simplification, documented: a refill
                // bucket's real ceiling only ever grows back toward
                // `capacity` over time (refill accrual is never modeled
                // here), so summing every settled entry with no decay
                // can only ever OVERSTATE real occupancy (understate
                // headroom), never the reverse. No fixture in this crate
                // currently uses `RefillBucket` at all, so this path is
                // untested against a real scenario — flagged here rather
                // than silently assumed correct.
                WindowSpecKind::RefillBucket { .. } => entries
                    .iter()
                    .filter(|(t, _)| *t <= at)
                    .map(|(_, a)| *a)
                    .sum(),
            },
            None => 0,
        };
        open_sum + settled_sum
    }
}

fn window_limit(window: &WindowSpec) -> u64 {
    match window.kind {
        WindowSpecKind::Sliding { limit, .. } => limit,
        WindowSpecKind::RefillBucket { capacity, .. } => capacity,
    }
}

/// Would starting `task` at `at`, given `occ`'s current state, violate
/// any window sharing its unit? Returns the names of every window it
/// would violate (empty means "fits everywhere").
fn would_violate(
    spec: &TraceSpec,
    occ: &Occupancy,
    task: &TraceTask,
    at: OffsetDateTime,
) -> Vec<String> {
    let mut violated = Vec::new();
    for w in Occupancy::matching_windows(spec, task.unit) {
        let limit = window_limit(w);
        let after = occ.occupancy_of(w, at) + task.relative_spend;
        if after > limit {
            violated.push(w.name.clone());
        }
    }
    violated
}

/// Verifies one `(spec, outcome)` pair against `meta`, computing every
/// AC1/AC2 metric structurally. See module docs for the full method.
pub fn verify(spec: &TraceSpec, meta: StrategyMeta, outcome: &DriveOutcome) -> VerifyReport {
    let mut occ = Occupancy::default();
    let mut per_task: BTreeMap<u32, TaskOutcome> = spec
        .tasks
        .iter()
        .map(|t| {
            (
                t.id,
                TaskOutcome {
                    task: t.id,
                    principal: t.principal.clone(),
                    completed: false,
                    settled: None,
                    first_ready_at: None,
                    started_at: None,
                    false_throttled: false,
                    false_throttle_first_at: None,
                    false_throttle_last_at: None,
                },
            )
        })
        .collect();

    let mut hard_limit_violations = Vec::new();
    let mut principal_violations = Vec::new();
    // `None` between batches. Set to a clone of `occ` the moment the
    // first `Started` entry of a new decision batch is seen, accumulates
    // that batch's own starts, and is merged back into `occ` once the
    // batch's `Offered` entry has been evaluated — see module docs on
    // why the false-throttle check must see pre-batch state while the
    // hard-limit check must see each sibling start in call order.
    let mut batch_occ: Option<Occupancy> = None;

    for (at, entry) in &outcome.log {
        let at = *at;
        match entry {
            DriveLogEntry::Started { task } => {
                let Some(task_spec) = spec.task(*task) else {
                    continue;
                };
                let current = batch_occ.get_or_insert_with(|| occ.clone());
                // Hard-limit check: evaluated against `current`, which
                // already includes any sibling task started earlier in
                // this same batch — exactly mirroring `World`'s own
                // sequential `start_task`/`open_claim` call order.
                let violated = would_violate(spec, current, task_spec, at);
                if !violated.is_empty() {
                    hard_limit_violations.push(Violation {
                        task: *task,
                        at,
                        detail: format!(
                            "starting task {task} violates window(s) {violated:?} given true \
                             occupancy at this instant"
                        ),
                    });
                }
                if let Some(owning) = &meta.owning_principal {
                    if task_spec.principal != *owning {
                        principal_violations.push(Violation {
                            task: *task,
                            at,
                            detail: format!(
                                "strategy {:?} claims to act only for principal {owning:?}, but \
                                 started task {task}, owned by principal {:?}",
                                meta.name, task_spec.principal
                            ),
                        });
                    }
                }
                current.apply_start(spec, task_spec);
                if let Some(out) = per_task.get_mut(task) {
                    out.started_at = Some(at);
                }
            }
            DriveLogEntry::Spend { task, amount } => {
                if let Some(task_spec) = spec.task(*task) {
                    occ.apply_spend(spec, task_spec, *amount);
                }
            }
            DriveLogEntry::Completed { task, settled } => {
                if let Some(task_spec) = spec.task(*task) {
                    occ.apply_completed(spec, task_spec, at, *settled);
                }
                if let Some(out) = per_task.get_mut(task) {
                    out.completed = true;
                    out.settled = Some(*settled);
                }
            }
            DriveLogEntry::ForeignSpend { window, amount } => {
                occ.apply_foreign_spend(window, at, *amount);
            }
            DriveLogEntry::Restart | DriveLogEntry::PreferenceChanged => {}
            DriveLogEntry::Offered {
                ready,
                started,
                rejected: _,
            } => {
                let started_now: BTreeSet<u32> = started.iter().copied().collect();
                for &task_id in ready {
                    if let Some(out) = per_task.get_mut(&task_id) {
                        if out.first_ready_at.is_none() {
                            out.first_ready_at = Some(at);
                        }
                    }
                }
                // Merge this batch's own starts into the baseline FIRST,
                // then evaluate held tasks against the now fully
                // up-to-date `occ` — i.e. "given everything this round
                // actually admitted (which is real, already-applied
                // occupancy), was there STILL headroom for this held
                // task?". Checking against PRE-batch state instead would
                // over-count: on `x0_unguarded`, all 5 tasks are ready at
                // once with only one ever admissible at a time: judged
                // against pre-batch (empty) occupancy, all 4 held tasks
                // individually "would fit", even though at most ONE of
                // them ever could, given the one admitted alongside them.
                // Post-batch state answers the right question: does
                // genuine headroom remain after what the strategy
                // already, actually, did.
                if let Some(finished_batch) = batch_occ.take() {
                    occ = finished_batch;
                }
                for &task_id in ready {
                    if started_now.contains(&task_id) {
                        continue;
                    }
                    let Some(task_spec) = spec.task(task_id) else {
                        continue;
                    };
                    let violated = would_violate(spec, &occ, task_spec, at);
                    if violated.is_empty() {
                        if let Some(out) = per_task.get_mut(&task_id) {
                            out.false_throttled = true;
                            if out.false_throttle_first_at.is_none() {
                                out.false_throttle_first_at = Some(at);
                            }
                            out.false_throttle_last_at = Some(at);
                        }
                    }
                }
            }
        }
    }

    let tasks_total = spec.tasks.len();
    let tasks_completed = per_task.values().filter(|t| t.completed).count();
    let volume_settled = per_task.values().filter_map(|t| t.settled).sum();
    let involuntary_wait_secs = per_task
        .values()
        .filter_map(TaskOutcome::involuntary_wait_secs)
        .sum();
    let false_throttle_task_count = per_task.values().filter(|t| t.false_throttled).count();
    let false_throttle_secs = per_task
        .values()
        .map(TaskOutcome::false_throttle_secs)
        .sum();

    // Quota waste: unused headroom at trace-end, counted only for a
    // window that saw at least one false-throttled task sharing its
    // unit — see `VerifyReport::quota_waste`'s own docs.
    let false_throttled_units: BTreeSet<TraceUnit> = per_task
        .values()
        .filter(|t| t.false_throttled)
        .filter_map(|t| spec.task(t.task).map(|s| s.unit))
        .collect();
    let trace_end = outcome.log.last().map(|(at, _)| *at).unwrap_or(TRACE_EPOCH);
    let mut quota_waste = 0u64;
    for w in &spec.windows {
        if false_throttled_units.contains(&w.unit) {
            let limit = window_limit(w);
            let final_occ = occ.occupancy_of(w, trace_end);
            quota_waste += limit.saturating_sub(final_occ);
        }
    }

    // Downstream cooldown impact (first-order only — see field docs).
    let depended_on: BTreeSet<u32> = spec
        .tasks
        .iter()
        .flat_map(|t| t.depends_on.iter().copied())
        .collect();
    let downstream_cooldown_secs = per_task
        .values()
        .filter(|t| depended_on.contains(&t.task))
        .map(TaskOutcome::false_throttle_secs)
        .sum();

    let completion_rate = if tasks_total == 0 {
        1.0
    } else {
        tasks_completed as f64 / tasks_total as f64
    };

    VerifyReport {
        fixture: spec.name.clone(),
        strategy: meta,
        tasks_total,
        tasks_completed,
        volume_settled,
        involuntary_wait_secs,
        false_throttle_task_count,
        false_throttle_secs,
        quota_waste,
        downstream_cooldown_secs,
        completion_rate,
        hard_limit_violations,
        principal_violations,
        per_task: per_task.into_values().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> OffsetDateTime {
        TRACE_EPOCH + time::Duration::seconds(secs)
    }

    fn task_outcome() -> TaskOutcome {
        TaskOutcome {
            task: 1,
            principal: "alice".to_string(),
            completed: false,
            settled: None,
            first_ready_at: None,
            started_at: None,
            false_throttled: false,
            false_throttle_first_at: None,
            false_throttle_last_at: None,
        }
    }

    /// Direct proof that these two metrics are bracket-interval math
    /// (first-seen to last-seen), never a per-sighting count: three
    /// sightings of the same held task collapse to exactly one interval
    /// duration, identical to what two sightings at the same two
    /// endpoints would give — a verifier that instead multiplied by
    /// sighting count would report a bigger number here, and would also
    /// change its answer if `drive::run_trace`'s own poll cadence changed
    /// (a fix to that cadence, like the `next_wake`/`drain_pending_starts`
    /// fixes this same PR-B session already made, must never silently
    /// move this crate's own AC1 metrics).
    #[test]
    fn false_throttle_duration_is_bracket_interval_not_sighting_count() {
        let mut three_sightings = task_outcome();
        three_sightings.false_throttle_first_at = Some(at(10));
        three_sightings.false_throttle_last_at = Some(at(40));

        let mut two_sightings_same_span = task_outcome();
        two_sightings_same_span.false_throttle_first_at = Some(at(10));
        two_sightings_same_span.false_throttle_last_at = Some(at(40));

        assert_eq!(
            three_sightings.false_throttle_secs(),
            two_sightings_same_span.false_throttle_secs(),
            "the SAME [first, last] bracket must give the same duration regardless of how many \
             sightings fell inside it"
        );
        assert_eq!(three_sightings.false_throttle_secs(), 30);
    }

    #[test]
    fn false_throttle_secs_is_zero_for_a_single_sighting() {
        let mut single = task_outcome();
        single.false_throttle_first_at = Some(at(10));
        single.false_throttle_last_at = Some(at(10));
        assert_eq!(single.false_throttle_secs(), 0);
    }

    #[test]
    fn false_throttle_secs_is_zero_when_never_held() {
        assert_eq!(task_outcome().false_throttle_secs(), 0);
    }

    #[test]
    fn involuntary_wait_is_none_when_the_task_never_started() {
        let mut never_started = task_outcome();
        never_started.first_ready_at = Some(at(0));
        assert_eq!(never_started.involuntary_wait_secs(), None);
    }

    #[test]
    fn involuntary_wait_is_the_ready_to_started_interval() {
        let mut waited = task_outcome();
        waited.first_ready_at = Some(at(5));
        waited.started_at = Some(at(65));
        assert_eq!(waited.involuntary_wait_secs(), Some(60));
    }
}
