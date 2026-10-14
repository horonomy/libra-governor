//! Confirms the false-throttle duration metric is real elapsed time
//! between log entries, never a per-sighting count and never a naive
//! first-seen-to-last-seen bracket across only `Offered` sightings — see
//! `src/verify.rs`'s own module docs ("Elapsed-time intervals, not entry
//! counts or naive brackets").

use time::OffsetDateTime;

use libra_governor_pacing_gate::drive::run_trace;
use libra_governor_pacing_gate::drive::DriveLogEntry;
use libra_governor_pacing_gate::fixtures;
use libra_governor_pacing_gate::strategy::{Capability, Strategy};
use libra_governor_pacing_gate::trace::{
    ExogenousEvent, TracePreference, TraceSpec, TraceTask, TraceUnit, WindowSpec, WindowSpecKind,
};
use libra_governor_pacing_gate::verify::{verify, StrategyMeta};

/// A strategy that never starts anything, regardless of `ready` — used
/// ONLY to force a task to be re-offered and held across multiple
/// distinct driver events, independent of either built-in strategy's own
/// quota-awareness (both `StaticBudget` and `Paced` happen to actually
/// START `t3_opaque_quota`'s only task immediately, each for its own
/// documented reason — `StaticBudget` because it is blind to quota
/// entirely, `Paced` because it never sees `ForeignSpend`/
/// `QuotaSnapshotTold` at all).
struct NeverStarts;

impl Strategy for NeverStarts {
    fn name(&self) -> &str {
        "never_starts"
    }
    fn capability(&self) -> Capability {
        Capability::LiveCapable
    }
    fn decide(&mut self, _now: OffsetDateTime, _ready: &[u32]) -> Vec<u32> {
        Vec::new()
    }
}

/// A strategy that holds every ready task until wall-clock `now` reaches
/// `threshold`, then admits everything at once — used to construct a
/// precise, deterministic "eligible from the start, starts much later"
/// gap with a known duration, which no existing fixture happens to
/// produce under either built-in strategy.
struct WaitsUntil {
    threshold: OffsetDateTime,
}

impl Strategy for WaitsUntil {
    fn name(&self) -> &str {
        "waits_until"
    }
    fn capability(&self) -> Capability {
        Capability::LiveCapable
    }
    fn decide(&mut self, now: OffsetDateTime, ready: &[u32]) -> Vec<u32> {
        if now >= self.threshold {
            ready.to_vec()
        } else {
            Vec::new()
        }
    }
}

/// One task, ample quota, and a single no-op `PreferenceChanged` event
/// 100 seconds in — purely to force the driver to re-poll at exactly
/// `t=100` (every exogenous event, of any kind, triggers a fresh
/// `ready`/`decide`/`Offered` check regardless of what it does to the
/// world — see `drive::run_trace`'s own docs). `WaitsUntil` holds the
/// task from `t=0` until that poll, at which point real headroom has
/// been continuously available the whole time.
fn single_task_spec(threshold_offset_secs: u64) -> TraceSpec {
    TraceSpec {
        name: "synthetic_single_task_gap".to_string(),
        description: "hand-built (not a fixture file): one task, ample quota, used only to pin \
                      an exact known false-throttle gap duration"
            .to_string(),
        initial_preference: TracePreference::Burst {
            target_end_offset_secs: 3600,
            max_fanout: 1,
        },
        windows: vec![WindowSpec {
            name: "ample".to_string(),
            unit: TraceUnit::Tokens,
            kind: WindowSpecKind::Sliding {
                length_secs: 3600,
                limit: 1000,
            },
        }],
        tasks: vec![TraceTask {
            id: 1,
            principal: "p".to_string(),
            depends_on: vec![],
            priority: Default::default(),
            relative_duration_secs: 10,
            relative_spend: 10,
            unit: TraceUnit::Tokens,
            overrun_spend: None,
        }],
        events: vec![ExogenousEvent::PreferenceChanged {
            offset_secs: threshold_offset_secs,
            preference: TracePreference::Burst {
                target_end_offset_secs: 3600,
                max_fanout: 1,
            },
        }],
    }
}

/// The core regression: a task eligible (real headroom available) from
/// `t=0`, sighted held at `t=0`, and not started until `t=100` — a naive
/// first-seen-to-last-seen bracket across only `Offered` sightings of
/// this task would see exactly ONE held sighting (at `t=0`; the `t=100`
/// sighting shows it started, not held) and score 0 seconds. The correct
/// answer is 100 seconds: the task was genuinely, continuously
/// false-throttled the entire gap, and that gap is exactly
/// `involuntary_wait_secs` too (the two must agree here, since nothing
/// BUT the false throttle caused this task to wait).
#[test]
fn false_throttle_seconds_reflects_the_full_gap_to_a_later_start_not_a_single_sighting() {
    let spec = single_task_spec(100);
    let strategy = WaitsUntil {
        threshold: libra_governor_pacing_gate::trace::TRACE_EPOCH + time::Duration::seconds(100),
    };
    let meta = StrategyMeta::of(&strategy);
    let outcome = run_trace(&spec, Box::new(strategy)).unwrap();

    let held_sightings_of_task_1 = outcome
        .log
        .iter()
        .filter(|(_, e)| {
            matches!(e, DriveLogEntry::Offered { ready, started, .. } if ready.contains(&1) && !started.contains(&1))
        })
        .count();
    assert_eq!(
        held_sightings_of_task_1, 1,
        "expected exactly one held sighting at t=0 — a naive bracket-across-sightings approach \
         would score 0 seconds from this alone, which is the bug this test guards against"
    );

    let report = verify(&spec, meta, &outcome);
    assert!(outcome.world.is_completed(1));
    let task1 = report.per_task.iter().find(|t| t.task == 1).unwrap();
    assert_eq!(
        task1.involuntary_wait_secs(),
        Some(100),
        "sanity: the task really did wait 100 seconds before starting"
    );
    assert_eq!(
        task1.false_throttle_secs, 100,
        "false_throttle_secs must reflect the full 100-second gap during which real headroom \
         was continuously available, not the single held sighting at t=0: {:?}",
        task1
    );
    assert!(report.hard_limit_violations.is_empty());
}

/// A task that becomes eligible and is NEVER started before the trace
/// ends must be flagged `stranded_with_headroom`, not silently scored
/// with whatever partial duration happened to accumulate before the
/// trace's own end (the true remaining duration past that point is
/// genuinely unknowable).
#[test]
fn a_task_still_eligible_at_trace_end_is_flagged_stranded_not_silently_zero() {
    let spec = fixtures::load("t6_overrun_backoff").unwrap();
    let strategy = libra_governor_pacing_gate::strategy::Paced::new(
        &spec.tasks,
        &spec.windows,
        &spec.initial_preference,
    );
    let meta = StrategyMeta::of(&strategy);
    let outcome = run_trace(&spec, Box::new(strategy)).unwrap();
    let report = verify(&spec, meta, &outcome);

    let task2 = report.per_task.iter().find(|t| t.task == 2).unwrap();
    assert!(!outcome.world.is_completed(2));
    assert!(
        task2.stranded_with_headroom,
        "task 2 is genuinely eligible at its last (only) sighting and never starts — this must \
         be visible as stranded_with_headroom, not just a quiet false_throttle_secs=0: {:?}",
        task2
    );
    assert_eq!(report.stranded_with_headroom_count, 1);
}

/// `t3_opaque_quota` declares two exogenous events (a `ForeignSpend` at
/// the epoch, a `QuotaSnapshotTold` 5 seconds later) — `drive::run_trace`
/// recomputes `ready`/`decide` after EVERY queue item regardless of
/// kind, so a strategy that never starts task 1 sees it re-offered at
/// least twice. Confirms repeated sightings of a task that is NOT
/// eligible (a real, persistent quota shortfall — 380 foreign + 150 own
/// = 530 > 400) never get credited as false-throttle time no matter how
/// many times it is re-offered.
#[test]
fn repeated_sightings_of_a_genuinely_ineligible_task_credit_zero_seconds() {
    let spec = fixtures::load("t3_opaque_quota").unwrap();
    let strategy = NeverStarts;
    let meta = StrategyMeta::of(&strategy);
    let outcome = run_trace(&spec, Box::new(strategy)).unwrap();

    let offered_sightings_of_task_1 = outcome
        .log
        .iter()
        .filter(|(_, e)| matches!(e, DriveLogEntry::Offered { ready, .. } if ready.contains(&1)))
        .count();
    assert!(
        offered_sightings_of_task_1 >= 2,
        "expected task 1 to be offered at least twice (epoch + the snapshot event) — got {}",
        offered_sightings_of_task_1
    );

    let report = verify(&spec, meta, &outcome);
    assert_eq!(
        report.false_throttle_task_count, 0,
        "task 1's hold is a real, persistent quota shortfall, not a false throttle: {:?}",
        report.per_task
    );
    let task1 = report.per_task.iter().find(|t| t.task == 1).unwrap();
    assert_eq!(task1.false_throttle_secs, 0);
    assert!(
        !outcome.world.is_completed(1),
        "task 1 never gets enough headroom to start"
    );
}
