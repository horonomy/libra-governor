//! The closed-loop driver (HORO-1768 PR-A).
//!
//! [`run_trace`] replays a frozen [`crate::trace::TraceSpec`]'s workload
//! against a pluggable [`crate::strategy::Strategy`], never against
//! `pacing::simulate::simulate`'s own replay loop — see `trace.rs`'s
//! module docs for why that function cannot cross strategies.
//!
//! # How this avoids `simulate()`'s absolute-timestamp problem
//!
//! Every instant this driver ever visits is *derived*, never read off a
//! fixture: an `ExogenousEvent`'s instant is `TRACE_EPOCH + offset_secs`
//! (fixed, strategy-independent), but a task's completion instant is
//! computed only *after* some strategy actually starts it —
//! `start_at + relative_duration_secs` — so the same fixture produces a
//! different, strategy-correct schedule under `StaticBudget` than under
//! `Paced`. Nothing in this module ever bakes in a schedule a different
//! strategy's decisions could invalidate.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use time::OffsetDateTime;

use crate::strategy::{DriveEvent, Strategy};
use crate::trace::{ExogenousEvent, TraceSpec, TRACE_EPOCH};
use crate::world::{World, WorldError};

/// Safety bound on total queue items processed, so a pathological
/// fixture or a strategy that keeps re-deciding the same thing forever
/// cannot hang a test. No real fixture in this PR comes close —
/// `World::start_task` refuses a task that's already active/completed,
/// which is what actually prevents an infinite restart loop.
const MAX_DRIVE_STEPS: usize = 100_000;

/// One entry the driver's event queue orders by `(at, priority, seq)` —
/// `seq` is this queue's own monotonic insertion counter, used only to
/// break ties deterministically (never wall-clock or `HashMap`
/// iteration), matching `pacing::step`'s own `(at, seq)` ordering
/// discipline.
#[derive(Debug, Clone)]
struct QueueItem {
    at: OffsetDateTime,
    priority: u8,
    seq: u64,
    kind: QueueKind,
}

/// Lower sorts first within the same instant: an exogenous fixture event
/// takes effect before a derived completion at the same instant (the
/// trace's own authored intent), and a completion before a synthesized
/// strategy-poll tick.
#[derive(Debug, Clone)]
enum QueueKind {
    Exogenous(ExogenousEvent),
    SpendReport { task: u32, amount: u64 },
    Completion { task: u32 },
    Poll,
}

impl QueueKind {
    fn priority(&self) -> u8 {
        match self {
            QueueKind::Exogenous(_) => 0,
            QueueKind::SpendReport { .. } => 1,
            QueueKind::Completion { .. } => 2,
            QueueKind::Poll => 3,
        }
    }
}

impl PartialEq for QueueItem {
    fn eq(&self, other: &Self) -> bool {
        self.at == other.at && self.priority == other.priority && self.seq == other.seq
    }
}
impl Eq for QueueItem {}
impl Ord for QueueItem {
    fn cmp(&self, other: &Self) -> Ordering {
        // `BinaryHeap` is a max-heap; reverse so the earliest `(at,
        // priority, seq)` pops first.
        other
            .at
            .cmp(&self.at)
            .then(other.priority.cmp(&self.priority))
            .then(other.seq.cmp(&self.seq))
    }
}
impl PartialOrd for QueueItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// One entry in the driver's own replay log — the record
/// `tests/deterministic_replay.rs` compares byte-for-byte across two
/// runs of the same fixture/strategy pair.
#[derive(Debug, Clone, PartialEq)]
pub enum DriveLogEntry {
    Started { task: u32 },
    Spend { task: u32, amount: u64 },
    Completed { task: u32, settled: u64 },
    ForeignSpend { window: String, amount: u64 },
    Restart,
    PreferenceChanged,
}

/// The full result of one `run_trace` call: the ordered log and the
/// final [`World`] (ground truth), which PR-B's verifier will read —
/// this PR only proves it is internally consistent
/// (`World::assert_consistent`).
pub struct DriveOutcome {
    pub log: Vec<(OffsetDateTime, DriveLogEntry)>,
    pub world: World,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum DriveError {
    #[error("drive exceeded the {MAX_DRIVE_STEPS}-step safety bound — a strategy or fixture is not converging")]
    StepBudgetExhausted,
    #[error(transparent)]
    World(#[from] WorldError),
}

/// Replays `spec`'s frozen workload against `strategy`, mutating a fresh
/// [`World`] as the ground-truth record of what actually happened.
pub fn run_trace(
    spec: &TraceSpec,
    mut strategy: Box<dyn Strategy>,
) -> Result<DriveOutcome, DriveError> {
    let mut world = World::new(&spec.windows);
    let mut log: Vec<(OffsetDateTime, DriveLogEntry)> = Vec::new();
    let mut queue: BinaryHeap<QueueItem> = BinaryHeap::new();
    let mut seq = 0u64;

    let push = |queue: &mut BinaryHeap<QueueItem>, seq: &mut u64, at, kind: QueueKind| {
        let priority = kind.priority();
        queue.push(QueueItem {
            at,
            priority,
            seq: *seq,
            kind,
        });
        *seq += 1;
    };

    for event in &spec.events {
        let at = TRACE_EPOCH
            + time::Duration::seconds(i64::try_from(event.offset_secs()).unwrap_or(i64::MAX));
        push(
            &mut queue,
            &mut seq,
            at,
            QueueKind::Exogenous(event.clone()),
        );
    }
    // An initial poll at the epoch, so a task with no dependencies and no
    // triggering exogenous event still gets a first decision chance.
    push(&mut queue, &mut seq, TRACE_EPOCH, QueueKind::Poll);

    let mut steps = 0usize;
    while let Some(item) = queue.pop() {
        steps += 1;
        if steps > MAX_DRIVE_STEPS {
            return Err(DriveError::StepBudgetExhausted);
        }
        let at = item.at;

        match item.kind {
            QueueKind::Exogenous(event) => match event {
                ExogenousEvent::QuotaSnapshotTold {
                    window, remaining, ..
                } => {
                    strategy.notify(&DriveEvent::QuotaSnapshotTold {
                        window,
                        at,
                        remaining,
                    });
                }
                ExogenousEvent::ForeignSpend { window, amount, .. } => {
                    world.apply_foreign_spend(&window, amount, at)?;
                    log.push((
                        at,
                        DriveLogEntry::ForeignSpend {
                            window: window.clone(),
                            amount,
                        },
                    ));
                    strategy.notify(&DriveEvent::ForeignSpend { window, at, amount });
                }
                ExogenousEvent::DaemonRestart { .. } => {
                    strategy.on_restart();
                    log.push((at, DriveLogEntry::Restart));
                }
                ExogenousEvent::PreferenceChanged { preference, .. } => {
                    strategy.notify(&DriveEvent::PreferenceChanged {
                        at,
                        preference: preference.clone(),
                    });
                    log.push((at, DriveLogEntry::PreferenceChanged));
                }
            },
            QueueKind::SpendReport { task, amount } => {
                world.report_spend(task, amount, at)?;
                log.push((at, DriveLogEntry::Spend { task, amount }));
                strategy.notify(&DriveEvent::Spend { task, at, amount });
            }
            QueueKind::Completion { task } => {
                let task_spec = spec
                    .task(task)
                    .expect("a scheduled completion always names a task the trace declares");
                let settled = world.complete_task(task_spec, at)?;
                log.push((at, DriveLogEntry::Completed { task, settled }));
                strategy.notify(&DriveEvent::TaskCompleted { task, at });
            }
            QueueKind::Poll => {}
        }

        // After every queue item, give the strategy a fresh chance to
        // start newly-ready work — a completion may have freed a
        // dependency, a restart may have reset internal pacing, etc.
        let ready = world.ready_tasks(&spec.tasks);
        if ready.is_empty() {
            continue;
        }
        let decided = strategy.decide(at, &ready);
        for task_id in decided {
            if !ready.contains(&task_id) {
                // The strategy named a task that isn't actually ready
                // (already active/completed, or unknown) — ignored, not
                // a panic: a strategy mutant that does this is exactly
                // the kind of thing PR-B's verifier should be free to
                // flag, never something this driver crashes on.
                continue;
            }
            let task_spec = spec
                .task(task_id)
                .expect("ready_tasks only ever returns ids present in spec.tasks");
            if world.start_task(task_spec, at).is_err() {
                continue;
            }
            log.push((at, DriveLogEntry::Started { task: task_id }));
            // Open a claim on every window sharing this task's unit —
            // mirrors `pacing::step::pending_for_window`'s "every window
            // of the task's unit sees the hold" rule. Opened at the
            // task's declared `relative_spend` floor; a later
            // `overrun_spend` report (if any) raises it further.
            for window in &spec.windows {
                if window.unit == task_spec.unit {
                    let _ = world.open_claim(task_id, &window.name, task_spec.relative_spend);
                }
            }
            let duration = time::Duration::seconds(
                i64::try_from(task_spec.relative_duration_secs).unwrap_or(i64::MAX),
            );
            if let Some(overrun) = task_spec.overrun_spend {
                push(
                    &mut queue,
                    &mut seq,
                    at + duration / 2,
                    QueueKind::SpendReport {
                        task: task_id,
                        amount: overrun,
                    },
                );
            }
            push(
                &mut queue,
                &mut seq,
                at + duration,
                QueueKind::Completion { task: task_id },
            );
        }
    }

    Ok(DriveOutcome { log, world })
}
