//! Proves `verify::verify`'s checks actually discriminate: each
//! negative-control mutant is run against a fixture built to expose
//! exactly its own defect, and paired with an honest strategy on the
//! SAME fixture that is NOT flagged for that class of violation — see
//! each mutant's own doc comment in `src/mutants.rs` for why each
//! fixture/pairing was chosen.

use time::OffsetDateTime;

use libra_governor_pacing_gate::drive::run_trace;
use libra_governor_pacing_gate::fixtures;
use libra_governor_pacing_gate::mutants::{ForeignSession, ForgetfulBaseline, Unguarded};
use libra_governor_pacing_gate::strategy::{Capability, DriveEvent, Paced, StaticBudget, Strategy};
use libra_governor_pacing_gate::trace::TraceTask;
use libra_governor_pacing_gate::verify::{verify, StrategyMeta};

/// A minimal honest, principal-scoped strategy: behaves like
/// `StaticBudget` but only ever starts a task it knows is owned by its
/// own claimed principal — used ONLY as this test's positive control for
/// the `ForeignSession` check, never shipped in `src/mutants.rs` (it has
/// no other purpose in this crate).
struct ScopedStaticBudget {
    owning_principal: String,
    principal_of: std::collections::BTreeMap<u32, String>,
    max_concurrent: usize,
    active: usize,
}

impl ScopedStaticBudget {
    fn new(owning_principal: &str, tasks: &[TraceTask], max_concurrent: usize) -> Self {
        Self {
            owning_principal: owning_principal.to_string(),
            principal_of: tasks.iter().map(|t| (t.id, t.principal.clone())).collect(),
            max_concurrent,
            active: 0,
        }
    }
}

impl Strategy for ScopedStaticBudget {
    fn name(&self) -> &str {
        "scoped_static_budget"
    }
    fn capability(&self) -> Capability {
        Capability::LiveCapable
    }
    fn owning_principal(&self) -> Option<&str> {
        Some(&self.owning_principal)
    }
    fn decide(&mut self, _now: OffsetDateTime, ready: &[u32]) -> Vec<u32> {
        let mut started = Vec::new();
        for &task in ready {
            if self.active >= self.max_concurrent {
                break;
            }
            if self.principal_of.get(&task).map(String::as_str)
                != Some(self.owning_principal.as_str())
            {
                continue;
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

// ---------------------------------------------------------------------
// Unguarded vs. hard limits, on x0_unguarded.
// ---------------------------------------------------------------------

#[test]
fn unguarded_is_caught_violating_the_hard_limit_on_x0() {
    let spec = fixtures::load("x0_unguarded").unwrap();
    let strategy = Unguarded;
    let meta = StrategyMeta::of(&strategy);
    let outcome = run_trace(&spec, Box::new(strategy)).unwrap();
    let report = verify(&spec, meta, &outcome);
    assert!(
        !report.hard_limit_violations.is_empty(),
        "Unguarded must be caught starting every ready task at once, oversubscribing x0's \
         120-token window with 5x100=500 tokens"
    );
}

#[test]
fn static_budget_concurrency_cap_alone_is_not_enough_to_respect_x0s_quota() {
    // Deliberately NOT this test's positive control (see
    // `paced_respects_the_hard_limit_on_x0` for that): x0's window never
    // resets within the trace's own duration (`length_secs` is 21600,
    // the whole trace takes well under 300 seconds), so starting even a
    // SECOND 100-token task — sequentially, not concurrently, after the
    // first has already settled — still cumulatively oversubscribes the
    // 120-token window. `StaticBudget` enforces only concurrency, never
    // quota, so it cannot avoid this: confirming here that it genuinely
    // DOES still get flagged is itself a real, honest finding (matching
    // `strategy.rs`'s own documented claim that `StaticBudget` is blind
    // to quota), not a check bug.
    let spec = fixtures::load("x0_unguarded").unwrap();
    let strategy = StaticBudget::new(1);
    let meta = StrategyMeta::of(&strategy);
    let outcome = run_trace(&spec, Box::new(strategy)).unwrap();
    let report = verify(&spec, meta, &outcome);
    assert!(
        !report.hard_limit_violations.is_empty(),
        "StaticBudget(1) is expected to still violate x0's quota by sequencing through more \
         than one task within the window's own 21600s decay horizon — if this now passes, \
         either the window's capacity or this fixture's task count/spend changed, and this \
         test's own reasoning needs revisiting"
    );
}

#[test]
fn paced_respects_the_hard_limit_on_x0() {
    // The real positive control: `Paced` is quota-aware (it wraps real
    // `pacing::step` admission, which checks window headroom before
    // admitting), so it is the one strategy in this crate that can
    // actually respect x0's tiny window rather than merely limiting
    // concurrency — confirming it is NOT flagged is what proves the
    // hard-limit check discriminates a real quota-aware strategy from
    // `Unguarded`, rather than flagging everything on this fixture.
    let spec = fixtures::load("x0_unguarded").unwrap();
    let strategy = Paced::new(&spec.tasks, &spec.windows, &spec.initial_preference);
    let meta = StrategyMeta::of(&strategy);
    let outcome = run_trace(&spec, Box::new(strategy)).unwrap();
    let report = verify(&spec, meta, &outcome);
    assert!(
        report.hard_limit_violations.is_empty(),
        "a quota-aware strategy must not be flagged: {:?}",
        report.hard_limit_violations
    );
}

// ---------------------------------------------------------------------
// ForeignSession vs. principal boundary, on x1_foreign_session.
// ---------------------------------------------------------------------

#[test]
fn foreign_session_is_caught_starting_another_principals_task_on_x1() {
    let spec = fixtures::load("x1_foreign_session").unwrap();
    let strategy = ForeignSession::new("mallory");
    let meta = StrategyMeta::of(&strategy);
    let outcome = run_trace(&spec, Box::new(strategy)).unwrap();
    let report = verify(&spec, meta, &outcome);
    assert!(
        !report.principal_violations.is_empty(),
        "ForeignSession claims to act only for 'mallory' but must be caught starting task 2, \
         owned by 'victim'"
    );
}

#[test]
fn scoped_strategy_never_starts_another_principals_task_on_x1() {
    let spec = fixtures::load("x1_foreign_session").unwrap();
    let strategy = ScopedStaticBudget::new("mallory", &spec.tasks, 2);
    let meta = StrategyMeta::of(&strategy);
    let outcome = run_trace(&spec, Box::new(strategy)).unwrap();
    let report = verify(&spec, meta, &outcome);
    assert!(
        report.principal_violations.is_empty(),
        "an honest, principal-scoped strategy must not be flagged: {:?}",
        report.principal_violations
    );
    // Confirms the positive control is meaningful, not vacuous: it
    // really did start its own task and really did leave the other
    // principal's task alone (never started at all), rather than the
    // check passing only because nothing was ever decided.
    assert!(
        outcome.world.is_completed(1),
        "mallory's own task 1 should complete"
    );
    assert!(
        !outcome.world.is_completed(2),
        "victim's task 2 must never be started by a strategy honestly scoped to mallory"
    );
}

// ---------------------------------------------------------------------
// ForgetfulBaseline vs. false-throttle/incompleteness, on t0_occupancy.
// ---------------------------------------------------------------------

#[test]
fn forgetful_baseline_is_caught_leaking_capacity_accounting_on_t0() {
    let spec = fixtures::load("t0_occupancy").unwrap();
    let strategy = ForgetfulBaseline::new(1);
    let meta = StrategyMeta::of(&strategy);
    let outcome = run_trace(&spec, Box::new(strategy)).unwrap();
    let report = verify(&spec, meta, &outcome);
    assert!(
        report.tasks_completed < report.tasks_total,
        "ForgetfulBaseline must leave work incomplete after its first task (never notices the \
         completion that would free its one slot): completed {}/{}",
        report.tasks_completed,
        report.tasks_total
    );
    assert!(
        report.false_throttle_task_count > 0,
        "the tasks ForgetfulBaseline stranded must show up as false-throttled: the 1000-token \
         window has ample true headroom for them, so the hold is accounting leakage, not quota \
         scarcity"
    );
}

#[test]
fn static_budget_with_no_concurrency_cap_never_false_throttles_on_t0() {
    // max_concurrent == tasks_total: this honest strategy never needs
    // its own completion-notify at all, so it structurally cannot
    // exhibit ForgetfulBaseline's bug — the positive control for the
    // check above, on the very same fixture.
    let spec = fixtures::load("t0_occupancy").unwrap();
    let strategy = StaticBudget::new(spec.tasks.len());
    let meta = StrategyMeta::of(&strategy);
    let outcome = run_trace(&spec, Box::new(strategy)).unwrap();
    let report = verify(&spec, meta, &outcome);
    assert_eq!(
        report.tasks_completed, report.tasks_total,
        "an honest strategy with no binding concurrency cap must complete every task"
    );
    assert_eq!(
        report.false_throttle_task_count, 0,
        "an honest strategy that never holds anything back must show zero false throttles: {:?}",
        report.per_task
    );
}
