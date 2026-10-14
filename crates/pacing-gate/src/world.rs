//! The ground-truth "world" model (HORO-1768 PR-A).
//!
//! A [`World`] tracks true concurrent occupancy and true cumulative spend
//! per window, **independent of what any strategy is told**. A strategy
//! only ever sees what [`crate::drive::run_trace`] chooses to hand it
//! (the ready task list, and whatever `ExogenousEvent::QuotaSnapshotTold`
//! fixtures declare) — never a reference to this type. That separation is
//! the whole point: a buggy or dishonest strategy's self-report (what it
//! believes it has admitted/spent) can be checked against what the world
//! says actually happened. PR-B's verifier is what actually performs that
//! check; this PR only builds the world model and proves it is
//! internally consistent (see `tests/world_consistency.rs`).

use std::collections::{BTreeMap, BTreeSet};

use time::OffsetDateTime;

use crate::trace::{TraceTask, TraceUnit, WindowSpec};

/// Ground truth for one window: every real spend ever recorded against
/// it (a task's own settled spend, or a third party's `ForeignSpend`),
/// plus which tasks currently hold an open (not-yet-settled) claim.
#[derive(Debug, Clone, PartialEq)]
pub struct WorldWindow {
    pub unit: TraceUnit,
    /// Append-only ground-truth ledger: every settled or foreign spend
    /// ever applied to this window, in application order.
    pub settled: Vec<u64>,
    /// Currently open (unsettled) claims, keyed by task id — a task's
    /// real spend-so-far against this window's unit, visible to the
    /// world the instant it is reported, never only at completion.
    pub open: BTreeMap<u32, u64>,
}

impl WorldWindow {
    /// True current occupancy: every settled spend plus every still-open
    /// claim. This is the number a strategy's own belief should never
    /// exceed without the world's evidence backing it up — the
    /// comparison PR-B's verifier performs.
    pub fn true_occupancy(&self) -> u64 {
        self.settled.iter().sum::<u64>() + self.open.values().sum::<u64>()
    }
}

/// One task the world has started and not yet completed.
#[derive(Debug, Clone, PartialEq)]
struct ActiveTruth {
    principal: String,
    unit: TraceUnit,
    started_at: OffsetDateTime,
    /// Real spend reported so far for this task, in its own unit —
    /// distinct from (and never capped by) whatever hold a strategy
    /// believes it reserved.
    spend_so_far: u64,
}

/// One entry in the world's append-only event log — the authoritative
/// record `tests/world_consistency.rs` and (in PR-B) the verifier read
/// back, never reconstructed from a strategy's own report.
#[derive(Debug, Clone, PartialEq)]
pub enum WorldEvent {
    TaskStarted {
        task: u32,
        at: OffsetDateTime,
    },
    TaskSpend {
        task: u32,
        at: OffsetDateTime,
        amount: u64,
    },
    TaskCompleted {
        task: u32,
        at: OffsetDateTime,
        settled: u64,
    },
    ForeignSpend {
        window: String,
        at: OffsetDateTime,
        amount: u64,
    },
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum WorldError {
    #[error("task {0} is already active")]
    AlreadyActive(u32),
    #[error("task {0} is already completed")]
    AlreadyCompleted(u32),
    #[error("task {0} is not active")]
    NotActive(u32),
    #[error("window {0:?} is not declared in this world")]
    UnknownWindow(String),
}

/// The ground-truth world a frozen trace is driven against. Keyed by
/// `BTreeMap`/`BTreeSet` throughout — the same AC4-style determinism
/// discipline `pacing::step::PacerState` follows — so iteration order
/// never affects a replay.
#[derive(Debug, Clone, Default)]
pub struct World {
    windows: BTreeMap<String, WorldWindow>,
    active: BTreeMap<u32, ActiveTruth>,
    completed: BTreeSet<u32>,
    log: Vec<(OffsetDateTime, WorldEvent)>,
}

impl World {
    pub fn new(windows: &[WindowSpec]) -> Self {
        let mut w = BTreeMap::new();
        for window in windows {
            w.insert(
                window.name.clone(),
                WorldWindow {
                    unit: window.unit,
                    settled: Vec::new(),
                    open: BTreeMap::new(),
                },
            );
        }
        World {
            windows: w,
            active: BTreeMap::new(),
            completed: BTreeSet::new(),
            log: Vec::new(),
        }
    }

    pub fn log(&self) -> &[(OffsetDateTime, WorldEvent)] {
        &self.log
    }

    pub fn window(&self, name: &str) -> Option<&WorldWindow> {
        self.windows.get(name)
    }

    pub fn is_completed(&self, task: u32) -> bool {
        self.completed.contains(&task)
    }

    pub fn is_active(&self, task: u32) -> bool {
        self.active.contains_key(&task)
    }

    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    /// Every task whose dependencies are all completed and which is
    /// neither active nor completed itself — the one fact every strategy
    /// is told, regardless of how much else it is kept blind to.
    pub fn ready_tasks(&self, tasks: &[TraceTask]) -> Vec<u32> {
        tasks
            .iter()
            .filter(|t| {
                !self.active.contains_key(&t.id)
                    && !self.completed.contains(&t.id)
                    && t.depends_on.iter().all(|d| self.completed.contains(d))
            })
            .map(|t| t.id)
            .collect()
    }

    /// Starts a task against the world's own books — never against a
    /// window directly: the task's real holds against each window it
    /// touches are recorded separately, by [`Self::record_window_claim`],
    /// since a trace task declares one unit but may be checked against
    /// several windows sharing that unit (mirrors `pacing::step`'s own
    /// `pending_for_window` shape, which keys holds by unit, not by a
    /// single named window).
    pub fn start_task(&mut self, task: &TraceTask, at: OffsetDateTime) -> Result<(), WorldError> {
        if self.active.contains_key(&task.id) {
            return Err(WorldError::AlreadyActive(task.id));
        }
        if self.completed.contains(&task.id) {
            return Err(WorldError::AlreadyCompleted(task.id));
        }
        self.active.insert(
            task.id,
            ActiveTruth {
                principal: task.principal.clone(),
                unit: task.unit,
                started_at: at,
                spend_so_far: 0,
            },
        );
        self.log
            .push((at, WorldEvent::TaskStarted { task: task.id, at }));
        Ok(())
    }

    /// Records a window claim opening for `task` the instant it starts —
    /// every window sharing the task's unit sees the same open claim,
    /// matching how a real quota window's "outstanding hold" check works
    /// (a task's reservation constrains every window of its unit at
    /// once, not just one named window).
    pub fn open_claim(&mut self, task: u32, window: &str, amount: u64) -> Result<(), WorldError> {
        let w = self
            .windows
            .get_mut(window)
            .ok_or_else(|| WorldError::UnknownWindow(window.to_string()))?;
        w.open.insert(task, amount);
        Ok(())
    }

    /// Reports real spend for an active task — world-visible the instant
    /// it happens, in every window that currently holds an open claim for
    /// it.
    pub fn report_spend(
        &mut self,
        task: u32,
        amount: u64,
        at: OffsetDateTime,
    ) -> Result<(), WorldError> {
        let active = self
            .active
            .get_mut(&task)
            .ok_or(WorldError::NotActive(task))?;
        active.spend_so_far = active.spend_so_far.max(amount);
        for w in self.windows.values_mut() {
            if w.unit == active.unit {
                if let Some(open) = w.open.get_mut(&task) {
                    *open = (*open).max(amount);
                }
            }
        }
        self.log
            .push((at, WorldEvent::TaskSpend { task, at, amount }));
        Ok(())
    }

    /// Settles a task: its true spend (the larger of any reported spend
    /// and its own declared `relative_spend` floor) moves from every
    /// window's open claims into that window's permanent settled ledger.
    /// Returns the settled amount.
    pub fn complete_task(
        &mut self,
        task_spec: &TraceTask,
        at: OffsetDateTime,
    ) -> Result<u64, WorldError> {
        let active = self
            .active
            .remove(&task_spec.id)
            .ok_or(WorldError::NotActive(task_spec.id))?;
        let settled = active.spend_so_far.max(task_spec.relative_spend);
        for w in self.windows.values_mut() {
            if w.unit == active.unit && w.open.remove(&task_spec.id).is_some() {
                w.settled.push(settled);
            }
        }
        self.completed.insert(task_spec.id);
        self.log.push((
            at,
            WorldEvent::TaskCompleted {
                task: task_spec.id,
                at,
                settled,
            },
        ));
        Ok(settled)
    }

    /// Applies a third party's real spend against a shared window — never
    /// routed through any task's own bookkeeping, and never told to a
    /// strategy (see `ExogenousEvent::ForeignSpend`'s docs).
    pub fn apply_foreign_spend(
        &mut self,
        window: &str,
        amount: u64,
        at: OffsetDateTime,
    ) -> Result<(), WorldError> {
        let w = self
            .windows
            .get_mut(window)
            .ok_or_else(|| WorldError::UnknownWindow(window.to_string()))?;
        w.settled.push(amount);
        self.log.push((
            at,
            WorldEvent::ForeignSpend {
                window: window.to_string(),
                at,
                amount,
            },
        ));
        Ok(())
    }

    /// Internal-consistency invariants this world must always satisfy,
    /// regardless of which strategy or fixture produced it — never a
    /// check of whether a *strategy* behaved correctly (that is PR-B's
    /// verifier). Returns every violation found, rather than stopping at
    /// the first, so a test failure reports the full picture.
    pub fn assert_consistent(&self) -> Result<(), Vec<String>> {
        let mut problems = Vec::new();
        for (name, window) in &self.windows {
            for task in window.open.keys() {
                if !self.active.contains_key(task) {
                    problems.push(format!(
                        "window {name:?} has an open claim for task {task}, \
                         which is not active"
                    ));
                }
            }
        }
        for (task, active) in &self.active {
            if self.completed.contains(task) {
                problems.push(format!("task {task} is both active and completed"));
            }
            let _ = active;
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems)
        }
    }
}
