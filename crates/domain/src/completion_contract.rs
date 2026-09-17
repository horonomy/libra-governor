//! [`CompletionContract`] — the objective Definition of Done for a task.

use serde::{Deserialize, Serialize};

/// One criterion a task must (or may optionally) satisfy to be considered
/// done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionCriterion {
    /// Human-readable description of the criterion, e.g. "All new code has
    /// passing unit tests".
    pub description: String,
    /// Whether this criterion is required for completion. `false` marks a
    /// nice-to-have that does not block a `Completed` outcome.
    pub required: bool,
}

impl CompletionCriterion {
    /// Creates a required criterion.
    pub fn required(description: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            required: true,
        }
    }

    /// Creates an optional criterion.
    pub fn optional(description: impl Into<String>) -> Self {
        Self {
            description: description.into(),
            required: false,
        }
    }
}

/// The objective Definition of Done for a task, at a specific revision.
///
/// A contract is immutable once created: changing the criteria produces a
/// new `revision`, never a mutation of an existing one, so that any
/// [`crate::ExecutionPlan`] or estimate that assumed a given revision
/// remains reproducible and auditable after the contract evolves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionContract {
    /// Monotonically increasing revision number for this task's contract.
    /// Starts at 1.
    pub revision: u32,
    /// The criteria that make up this revision of the Definition of Done.
    pub criteria: Vec<CompletionCriterion>,
}

impl CompletionContract {
    /// Creates the first revision (revision 1) of a contract.
    pub fn first(criteria: Vec<CompletionCriterion>) -> Self {
        Self {
            revision: 1,
            criteria,
        }
    }

    /// Produces the next revision of this contract with new criteria,
    /// leaving `self` untouched.
    pub fn next_revision(&self, criteria: Vec<CompletionCriterion>) -> Self {
        Self {
            revision: self.revision + 1,
            criteria,
        }
    }

    /// Returns only the required criteria.
    pub fn required_criteria(&self) -> impl Iterator<Item = &CompletionCriterion> {
        self.criteria.iter().filter(|c| c.required)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_revision_increments_without_mutating_original() {
        let v1 = CompletionContract::first(vec![CompletionCriterion::required("tests pass")]);
        let v2 = v1.next_revision(vec![
            CompletionCriterion::required("tests pass"),
            CompletionCriterion::optional("docs updated"),
        ]);

        assert_eq!(v1.revision, 1);
        assert_eq!(v2.revision, 2);
        assert_eq!(v1.criteria.len(), 1);
        assert_eq!(v2.criteria.len(), 2);
    }

    #[test]
    fn required_criteria_filters_optional_ones() {
        let contract = CompletionContract::first(vec![
            CompletionCriterion::required("required one"),
            CompletionCriterion::optional("optional one"),
        ]);

        let required: Vec<_> = contract.required_criteria().collect();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0].description, "required one");
    }
}
