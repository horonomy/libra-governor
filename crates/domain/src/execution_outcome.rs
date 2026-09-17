//! [`ExecutionOutcome`] — the structural distinction between "a model said
//! it's done" and "completion is evidenced".
//!
//! Libra must never let a task be inferable as done merely because a model
//! claimed so in its final message. Every variant that can represent
//! success carries an explicit evidence list; there is no variant that
//! means "done, trust me" with no evidence field at all.

use serde::{Deserialize, Serialize};

/// The outcome of a task's execution, with supporting evidence references.
///
/// Evidence entries are references (URLs, file paths, CI run IDs, test
/// report identifiers) — never raw output content, per the privacy
/// invariant (see crate docs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExecutionOutcome {
    /// The task's Completion Contract was satisfied, evidenced by the
    /// listed references (e.g. a CI run URL, a PR merge commit SHA).
    Completed { evidence: Vec<String> },
    /// The task did not meet its Completion Contract.
    Failed { evidence: Vec<String> },
    /// Execution was stopped before reaching a terminal state (user
    /// cancellation, budget exhaustion, circuit breaker).
    Aborted { evidence: Vec<String> },
    /// No outcome has been evidenced yet (e.g. the task is still running,
    /// or the harness never reported a terminal state). This is the only
    /// variant a task may start in and is never itself proof of anything.
    Unknown,
}

impl ExecutionOutcome {
    /// The evidence references for this outcome, if any.
    pub fn evidence(&self) -> &[String] {
        match self {
            ExecutionOutcome::Completed { evidence }
            | ExecutionOutcome::Failed { evidence }
            | ExecutionOutcome::Aborted { evidence } => evidence,
            ExecutionOutcome::Unknown => &[],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_outcome_requires_no_implicit_evidence() {
        let outcome = ExecutionOutcome::Completed {
            evidence: vec!["https://github.com/org/repo/pull/1".to_string()],
        };
        assert_eq!(outcome.evidence().len(), 1);
    }

    #[test]
    fn unknown_outcome_has_no_evidence() {
        assert!(ExecutionOutcome::Unknown.evidence().is_empty());
    }
}
