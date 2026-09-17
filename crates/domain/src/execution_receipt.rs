//! [`ExecutionReceipt`] — the estimate-vs-actual record used for
//! calibration.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{
    execution_outcome::ExecutionOutcome, execution_plan::PlanId, resource_amount::ResourceAmount,
    task_identity::TaskId,
};

/// The recorded actuals for one task's execution, tied back to the plan
/// (and therefore contract revision) it was estimated against.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionReceipt {
    pub task_id: TaskId,
    /// The contract revision in force when this receipt was recorded.
    pub contract_revision: u32,
    /// The plan/estimate this receipt is being compared against.
    pub plan_id: PlanId,
    /// Wall-clock duration actually spent, in seconds.
    pub actual_duration_secs: u64,
    /// Actual resource usage. A `Vec` because real execution may consume
    /// more than one kind of resource (e.g. both provider USD and a
    /// subscription quota percentage) and none should be discarded to
    /// force-fit a single field.
    pub actual_usage: Vec<ResourceAmount>,
    pub outcome: ExecutionOutcome,
    pub recorded_at: OffsetDateTime,
}

impl ExecutionReceipt {
    pub fn new(
        task_id: TaskId,
        contract_revision: u32,
        plan_id: PlanId,
        actual_duration_secs: u64,
        actual_usage: Vec<ResourceAmount>,
        outcome: ExecutionOutcome,
        recorded_at: OffsetDateTime,
    ) -> Self {
        Self {
            task_id,
            contract_revision,
            plan_id,
            actual_duration_secs,
            actual_usage,
            outcome,
            recorded_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution_plan::PlanId;

    #[test]
    fn receipt_links_task_and_plan() {
        let task_id = TaskId::new();
        let plan_id = PlanId::new();
        let receipt = ExecutionReceipt::new(
            task_id,
            1,
            plan_id,
            120,
            vec![ResourceAmount::Tokens(5000)],
            ExecutionOutcome::Completed {
                evidence: vec!["pr#1".to_string()],
            },
            OffsetDateTime::UNIX_EPOCH,
        );
        assert_eq!(receipt.task_id, task_id);
        assert_eq!(receipt.plan_id, plan_id);
    }
}
