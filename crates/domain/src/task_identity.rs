//! [`TaskIdentity`] — the durable, harness-agnostic anchor for one unit of
//! work.
//!
//! A task is the *economic unit* Libra reasons about (see
//! `docs/adr/0002-task-not-session-as-economic-unit.md`): one task may span
//! multiple agent sessions, multiple tool invocations, and multiple
//! replans. `TaskIdentity` is what every [`crate::ExecutionEvent`],
//! [`crate::CompletionContract`], [`crate::ExecutionPlan`], and
//! [`crate::ExecutionReceipt`] attaches to — never a session ID, which
//! resets every time the host agent restarts or the user opens a new
//! terminal.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A stable, unique identifier for one task.
///
/// Newtype over [`Uuid`] so a `TaskId` can never be confused with a
/// session ID, an event ID, or any other UUID-shaped value at the type
/// level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaskId(pub Uuid);

impl TaskId {
    /// Generates a new, random task identifier.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for TaskId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// An optional pointer to where this task is tracked outside Libra.
///
/// Kept as a small enum (rather than a bare `Option<String>`) so callers
/// cannot silently mix up a Jira key with a bare URL — the source system
/// is explicit at the type level. Libra never authenticates against or
/// synchronizes with these systems in MVP 1.0; the reference is
/// informational only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ExternalRef {
    /// A Jira issue key or URL, e.g. `HORO-1124` or a full browse URL.
    Jira(String),
    /// A GitHub issue or PR URL.
    GitHub(String),
    /// Any other external tracker reference not worth a dedicated variant.
    Other(String),
}

/// The durable anchor for one task, spanning any number of agent sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskIdentity {
    /// The stable task identifier.
    pub id: TaskId,
    /// Optional external tracker reference (Jira, GitHub, ...).
    pub external_ref: Option<ExternalRef>,
}

impl TaskIdentity {
    /// Creates a new task identity with a freshly generated [`TaskId`].
    pub fn new(external_ref: Option<ExternalRef>) -> Self {
        Self {
            id: TaskId::new(),
            external_ref,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_id_new_values_are_unique() {
        assert_ne!(TaskId::new(), TaskId::new());
    }

    #[test]
    fn task_identity_carries_optional_external_ref() {
        let identity = TaskIdentity::new(Some(ExternalRef::Jira("HORO-1124".to_string())));
        assert_eq!(
            identity.external_ref,
            Some(ExternalRef::Jira("HORO-1124".to_string()))
        );
    }
}
