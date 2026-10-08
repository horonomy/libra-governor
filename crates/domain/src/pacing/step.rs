//! The SUSTAIN/BURST pacing state machine (HORO-1765 PR-b).
//!
//! [`step`] is the single pure state transition this ticket's simulator
//! is built from: `(state, event, policy, scenario) -> (state, proposals)`.
//! [`super::simulate::simulate`] folds it over a sorted event list,
//! merging in one derived timer so the replay never polls.
//!
//! # Scope note (MVP, documented rather than silently assumed)
//!
//! This state machine models one global schedule per [`Scenario`], not a
//! fully general multi-principal admission controller: SUSTAIN admits
//! at most one task at a time for the whole scenario (never per-
//! principal), and the windows/holds this module tracks are exactly the
//! tasks *this* simulator started — it does not re-read external ledger
//! evidence. That is the right scope for "plan a deterministic
//! horizon/cooldown schedule for one scenario" (HORO-1765's acceptance
//! criteria); a multi-principal, ledger-integrated scheduler is exactly
//! the live-wiring work HORO-1727 gates (see `docs/adr/0016-sustain-
//! burst-pacing.md`).

use std::collections::{BTreeMap, BTreeSet};

use time::OffsetDateTime;

use crate::quota_window::{next_working_instant, QuotaAmount, QuotaUsage, QuotaWindow};
use crate::reservation::ReservationId;
use crate::resource_amount::ResourceAmount;
use crate::{CompletionContract, EconomicEventId, Policy};

use super::forecast::{earliest_safe_admit, ForecastMode, PendingHold, WindowInput};
use super::{NextAdmit, PacingEvent, PacingPreference, Proposal, SimTaskId, TaskSet};

/// Maximum integer backoff ratio (expressed as `x100`, i.e. 400 == 4x) a
/// principal's overrun may inflate new-start needs to (AC5).
pub const MAX_BACKOFF_RATIO_X100: u64 = 400;

/// A fixed input bundle `step` evaluates against — never mutated by
/// `step` itself, unlike [`PacerState`].
pub struct Scenario<'a> {
    pub tasks: &'a TaskSet,
    pub windows: &'a [QuotaWindow],
    pub contract: Option<&'a CompletionContract>,
    pub preference: PacingPreference,
}

/// One task this state machine has started and not yet completed.
#[derive(Debug, Clone, PartialEq)]
struct ActiveTask {
    principal: String,
    hold: QuotaAmount,
    /// The floor settlement falls back to when no `Spend` was ever
    /// reported — deliberately *not* `hold.value`: `hold` may be
    /// backoff-inflated (a principal currently throttled by an earlier
    /// overrun). Settling at the full inflated `hold` would record that
    /// throttling margin as if it were real spend the task never
    /// actually made, inflating its contribution to every window's
    /// cumulative-usage history. `settlement_floor` is the same need
    /// computed with the backoff ratio forced to 100 (still including
    /// the completion-reserve floor — the ledger's own settle-at-full-
    /// reservation convention).
    settlement_floor: u64,
    #[allow(dead_code)]
    started_at: OffsetDateTime,
}

/// The pacing simulator's own replayable state. `BTreeMap`/`BTreeSet`
/// only (AC4: no `HashMap` — iteration order must never affect the
/// output).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PacerState {
    completed: BTreeSet<SimTaskId>,
    active: BTreeMap<SimTaskId, ActiveTask>,
    /// Per-principal backoff ratio, `x100` (100 == no backoff), capped at
    /// [`MAX_BACKOFF_RATIO_X100`]. Reset to 100 the first time that
    /// principal completes a task within its estimated duration (AC5).
    backoff_x100: BTreeMap<String, u64>,
    /// The instant the most recent task was started (not a projected
    /// completion instant) — `None` until the first `Start`, then set by
    /// every subsequent one and never reset back to `None`. SUSTAIN's
    /// spacing check reads it to enforce a minimum gap between starts
    /// even after a slot frees up early (see `sustain_spacing_is_
    /// enforced_even_when_a_slot_frees_up_early`'s test below).
    last_start: Option<OffsetDateTime>,
    pending_timer: Option<OffsetDateTime>,
    /// Every completed task's settled usage, append-only — this is what
    /// lets a window's *cumulative* spend (not just its currently
    /// concurrent holds) constrain later admits (AC2: "including
    /// simultaneous in-flight work" is about concurrency; this is its
    /// cumulative-spend counterpart, the actual "sustainable pacing"
    /// mechanism). Flat (not per-window): `QuotaWindow::evaluate`
    /// already filters by unit internally, so every window simply reads
    /// the same log.
    usage_log: Vec<QuotaUsage>,
    /// The largest `Spend` amount reported so far for each still-active
    /// task, in that task's hold's own unit (a `Spend` in a different
    /// unit than the hold is ignored for settlement purposes — this
    /// MVP's `ActiveTask` tracks one hold in one unit per task; see
    /// module docs). Consulted at `TaskCompleted` so a task's settled
    /// usage is the larger of its reservation and its reported actual
    /// spend, never silently the reservation alone.
    spend_so_far: BTreeMap<SimTaskId, u64>,
}

impl PacerState {
    pub fn completed(&self) -> &BTreeSet<SimTaskId> {
        &self.completed
    }

    pub fn pending_timer(&self) -> Option<OffsetDateTime> {
        self.pending_timer
    }

    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    fn backoff_for(&self, principal: &str) -> u64 {
        *self.backoff_x100.get(principal).unwrap_or(&100)
    }
}

/// Derives a [`ReservationId`] unique per `(window, task)` pair — a
/// single-tag [`synthetic_uuid`] is only unique per window, which a
/// prior self-review pass in this module found collapses every
/// in-flight task's hold on the same window into one (silently
/// understating how constrained the window really is). Mixes the task's
/// own id into a different byte range than [`synthetic_uuid`] touches,
/// so this derivation and the candidate-need derivation in `forecast`
/// can never collide with each other either.
fn pending_hold_id(
    window_id: crate::quota_window::QuotaWindowId,
    task: SimTaskId,
) -> ReservationId {
    let mut bytes = *window_id.0.as_bytes();
    bytes[0] ^= ACTIVE_HOLD_TAG;
    let task_bytes = task.0.to_be_bytes();
    for (i, b) in task_bytes.iter().enumerate() {
        bytes[4 + i] ^= *b;
    }
    ReservationId(uuid::Uuid::from_bytes(bytes))
}

/// Builds this window's [`PendingHold`] list from every currently active
/// task whose hold shares the window's unit. The injected amount is
/// `hold.value.max(spend_so_far)` — whichever is larger, the reservation
/// recorded at `Start` time or the largest `Spend` this task has actually
/// reported since (HORO-1781 fast-follow: `PacerState::spend_so_far` was
/// already tracked and already read at `TaskCompleted`, but this function
/// used to ignore it entirely, leaving every other candidate's admission
/// check blind to a task that was already known, in-scenario, to be
/// overrunning its hold).
fn pending_for_window(window: &QuotaWindow, state: &PacerState) -> Vec<PendingHold> {
    state
        .active
        .iter()
        .filter(|(_, a)| a.hold.unit == *window.unit())
        .map(|(task_id, a)| {
            let reported_spend = state.spend_so_far.get(task_id).copied().unwrap_or(0);
            let amount = a.hold.value.max(reported_spend);
            PendingHold {
                id: pending_hold_id(window.id(), *task_id),
                amount: QuotaAmount::new(a.hold.unit.clone(), amount),
            }
        })
        .collect()
}

const ACTIVE_HOLD_TAG: u8 = 0x5A;

/// Tries to admit the next ready task(s) at `now`, mutating a working
/// copy of `state` as each admitted task's hold becomes visible to the
/// next candidate in the same pass — exactly the within-step ordering
/// dependency BURST fanout needs (two tasks competing for the same
/// window must see each other's hold).
fn try_admit(
    state: &PacerState,
    now: OffsetDateTime,
    policy: &Policy,
    scenario: &Scenario<'_>,
) -> (PacerState, Vec<Proposal>) {
    let mut working = state.clone();
    let mut proposals = Vec::new();

    let fanout_cap: u32 = match &scenario.preference {
        PacingPreference::Burst { max_fanout, .. } => *max_fanout as u32,
        PacingPreference::Sustain { .. } => 1,
    };

    // Tasks this pass has already decided not to start (backoff-skipped,
    // held for a future instant, or refused outright) — excluded from
    // candidacy so a later, lower-priority ready task still gets a
    // chance in the same pass. A principal's backoff therefore only ever
    // delays *that* principal's new starts, never every other ready
    // task's (a bug an earlier version of this loop had: `break` on the
    // first backing-off candidate stopped the whole pass).
    let mut skip: BTreeSet<SimTaskId> = BTreeSet::new();
    let mut earliest_pending_timer: Option<OffsetDateTime> = None;

    loop {
        if working.active.len() as u32 >= fanout_cap {
            break;
        }
        let ready = super::ready::ready_order(scenario.tasks, &working.completed);
        let candidate = ready
            .iter()
            .find(|t| !working.active.contains_key(&t.id) && !skip.contains(&t.id));
        let Some(task) = candidate else {
            break;
        };

        // SUSTAIN: a backing-off principal's fanout is capped at 1 even
        // when the scenario's own concurrency allows more (AC5) — for
        // SUSTAIN this is already implied by `fanout_cap == 1`, so the
        // check only matters for BURST. Skipping (not breaking) means a
        // different, non-backing-off principal's ready task can still
        // start in this same pass.
        let backoff = working.backoff_for(&task.principal.0);
        if backoff > 100
            && working
                .active
                .values()
                .any(|a| a.principal == task.principal.0)
        {
            skip.insert(task.id);
            continue;
        }

        let mut earliest_at = now;
        // Tracks *why* `earliest_at` moved past `now`, if it did, so a
        // held task's `NextAdmit::Paced` names the real cause instead of
        // a placeholder window.
        let mut pacing_cause: Option<super::PacingCause> = None;
        if let PacingPreference::Sustain {
            working_hours: Some(hours),
            ..
        } = &scenario.preference
        {
            let after_hours = next_working_instant(earliest_at, hours);
            if after_hours > earliest_at {
                earliest_at = after_hours;
                pacing_cause = Some(super::PacingCause::WorkingHours);
            }
        }
        if let Some(last) = working.last_start {
            if let PacingPreference::Sustain { .. } = &scenario.preference {
                if let Some(spacing) = sustain_spacing(scenario, approximate_need(&task.estimate)) {
                    let next_allowed = last.saturating_add(time::Duration::seconds(
                        i64::try_from(spacing).unwrap_or(i64::MAX),
                    ));
                    if next_allowed > earliest_at {
                        earliest_at = next_allowed;
                        pacing_cause = Some(super::PacingCause::Spacing);
                    }
                }
            }
        }

        let mode = match &scenario.preference {
            PacingPreference::Sustain {
                continuity_reserve_bp,
                ..
            } => ForecastMode::Sustain {
                continuity_reserve_bp: *continuity_reserve_bp,
            },
            PacingPreference::Burst { .. } => ForecastMode::Burst,
        };

        let pending_per_window: Vec<Vec<PendingHold>> = scenario
            .windows
            .iter()
            .map(|w| pending_for_window(w, &working))
            .collect();
        let window_inputs: Vec<WindowInput<'_>> = scenario
            .windows
            .iter()
            .zip(pending_per_window.iter())
            .map(|(window, pending)| WindowInput {
                window,
                usage: &working.usage_log,
                pending,
                snapshots: &[],
            })
            .collect();

        let admit = earliest_safe_admit(
            &window_inputs,
            &task.estimate,
            policy,
            scenario.contract,
            None,
            mode,
            backoff,
            100,
            earliest_at,
        );

        // `admit` was computed with `start_at: earliest_at`, so
        // `NextAdmit::Now` means "safe at `earliest_at`", not "safe at
        // the real event time `now`" — these differ whenever SUSTAIN's
        // spacing or working-hours constraint pushed `earliest_at` into
        // the future. A prior version of this loop conflated the two
        // and started every SUSTAIN task at `now`, silently skipping
        // spacing/working-hours entirely. Resolve the actual admit
        // instant first, then compare *that* against `now`.
        let resolved_limiting = match &admit {
            NextAdmit::At { limiting, .. } => Some(*limiting),
            _ => None,
        };
        let resolved_at = match admit {
            NextAdmit::Now => Some(earliest_at),
            NextAdmit::At { at, .. } | NextAdmit::Paced { at, .. } => Some(at),
            NextAdmit::Unavailable(_) => None,
        };

        match resolved_at {
            Some(instant) if instant <= now => {
                let hold = recorded_hold(&task.estimate, scenario, policy, backoff);
                // Backoff ratio forced to 100 (no inflation): a
                // throttled task that never reports a `Spend` must
                // settle at its own real need, never at a backoff-
                // inflated figure that was never actually spent — see
                // `ActiveTask::settlement_floor`'s docs.
                let settlement_floor = recorded_hold(&task.estimate, scenario, policy, 100).value;
                start_task(
                    &mut working,
                    task.id,
                    task.principal.0.clone(),
                    hold.clone(),
                    settlement_floor,
                    now,
                );
                proposals.push(Proposal::Start {
                    task: task.id,
                    at: now,
                    hold,
                    limiting: resolved_limiting,
                });
            }
            Some(instant) => {
                earliest_pending_timer = Some(earlier_timer(earliest_pending_timer, instant));
                // When the delay came from spacing/working-hours alone
                // (forecast itself said `Now`, i.e. no window was ever
                // blocking), report `Paced` with the real cause rather
                // than attributing the delay to a window that was never
                // actually constraining — see `NextAdmit::Paced`'s docs.
                let next = match admit {
                    NextAdmit::At { .. } | NextAdmit::Paced { .. } => admit,
                    _ => match pacing_cause {
                        Some(cause) => NextAdmit::Paced { at: instant, cause },
                        None => admit,
                    },
                };
                proposals.push(Proposal::Hold { next });
                skip.insert(task.id);
            }
            None => {
                proposals.push(Proposal::Hold { next: admit });
                skip.insert(task.id);
            }
        }
    }

    working.pending_timer = earliest_pending_timer;
    (working, proposals)
}

fn earlier_timer(current: Option<OffsetDateTime>, candidate: OffsetDateTime) -> OffsetDateTime {
    match current {
        Some(c) if c <= candidate => c,
        _ => candidate,
    }
}

/// `ceil(need * secs / rate)` for the tightest (largest-spacing) window
/// — the ticket's own spacing formula, not `secs / rate` (which ignores
/// the task's actual need and under-spaces badly for a large task
/// against a high-rate window). Returns `None` when no window yields a
/// usable rate (e.g. an empty window set) — SUSTAIN then relies solely
/// on `earliest_safe_admit` for pacing.
fn sustain_spacing(scenario: &Scenario<'_>, need: u64) -> Option<u64> {
    let mut tightest: Option<u64> = None;
    for window in scenario.windows {
        let rate_per_sec = match window.kind() {
            crate::quota_window::WindowKind::Sliding { length_secs, limit } if *length_secs > 0 => {
                Some((*limit, *length_secs))
            }
            crate::quota_window::WindowKind::RefillBucket {
                refill_amount,
                refill_period_secs,
                ..
            } if *refill_period_secs > 0 => Some((*refill_amount, *refill_period_secs)),
            // A FixedAligned window's "rate" depends on how much of the
            // current period remains, which this spacing heuristic
            // (deliberately simple — real admission safety always comes
            // from `earliest_safe_admit`, never from this spacing
            // number alone) does not attempt to model; skipped here.
            _ => None,
        };
        if let Some((rate, secs)) = rate_per_sec {
            if rate > 0 {
                let numerator = (need as u128).saturating_mul(secs as u128);
                let candidate_spacing = numerator
                    .saturating_add(rate as u128 - 1)
                    .saturating_div(rate as u128);
                let candidate_spacing = u64::try_from(candidate_spacing).unwrap_or(u64::MAX);
                tightest =
                    Some(tightest.map_or(candidate_spacing, |t: u64| t.max(candidate_spacing)));
            }
        }
    }
    tightest
}

/// The task's p90 remaining-resource magnitude, ignoring unit — used
/// only as `sustain_spacing`'s `need` input (a scheduling heuristic);
/// the actual per-window, per-unit need (and admission safety) is always
/// `earliest_safe_admit`'s job, never this function's.
fn approximate_need(estimate: &crate::progressive::RemainingWorkEstimate) -> u64 {
    match &estimate.resource {
        crate::progressive::RemainingResource::Quantiles { p90, .. } => match p90 {
            ResourceAmount::UsdCents(c) => u64::try_from(*c).unwrap_or(0),
            ResourceAmount::Tokens(t) => *t,
            ResourceAmount::QuotaPercent(p) => p.max(0.0) as u64,
        },
        _ => 0,
    }
}

/// Recomputes the same need [`earliest_safe_admit`] already verified
/// safe for every window sharing the task's resource unit, and records
/// the **largest** of them as the recorded hold — never the raw,
/// un-floored p90 estimate. A self-review pass found this exact
/// mismatch: recording the raw estimate while admission actually keyed
/// its decision off a contract-floored/backoff-scaled need meant the
/// recorded reservation could understate what was really required,
/// undermining the whole point of checking it in the first place. Since
/// different windows sharing a unit can want different needs (the
/// continuity-reserve addition is window-limit-scaled), the max across
/// all of them is the only value guaranteed to still satisfy every one.
fn recorded_hold(
    estimate: &crate::progressive::RemainingWorkEstimate,
    scenario: &Scenario<'_>,
    policy: &Policy,
    backoff: u64,
) -> QuotaAmount {
    let windows: Vec<&QuotaWindow> = scenario.windows.iter().collect();
    super::forecast::compute_task_need(
        &windows,
        estimate,
        scenario.contract,
        None,
        policy,
        backoff,
        100,
    )
    .ok()
    .flatten()
    .unwrap_or_else(|| QuotaAmount::new(crate::quota_window::QuotaUnit::Tokens, 0))
}

fn start_task(
    state: &mut PacerState,
    id: SimTaskId,
    principal: String,
    hold: QuotaAmount,
    settlement_floor: u64,
    at: OffsetDateTime,
) {
    state.active.insert(
        id,
        ActiveTask {
            principal,
            hold,
            settlement_floor,
            started_at: at,
        },
    );
    state.last_start = Some(at);
}

/// The pure state transition: applies `event` to `state`, then attempts
/// to admit as many ready tasks as the scenario's concurrency allows.
/// Returns the new state and every [`Proposal`] generated — **never** a
/// cancel/stop proposal (see [`Proposal`]'s own docs: that guarantee is
/// structural, not re-checked here).
pub fn step(
    state: &PacerState,
    event: &PacingEvent,
    policy: &Policy,
    scenario: &Scenario<'_>,
) -> (PacerState, Vec<Proposal>) {
    let mut next = state.clone();
    next.pending_timer = None;

    match event {
        PacingEvent::TaskCompleted { task, at, .. } => {
            if let Some(active) = next.active.remove(task) {
                next.completed.insert(*task);
                let spend = next.spend_so_far.remove(task);
                // Reset on *cost*, not duration: a task that ran long
                // but stayed within its reserved hold never overran
                // anything a backoff should be punishing, and — the
                // real bug an independent review found — a task that
                // spent 4x its hold but happened to finish on time must
                // not have that reset wipe its principal's backoff
                // before it ever reaches a later task. The duration
                // comparison alone (this function's first version) let
                // exactly that happen.
                let within_reservation = spend.unwrap_or(0) <= active.hold.value;
                if within_reservation {
                    next.backoff_x100.insert(active.principal.clone(), 100);
                }
                let settled_value = spend.unwrap_or(0).max(active.settlement_floor);
                let settled_id = EconomicEventId(pending_hold_settlement_uuid(*task));
                if let Ok(usage) = QuotaUsage::new(
                    settled_id,
                    *at,
                    QuotaAmount::new(active.hold.unit, settled_value),
                ) {
                    next.usage_log.push(usage);
                }
            }
        }
        PacingEvent::Spend { task, amount, .. } => {
            if let Some(active) = next.active.get(task) {
                if active.hold.unit == amount.unit {
                    let entry = next.spend_so_far.entry(*task).or_insert(0);
                    *entry = (*entry).max(amount.value);
                }
                if active.hold.unit == amount.unit && amount.value > active.hold.value {
                    let ratio =
                        ceil_div(amount.value, active.hold.value.max(1)).saturating_mul(100);
                    let capped = ratio.clamp(100, MAX_BACKOFF_RATIO_X100);
                    let principal = active.principal.clone();
                    let entry = next.backoff_x100.entry(principal).or_insert(100);
                    *entry = (*entry).max(capped);
                }
                // The active hold/task itself is never touched here —
                // AC5: an overrun never cancels in-flight work. Only
                // *new* starts (via `try_admit`) see the inflated ratio.
            }
        }
        PacingEvent::EstimateRevised { .. }
        | PacingEvent::ModeChanged { .. }
        | PacingEvent::SnapshotIngested { .. } => {
            // Recorded for replay completeness; this MVP scenario has no
            // external evidence store for an estimate/snapshot update to
            // flow into (see module docs) — a live consumer wiring real
            // evidence is exactly HORO-1727's follow-up scope.
        }
    }

    let (admitted_state, proposals) = try_admit(&next, event.at(), policy, scenario);
    (admitted_state, proposals)
}

/// A fixed, arbitrary namespace for [`pending_hold_settlement_uuid`] —
/// never regenerated, the same discipline
/// `economic_event::EconomicEventId::deterministic`'s own namespace
/// constant follows.
const SETTLEMENT_NAMESPACE: uuid::Uuid = uuid::Uuid::from_bytes([
    0x1a, 0x2b, 0x3c, 0x4d, 0x5e, 0x6f, 0x70, 0x81, 0x92, 0xa3, 0xb4, 0xc5, 0xd6, 0xe7, 0xf8, 0x09,
]);

/// Derives a settlement record's id from the namespace above and the
/// task id alone — deliberately *not* also mixing in the completion
/// instant: a `SimTaskId` is removed from `active` the moment it
/// completes and `ready_order` excludes every completed task forever
/// (see `TaskSet`/`ready`), so a given task settles at most once per
/// scenario, which makes the task id alone already unique. Mixing in a
/// second field via a wrapping `% 16` byte range (an earlier version of
/// this function did) only reintroduces a collision opportunity between
/// two different `(task, at)` pairs for no benefit.
fn pending_hold_settlement_uuid(task: SimTaskId) -> uuid::Uuid {
    let mut bytes = *SETTLEMENT_NAMESPACE.as_bytes();
    let task_bytes = task.0.to_be_bytes();
    for (i, b) in task_bytes.iter().enumerate() {
        bytes[i] ^= *b;
    }
    uuid::Uuid::from_bytes(bytes)
}

fn ceil_div(a: u64, b: u64) -> u64 {
    if b == 0 {
        return a;
    }
    a.saturating_add(b - 1).saturating_div(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::business_context::Priority;
    use crate::economic_attribution::PrincipalId;
    use crate::pacing::tests::test_estimate;
    use crate::pacing::{SimTask, SimTaskId, TaskSet};
    use crate::progressive::{RemainingDuration, RemainingResource};
    use crate::quota_window::{
        EntitlementSource, QuotaScope, QuotaSubject, QuotaUnit, QuotaWindowId, WindowKind,
    };
    use crate::resource_amount::{ResourceAmount, ResourceKind};
    use crate::Confidence;

    fn window(limit: u64, length_secs: u64) -> QuotaWindow {
        QuotaWindow::validated(
            QuotaWindowId::new(),
            QuotaScope {
                subject: QuotaSubject::Principal(PrincipalId("p".to_string())),
                source: EntitlementSource::OperatorConfigured,
                confidence: Confidence::High,
                observed_at: OffsetDateTime::UNIX_EPOCH,
                valid_until: None,
            },
            QuotaUnit::Tokens,
            WindowKind::Sliding { length_secs, limit },
        )
        .unwrap()
    }

    fn task_with_tokens(id: u32, tokens: u64, duration_secs: u64) -> SimTask {
        let mut e = test_estimate();
        e.confidence = Confidence::High;
        e.resource = RemainingResource::Quantiles {
            kind: ResourceKind::Tokens,
            p50: ResourceAmount::Tokens(tokens),
            p80: ResourceAmount::Tokens(tokens),
            p90: ResourceAmount::Tokens(tokens),
            conditional_n: 20,
            weakest_truth: crate::economic_event::TruthStrength::Metered,
        };
        e.duration = RemainingDuration::Quantiles {
            p50_secs: duration_secs,
            p80_secs: duration_secs,
            p90_secs: duration_secs,
            conditional_n: 20,
        };
        SimTask {
            id: SimTaskId(id),
            principal: PrincipalId("p".to_string()),
            depends_on: vec![],
            priority: Priority::Normal,
            deadline: None,
            estimate: e,
        }
    }

    fn policy() -> Policy {
        super::super::forecast::tests_support::policy()
    }

    #[test]
    fn burst_starts_ready_tasks_up_to_fanout() {
        let tasks = TaskSet::validated(vec![
            task_with_tokens(1, 100, 60),
            task_with_tokens(2, 100, 60),
            task_with_tokens(3, 100, 60),
        ])
        .unwrap();
        let w = window(1000, 21_600);
        let scenario = Scenario {
            tasks: &tasks,
            windows: std::slice::from_ref(&w),
            contract: None,
            preference: PacingPreference::Burst {
                target_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
                max_fanout: 2,
            },
        };
        let state = PacerState::default();
        let event = PacingEvent::ModeChanged {
            at: OffsetDateTime::UNIX_EPOCH,
            seq: 0,
        };
        let (new_state, proposals) = step(&state, &event, &policy(), &scenario);
        let starts = proposals
            .iter()
            .filter(|p| matches!(p, Proposal::Start { .. }))
            .count();
        assert_eq!(starts, 2);
        assert_eq!(new_state.active.len(), 2);
    }

    /// Regression test for a real bug a self-review pass found in this
    /// module: deriving each in-flight task's hold id from the window
    /// alone (ignoring the task) made every active task's hold on a
    /// given window collapse to the same id, which
    /// `quota_window::dedup_holds_by_id` then silently deduplicated to
    /// one — understating how constrained the window really was. With
    /// 11 ready tasks at 100 tokens each against a 1000-token sliding
    /// limit and no fanout cap, exactly 9 fit (9*100 = 900, a 10th would
    /// push outstanding to 1000 and `remaining <= 0` blocks); the bug
    /// this guards against would have let all 11 start.
    #[test]
    fn distinct_tasks_get_distinct_hold_ids_so_concurrent_holds_genuinely_stack() {
        let many: Vec<SimTask> = (0..11).map(|i| task_with_tokens(i, 100, 60)).collect();
        let tasks = TaskSet::validated(many).unwrap();
        let w = window(1000, 21_600);
        let scenario = Scenario {
            tasks: &tasks,
            windows: std::slice::from_ref(&w),
            contract: None,
            preference: PacingPreference::Burst {
                target_end: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1),
                max_fanout: 20,
            },
        };
        let state = PacerState::default();
        let event = PacingEvent::ModeChanged {
            at: OffsetDateTime::UNIX_EPOCH,
            seq: 0,
        };
        let (new_state, _proposals) = step(&state, &event, &policy(), &scenario);
        assert_eq!(
            new_state.active_count(),
            9,
            "exactly 9 of 11 tasks at 100 tokens must fit under a 1000-token sliding limit"
        );
    }

    #[test]
    fn sustain_never_cancels_on_overrun() {
        let tasks = TaskSet::validated(vec![task_with_tokens(1, 100, 60)]).unwrap();
        let w = window(1000, 21_600);
        let scenario = Scenario {
            tasks: &tasks,
            windows: std::slice::from_ref(&w),
            contract: None,
            preference: PacingPreference::Sustain {
                horizon_secs: 21_600,
                working_hours: None,
                continuity_reserve_bp: 0,
            },
        };
        let state = PacerState::default();
        let start_event = PacingEvent::ModeChanged {
            at: OffsetDateTime::UNIX_EPOCH,
            seq: 0,
        };
        let (state_after_start, proposals) = step(&state, &start_event, &policy(), &scenario);
        assert!(proposals
            .iter()
            .any(|p| matches!(p, Proposal::Start { .. })));
        assert_eq!(state_after_start.active.len(), 1);

        let overrun_event = PacingEvent::Spend {
            at: OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(10),
            seq: 1,
            task: SimTaskId(1),
            amount: QuotaAmount::new(QuotaUnit::Tokens, 500),
        };
        let (state_after_overrun, proposals2) =
            step(&state_after_start, &overrun_event, &policy(), &scenario);
        // AC5: no proposal is ever a cancel — `Proposal` has only
        // `Start`/`Hold` variants (enforced by the type itself, not a
        // runtime check); `proposals2` below is exhaustively one or the
        // other, which this match would fail to compile against a
        // hypothetical third "cancel" variant if one were ever added.
        for p in &proposals2 {
            match p {
                Proposal::Start { .. } | Proposal::Hold { .. } => {}
            }
        }
        assert_eq!(
            state_after_overrun.active.len(),
            1,
            "the active task must remain active across an overrun"
        );
        assert!(
            state_after_overrun
                .backoff_x100
                .get("p")
                .copied()
                .unwrap_or(100)
                > 100
        );
    }

    /// Regression test for a real bug an independent review found: the
    /// admission loop compared `NextAdmit::Now` against the real event
    /// time `now` instead of against `earliest_at` (the spacing/working-
    /// hours floor actually passed to `earliest_safe_admit` as its
    /// `start_at`), so a SUSTAIN task that completed early let the
    /// *next* task start immediately too — silently skipping spacing
    /// entirely. need=100, 6h sliding limit=1000 gives spacing =
    /// ceil(100 * 21600 / 1000) = 2160s. Task 1 starts at t0; it
    /// completes at t0+60 (well inside its own estimate, so no
    /// backoff); task 2 must still wait until t0+2160, not start at
    /// t0+60 just because a concurrency slot freed up.
    #[test]
    fn sustain_spacing_is_enforced_even_when_a_slot_frees_up_early() {
        let tasks = TaskSet::validated(vec![
            task_with_tokens(1, 100, 60),
            task_with_tokens(2, 100, 60),
        ])
        .unwrap();
        let w = window(1000, 21_600);
        let scenario = Scenario {
            tasks: &tasks,
            windows: std::slice::from_ref(&w),
            contract: None,
            preference: PacingPreference::Sustain {
                horizon_secs: 21_600,
                working_hours: None,
                continuity_reserve_bp: 0,
            },
        };
        let t0 = OffsetDateTime::UNIX_EPOCH;
        let events = vec![
            PacingEvent::ModeChanged { at: t0, seq: 0 },
            PacingEvent::TaskCompleted {
                at: t0 + time::Duration::seconds(60),
                seq: 1,
                task: SimTaskId(1),
            },
        ];
        let (_, trace) = crate::pacing::simulate::simulate(&events, &policy(), &scenario);

        let task2_start_at = trace
            .iter()
            .flat_map(|(_, proposals)| proposals.iter())
            .find_map(|p| match p {
                Proposal::Start { task, at, .. } if *task == SimTaskId(2) => Some(*at),
                _ => None,
            })
            .expect("task 2 must eventually start");

        assert_eq!(
            task2_start_at,
            t0 + time::Duration::seconds(2160),
            "task 2 must wait the full spacing interval from task 1's start, \
             not start the instant task 1 completes and frees a slot"
        );
    }
}
