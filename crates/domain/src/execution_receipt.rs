//! [`ExecutionReceipt`] — the estimate-vs-actual record used for
//! calibration.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{
    execution_outcome::ExecutionOutcome, execution_plan::PlanId, resource_amount::ResourceAmount,
    task_features::TaskFeatures, task_identity::TaskId,
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
    /// Number of tool invocations observed for the finalized session (see
    /// `PostToolUse` hook counter, HORO-1126). `0` is a genuine count, not
    /// a missing-data marker — the counter always starts at zero and is
    /// always available once a session exists.
    pub tool_call_count: u64,
    /// The model identifier reported by the harness's hook payload, if
    /// any. `None` when the harness does not expose it (see
    /// [`Self::provider`] docs — this is a real, honestly-`None`-able
    /// field, not a placeholder that will always be populated later).
    pub model: Option<String>,
    /// The provider identifier, if the harness's hook payload exposes
    /// one. As of MVP 1.0, Claude Code's `Stop`/`PostToolUse` hook
    /// payloads do not expose a provider field at all (only `model`), so
    /// this is always `None` in practice today — a real platform
    /// limitation, not a bug. The field exists so a future harness that
    /// does expose it does not require another schema change.
    pub provider: Option<String>,
    /// The preflight-knowable [`TaskFeatures`] this task's plan was
    /// estimated against, if any (HORO-1130). `None` for a pre-MVP-2
    /// receipt recorded before this field existed — a genuinely absent
    /// value, not a placeholder; such a receipt still contributes to
    /// global-tier estimation (see `libra-governor-estimator`), just
    /// never to a class-bucketed tier.
    pub task_features: Option<TaskFeatures>,
}

impl ExecutionReceipt {
    #[allow(clippy::too_many_arguments)]
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
            tool_call_count: 0,
            model: None,
            provider: None,
            task_features: None,
        }
    }

    /// Attaches the tool-call count observed for the finalized session.
    pub fn with_tool_call_count(mut self, tool_call_count: u64) -> Self {
        self.tool_call_count = tool_call_count;
        self
    }

    /// Attaches the model identifier, if the harness's hook payload
    /// exposed one.
    pub fn with_model(mut self, model: Option<String>) -> Self {
        self.model = model;
        self
    }

    /// Attaches the provider identifier, if the harness's hook payload
    /// exposed one. See field docs on [`Self::provider`] for why this is
    /// always `None` for Claude Code today.
    pub fn with_provider(mut self, provider: Option<String>) -> Self {
        self.provider = provider;
        self
    }

    /// Attaches the [`TaskFeatures`] the originating plan was estimated
    /// against, if any (HORO-1130).
    pub fn with_task_features(mut self, task_features: Option<TaskFeatures>) -> Self {
        self.task_features = task_features;
        self
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
