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

use crate::quota_window::{
    next_working_instant, OutstandingHold, QuotaAmount, QuotaEvidence, QuotaWindow,
};
use crate::reservation::ReservationId;
use crate::{CompletionContract, Policy};

use super::forecast::{earliest_safe_admit, ForecastMode, WindowInput};
use super::{
    synthetic_uuid, NextAdmit, PacingEvent, PacingPreference, Proposal, SimTaskId, TaskSet,
};

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
    #[allow(dead_code)]
    started_at: OffsetDateTime,
    projected_complete_at: OffsetDateTime,
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
    /// `Some` only while SUSTAIN holds the schedule's one concurrency
    /// slot — the instant the active task is projected to complete.
    last_start: Option<OffsetDateTime>,
    pending_timer: Option<OffsetDateTime>,
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

fn window_evidence<'a>(
    window: &'a QuotaWindow,
    state: &PacerState,
    holds_buf: &'a mut Vec<OutstandingHold>,
) -> (QuotaEvidence<'a>, Vec<OffsetDateTime>) {
    let mut completions = Vec::new();
    for active in state.active.values() {
        if active.hold.unit == *window.unit() {
            let id = ReservationId(synthetic_uuid(window.id().0, ACTIVE_HOLD_TAG));
            holds_buf.push(OutstandingHold::projected(id, active.hold.clone()));
            completions.push(active.projected_complete_at);
        }
    }
    completions.sort();
    (
        QuotaEvidence {
            usage: &[],
            holds: holds_buf,
            snapshots: &[],
        },
        completions,
    )
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

    loop {
        if working.active.len() as u32 >= fanout_cap {
            break;
        }
        let ready = super::ready::ready_order(scenario.tasks, &working.completed);
        let candidate = ready.iter().find(|t| !working.active.contains_key(&t.id));
        let Some(task) = candidate else {
            break;
        };

        // SUSTAIN: a backing-off principal's fanout is capped at 1 even
        // when the scenario's own concurrency allows more (AC5) — for
        // SUSTAIN this is already implied by `fanout_cap == 1`, so the
        // check only matters for BURST.
        let backoff = working.backoff_for(&task.principal.0);
        if backoff > 100
            && working
                .active
                .values()
                .any(|a| a.principal == task.principal.0)
        {
            break;
        }

        let mut earliest_at = now;
        if let PacingPreference::Sustain {
            working_hours: Some(hours),
            ..
        } = &scenario.preference
        {
            earliest_at = next_working_instant(earliest_at, hours);
        }
        if let Some(last) = working.last_start {
            if let PacingPreference::Sustain { .. } = &scenario.preference {
                if let Some(spacing) = sustain_spacing(scenario, &working, now) {
                    let next_allowed = last.saturating_add(time::Duration::seconds(
                        i64::try_from(spacing).unwrap_or(i64::MAX),
                    ));
                    if next_allowed > earliest_at {
                        earliest_at = next_allowed;
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

        let mut window_inputs_holds: Vec<Vec<OutstandingHold>> =
            vec![Vec::new(); scenario.windows.len()];
        let mut window_inputs = Vec::with_capacity(scenario.windows.len());
        let mut completions_per_window = Vec::with_capacity(scenario.windows.len());
        for (window, holds_buf) in scenario.windows.iter().zip(window_inputs_holds.iter_mut()) {
            let (_, completions) = window_evidence(window, &working, holds_buf);
            completions_per_window.push(completions);
        }
        for (i, window) in scenario.windows.iter().enumerate() {
            window_inputs.push(WindowInput {
                window,
                evidence: QuotaEvidence {
                    usage: &[],
                    holds: &window_inputs_holds[i],
                    snapshots: &[],
                },
                hold_completions: &completions_per_window[i],
            });
        }

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

        match admit {
            NextAdmit::Now => {
                start_task(
                    &mut working,
                    task.id,
                    task.principal.0.clone(),
                    &task.estimate,
                    now,
                );
                proposals.push(Proposal::Start {
                    task: task.id,
                    at: now,
                    hold: representative_hold(&working, task.id),
                    limiting: None,
                });
            }
            NextAdmit::At { at, limiting } if at <= now => {
                start_task(
                    &mut working,
                    task.id,
                    task.principal.0.clone(),
                    &task.estimate,
                    now,
                );
                proposals.push(Proposal::Start {
                    task: task.id,
                    at: now,
                    hold: representative_hold(&working, task.id),
                    limiting: Some(limiting),
                });
            }
            NextAdmit::At { at, .. } => {
                working.pending_timer = Some(earlier_timer(working.pending_timer, at));
                proposals.push(Proposal::Hold { next: admit });
                break;
            }
            NextAdmit::Unavailable(_) => {
                proposals.push(Proposal::Hold { next: admit });
                break;
            }
        }
    }

    (working, proposals)
}

fn representative_hold(state: &PacerState, task: SimTaskId) -> QuotaAmount {
    state
        .active
        .get(&task)
        .map(|a| a.hold.clone())
        .unwrap_or(QuotaAmount::new(crate::quota_window::QuotaUnit::Tokens, 0))
}

fn earlier_timer(current: Option<OffsetDateTime>, candidate: OffsetDateTime) -> OffsetDateTime {
    match current {
        Some(c) if c <= candidate => c,
        _ => candidate,
    }
}

/// The tightest window's integer rate, used only to space successive
/// SUSTAIN starts (`ceil(need * secs / amount)`), per the ticket's
/// design. Returns `None` when no window yields a usable rate (e.g. an
/// empty window set) — SUSTAIN then relies solely on `earliest_safe_admit`
/// for pacing.
fn sustain_spacing(
    scenario: &Scenario<'_>,
    _state: &PacerState,
    _now: OffsetDateTime,
) -> Option<u64> {
    let mut tightest: Option<u64> = None;
    for window in scenario.windows {
        let rate_per_sec = match window.kind() {
            crate::quota_window::WindowKind::Sliding { length_secs, limit } if *length_secs > 0 => {
                Some((*limit, *length_secs))
            }
            crate::quota_window::WindowKind::FixedAligned { limit, .. } => Some((*limit, 1)),
            crate::quota_window::WindowKind::RefillBucket {
                refill_amount,
                refill_period_secs,
                ..
            } if *refill_period_secs > 0 => Some((*refill_amount, *refill_period_secs)),
            _ => None,
        };
        if let Some((amount, secs)) = rate_per_sec {
            if amount > 0 {
                let candidate_spacing = secs.saturating_div(amount.max(1));
                tightest =
                    Some(tightest.map_or(candidate_spacing, |t: u64| t.max(candidate_spacing)));
            }
        }
    }
    tightest
}

fn start_task(
    state: &mut PacerState,
    id: SimTaskId,
    principal: String,
    estimate: &crate::progressive::RemainingWorkEstimate,
    at: OffsetDateTime,
) {
    let (hold, duration_secs) = match &estimate.resource {
        crate::progressive::RemainingResource::Quantiles { p90, .. } => {
            let hold = QuotaAmount::try_from(*p90)
                .unwrap_or(QuotaAmount::new(crate::quota_window::QuotaUnit::Tokens, 0));
            let duration = match estimate.duration {
                crate::progressive::RemainingDuration::Quantiles { p90_secs, .. } => p90_secs,
                crate::progressive::RemainingDuration::Insufficient { .. } => 0,
            };
            (hold, duration)
        }
        _ => (
            QuotaAmount::new(crate::quota_window::QuotaUnit::Tokens, 0),
            0,
        ),
    };
    let projected_complete_at = at.saturating_add(time::Duration::seconds(
        i64::try_from(duration_secs).unwrap_or(i64::MAX),
    ));
    state.active.insert(
        id,
        ActiveTask {
            principal,
            hold,
            started_at: at,
            projected_complete_at,
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
                let within_estimate = *at <= active.projected_complete_at;
                if within_estimate {
                    next.backoff_x100.insert(active.principal, 100);
                }
            }
        }
        PacingEvent::Spend { task, amount, .. } => {
            if let Some(active) = next.active.get(task) {
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
}
