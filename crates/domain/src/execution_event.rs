//! [`ExecutionEvent`] — a normalized, harness-agnostic lifecycle event.
//!
//! Claude Code (or any future host agent) has its own hook payload shape;
//! `ExecutionEvent` is deliberately not that shape. The Claude Code hook
//! integration (HORO-1125) is responsible for translating host-specific
//! payloads into these variants. Keeping the mapping at the boundary means
//! the ledger schema and every downstream consumer (estimator, receipt
//! query) never depend on Claude Code specifically.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::task_identity::TaskId;

/// A normalized lifecycle event, always tied to exactly one [`TaskId`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionEvent {
    /// The task this event belongs to.
    pub task_id: TaskId,
    /// When the event occurred (harness/host clock, normalized to UTC).
    pub occurred_at: OffsetDateTime,
    /// What happened.
    pub kind: ExecutionEventKind,
}

impl ExecutionEvent {
    pub fn new(task_id: TaskId, occurred_at: OffsetDateTime, kind: ExecutionEventKind) -> Self {
        Self {
            task_id,
            occurred_at,
            kind,
        }
    }
}

/// The minimal-but-real set of lifecycle event kinds MVP 1.0/1.1 need.
///
/// This is intentionally not an exhaustive taxonomy of every possible
/// agent action — only what the estimator (HORO-1126) needs to compute
/// P50/P90 estimates from historical trajectories, and what the hook
/// integration (HORO-1125) needs to mark session boundaries and prompt
/// admission points. New variants are added when a real consumer needs
/// them, not speculatively.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecutionEventKind {
    /// A new agent session (process/conversation) began working this task.
    SessionStarted {
        /// Host-assigned session identifier (opaque; not a `TaskId`).
        session_id: String,
    },
    /// An agent session working this task ended.
    SessionEnded { session_id: String },
    /// The user submitted a prompt within a session for this task.
    UserPromptSubmitted { session_id: String },
    /// A tool call was invoked.
    ToolInvoked {
        session_id: String,
        /// Tool name only (e.g. `"Bash"`, `"Edit"`) — never raw arguments,
        /// per the privacy invariant (see crate docs).
        tool_name: String,
    },
    /// A previously invoked tool call completed.
    ToolCompleted {
        session_id: String,
        tool_name: String,
        /// Whether the tool call succeeded, for calibration purposes.
        succeeded: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_identity::TaskId;

    #[test]
    fn event_carries_its_task_id() {
        let task_id = TaskId::new();
        let event = ExecutionEvent::new(
            task_id,
            OffsetDateTime::UNIX_EPOCH,
            ExecutionEventKind::SessionStarted {
                session_id: "sess-1".to_string(),
            },
        );
        assert_eq!(event.task_id, task_id);
    }
}
