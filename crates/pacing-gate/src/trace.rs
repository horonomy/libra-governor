//! Frozen trace fixtures (HORO-1768 PR-A).
//!
//! A [`TraceSpec`] freezes **the workload, not the outcome**: tasks carry
//! only *relative* duration/spend/priority/dependency shape, and every
//! [`ExogenousEvent`] fires at a *relative* offset from the trace's own
//! epoch. This is deliberately not
//! `libra_governor_domain::pacing::PacingEvent`'s format — that type
//! carries absolute timestamps tied to one already-resolved schedule,
//! which is exactly what makes `pacing::simulate::simulate` unable to
//! replay a frozen trace unmodified across different strategies (a
//! completion time that fits a static-budget schedule is meaningless
//! under sustain/burst pacing, and `step::step` treats a `TaskCompleted`
//! for a non-active task as a no-op). [`crate::drive::run_trace`] instead
//! derives every absolute instant itself, from whichever strategy
//! actually started each task and when (see that module's docs).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// Every trace shares one fixed epoch — never `OffsetDateTime::now_utc()`
/// — so two runs of the same fixture are byte-identical (the
/// `deterministic_replay` test in `tests/` depends on this).
pub const TRACE_EPOCH: OffsetDateTime = OffsetDateTime::UNIX_EPOCH;

/// Fixed namespace for deriving a window's [`libra_governor_domain::
/// QuotaWindowId`] from its stable fixture-local name, via
/// `Uuid::new_v5`. Never [`Uuid::new_v4`] (would make two runs diverge)
/// and never `Uuid::from_u128` of a small sequential integer — see
/// `libra_governor_domain::pacing::synthetic_uuid`'s docs for the
/// collision class that small-integer seeding invites.
const WINDOW_NAMESPACE: Uuid = Uuid::from_bytes([
    0xb7, 0x68, 0x1d, 0x00, 0xfa, 0xce, 0x48, 0x1b, 0x9e, 0x01, 0x11, 0x68, 0x00, 0x17, 0x7a, 0x1e,
]);

/// Derives a deterministic window id from its fixture-local stable name.
pub fn window_uuid(window_name: &str) -> Uuid {
    Uuid::new_v5(&WINDOW_NAMESPACE, window_name.as_bytes())
}

/// Resource unit a task's relative spend/duration figures are expressed
/// in. A small closed set, not `libra_governor_domain::QuotaUnit`
/// directly, so a fixture file never needs to spell that type's full
/// serde shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceUnit {
    Tokens,
    Requests,
    UsdCents,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TracePriority {
    High,
    #[default]
    Normal,
    Low,
}

/// One task in a frozen trace. `depends_on`/`priority` mirror
/// `pacing::SimTask`'s own shape; `relative_duration_secs` and
/// `relative_spend` are what the world uses to compute *real* completion
/// and settlement once some strategy actually starts the task — never
/// baked into an absolute timestamp here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceTask {
    pub id: u32,
    pub principal: String,
    #[serde(default)]
    pub depends_on: Vec<u32>,
    #[serde(default)]
    pub priority: TracePriority,
    pub relative_duration_secs: u64,
    /// The task's true total spend, settled at completion — ground
    /// truth the [`crate::world::World`] records, independent of
    /// whatever a strategy believed it was spending.
    pub relative_spend: u64,
    pub unit: TraceUnit,
    /// An optional real mid-task spend report, strictly larger than
    /// `relative_spend`, reported at the task's halfway point (t6's
    /// overrun/backoff scenario: a task that reports spending more than
    /// its own hold before it ever completes). `None` for every fixture
    /// that has no overrun to model — the task simply settles at
    /// `relative_spend` with no interim report.
    #[serde(default)]
    pub overrun_spend: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WindowSpecKind {
    Sliding {
        length_secs: u64,
        limit: u64,
    },
    RefillBucket {
        capacity: u64,
        refill_amount: u64,
        refill_period_secs: u64,
    },
}

/// A quota window as declared by a fixture. `name` is the stable fixture-
/// local identifier [`window_uuid`] derives a real `QuotaWindowId` from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowSpec {
    pub name: String,
    pub unit: TraceUnit,
    pub kind: WindowSpecKind,
}

/// The scenario's pacing preference, in relative form — `target_end` is
/// an offset from the trace epoch, never an absolute instant, for the
/// same reason every other field in this module is relative.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum TracePreference {
    Sustain {
        horizon_secs: u64,
        continuity_reserve_bp: u16,
    },
    Burst {
        target_end_offset_secs: u64,
        max_fanout: u16,
    },
}

/// One exogenous (non-task) event a fixture declares, at a relative
/// offset from the trace epoch. Deliberately **not**
/// `pacing::PacingEvent` — these describe what happens to the world
/// (and, for `QuotaSnapshotTold`, what a strategy is *told*), not a
/// pacing state machine's own input vocabulary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum ExogenousEvent {
    /// An external quota snapshot a strategy is told about — may
    /// understate or overstate the window's true remaining headroom on
    /// purpose (t3's "insufficient/honest opaque quota" scenario), which
    /// is exactly why [`crate::world::World`] never reads this variant
    /// to update its own ground truth.
    QuotaSnapshotTold {
        offset_secs: u64,
        window: String,
        remaining: u64,
    },
    /// A concurrent session or third party spending against a *shared*
    /// window — real, world-visible, and never reported to a strategy
    /// directly (no strategy in this PR reads `ForeignSpend`; a strategy
    /// that silently peeked at it to look good would be exactly the kind
    /// of finding PR-B's verifier exists to catch).
    ForeignSpend {
        offset_secs: u64,
        window: String,
        amount: u64,
        unit: TraceUnit,
    },
    /// The daemon process restarts mid-run: every strategy's in-memory
    /// state is wiped (see [`crate::strategy::Strategy::on_restart`]) but
    /// the world's ground truth survives unaffected — a restart never
    /// un-happens real spend.
    DaemonRestart { offset_secs: u64 },
    /// The scenario's pacing preference switches mode mid-run (t7's
    /// burst-to-sustain transition). Strategies that do not model a
    /// preference (e.g. `StaticBudget`) ignore this.
    PreferenceChanged {
        offset_secs: u64,
        preference: TracePreference,
    },
}

impl ExogenousEvent {
    pub fn offset_secs(&self) -> u64 {
        match self {
            ExogenousEvent::QuotaSnapshotTold { offset_secs, .. }
            | ExogenousEvent::ForeignSpend { offset_secs, .. }
            | ExogenousEvent::DaemonRestart { offset_secs }
            | ExogenousEvent::PreferenceChanged { offset_secs, .. } => *offset_secs,
        }
    }
}

/// A frozen trace: the full, relative-time description of one
/// falsification scenario. See module docs for why every field here is
/// relative rather than absolute.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TraceSpec {
    pub name: String,
    pub description: String,
    pub initial_preference: TracePreference,
    #[serde(default)]
    pub windows: Vec<WindowSpec>,
    pub tasks: Vec<TraceTask>,
    #[serde(default)]
    pub events: Vec<ExogenousEvent>,
}

#[derive(Debug, thiserror::Error)]
pub enum TraceError {
    #[error("invalid trace JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("trace {name:?} has duplicate task id {task}")]
    DuplicateTaskId { name: String, task: u32 },
    #[error("trace {name:?} task {task} depends on unknown task {dep}")]
    UnknownDependency { name: String, task: u32, dep: u32 },
    #[error("trace {name:?} event references unknown window {window:?}")]
    UnknownWindow { name: String, window: String },
}

impl TraceSpec {
    pub fn from_json(s: &str) -> Result<Self, TraceError> {
        let spec: TraceSpec = serde_json::from_str(s)?;
        spec.validate()?;
        Ok(spec)
    }

    /// Structural validation only (dependency graph, window references) —
    /// deliberately not a cycle check: `TaskSet::validated` (the real
    /// `pacing` scenario constructor the `Paced` strategy feeds) already
    /// owns that, and duplicating it here would risk the two drifting.
    fn validate(&self) -> Result<(), TraceError> {
        let mut seen = std::collections::BTreeSet::new();
        for task in &self.tasks {
            if !seen.insert(task.id) {
                return Err(TraceError::DuplicateTaskId {
                    name: self.name.clone(),
                    task: task.id,
                });
            }
        }
        for task in &self.tasks {
            for dep in &task.depends_on {
                if !seen.contains(dep) {
                    return Err(TraceError::UnknownDependency {
                        name: self.name.clone(),
                        task: task.id,
                        dep: *dep,
                    });
                }
            }
        }
        let window_names: std::collections::BTreeSet<&str> =
            self.windows.iter().map(|w| w.name.as_str()).collect();
        for event in &self.events {
            let referenced = match event {
                ExogenousEvent::QuotaSnapshotTold { window, .. }
                | ExogenousEvent::ForeignSpend { window, .. } => Some(window.as_str()),
                _ => None,
            };
            if let Some(w) = referenced {
                if !window_names.contains(w) {
                    return Err(TraceError::UnknownWindow {
                        name: self.name.clone(),
                        window: w.to_string(),
                    });
                }
            }
        }
        Ok(())
    }

    pub fn task(&self, id: u32) -> Option<&TraceTask> {
        self.tasks.iter().find(|t| t.id == id)
    }

    /// Every window keyed by its stable fixture-local name, for a
    /// strategy or the world to look up by the same name an
    /// [`ExogenousEvent`] references.
    pub fn windows_by_name(&self) -> BTreeMap<&str, &WindowSpec> {
        self.windows.iter().map(|w| (w.name.as_str(), w)).collect()
    }
}
