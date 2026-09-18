//! [`ExecutionPlan`] — the identity/versioning linkage an estimate assumes.
//!
//! This does not contain estimator math (P50/P90 numbers land in
//! HORO-1126); it exists so that once an estimate *is* produced, it can be
//! tied back unambiguously to the exact task, contract revision, and
//! reconnaissance snapshot it was computed against.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    estimate::Estimate, replan::ReplanReason, task_features::TaskFeatures, task_identity::TaskId,
};

/// Identifier for one [`ExecutionPlan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PlanId(pub Uuid);

impl PlanId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for PlanId {
    fn default() -> Self {
        Self::new()
    }
}

/// Links a produced estimate to the exact `(task, contract revision,
/// reconnaissance snapshot)` triple it assumed.
///
/// `recon_snapshot_ref` is an opaque reference (e.g. a content hash or
/// stored-artifact ID for the bounded reconnaissance output) rather than
/// the snapshot content itself — the snapshot's storage/format is owned
/// by the estimator ticket (HORO-1126); this type only needs a stable
/// pointer to it.
///
/// Not `Eq` — [`Estimate`] embeds [`crate::ResourceAmount`], which carries
/// an `f32` (`QuotaPercent`) and so is `PartialEq` only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionPlan {
    pub id: PlanId,
    pub task_id: TaskId,
    /// The [`crate::CompletionContract`] revision this plan assumed.
    pub contract_revision: u32,
    /// Opaque reference to the reconnaissance/feature snapshot used, if
    /// any was taken.
    pub recon_snapshot_ref: Option<String>,
    pub created_at: OffsetDateTime,
    /// The probabilistic estimate (HORO-1126) computed for this plan, if
    /// any. Set after construction via [`Self::with_estimate`] rather than
    /// as a required constructor argument, so every existing
    /// `ExecutionPlan::new` call site (pre-dating the estimator) keeps
    /// compiling unchanged.
    pub estimate: Option<Estimate>,
    /// The [`TaskFeatures`] this plan's estimate was computed against, if
    /// any (HORO-1130). Persisted here (mirroring how [`Self::estimate`]
    /// is carried) so `hook stop`'s later `Finalize` request — which has
    /// no access to the original prompt or reconnaissance output — can
    /// copy it onto the finalized [`crate::ExecutionReceipt`] without
    /// re-deriving it.
    pub task_features: Option<TaskFeatures>,
    /// The prior [`PlanId`] this plan replaces, if this plan was
    /// produced by a replan (HORO-1139) rather than an original
    /// preflight. `None` for an original preflight plan.
    pub replaces: Option<PlanId>,
    /// The structured reason a replan produced this plan, if any. Always
    /// `Some` iff `replaces` is `Some` — see [`Self::with_replan_linkage`].
    pub replan_reason: Option<ReplanReason>,
}

impl ExecutionPlan {
    pub fn new(
        task_id: TaskId,
        contract_revision: u32,
        recon_snapshot_ref: Option<String>,
        created_at: OffsetDateTime,
    ) -> Self {
        Self {
            id: PlanId::new(),
            task_id,
            contract_revision,
            recon_snapshot_ref,
            created_at,
            estimate: None,
            task_features: None,
            replaces: None,
            replan_reason: None,
        }
    }

    /// Attaches an [`Estimate`] to this plan, returning `self` for
    /// chaining at the construction site.
    pub fn with_estimate(mut self, estimate: Estimate) -> Self {
        self.estimate = Some(estimate);
        self
    }

    /// Attaches the [`TaskFeatures`] this plan's estimate was computed
    /// against, returning `self` for chaining at the construction site.
    pub fn with_task_features(mut self, task_features: Option<TaskFeatures>) -> Self {
        self.task_features = task_features;
        self
    }

    /// Marks this plan as a replan of `prior_plan_id` for `reason`
    /// (HORO-1139) — the linkage a replanned plan MUST carry: which plan
    /// it replaces and why. Never touches `contract_revision` or any
    /// other field of `self` — a replan cannot alter the Completion
    /// Contract this plan assumes, only the estimate/scheduling it
    /// carries (see `crate::replan` module docs and the
    /// `required_criteria_*` tests there).
    pub fn with_replan_linkage(mut self, prior_plan_id: PlanId, reason: ReplanReason) -> Self {
        self.replaces = Some(prior_plan_id);
        self.replan_reason = Some(reason);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_links_task_and_contract_revision() {
        let task_id = TaskId::new();
        let plan = ExecutionPlan::new(
            task_id,
            2,
            Some("snap-1".to_string()),
            OffsetDateTime::UNIX_EPOCH,
        );
        assert_eq!(plan.task_id, task_id);
        assert_eq!(plan.contract_revision, 2);
    }

    #[test]
    fn a_fresh_plan_carries_no_replan_linkage() {
        let plan = ExecutionPlan::new(TaskId::new(), 1, None, OffsetDateTime::UNIX_EPOCH);
        assert_eq!(plan.replaces, None);
        assert_eq!(plan.replan_reason, None);
    }

    #[test]
    fn with_replan_linkage_records_the_prior_plan_and_reason_without_touching_contract_revision() {
        use crate::replan::ReplanTriggerKind;

        let prior = ExecutionPlan::new(TaskId::new(), 3, None, OffsetDateTime::UNIX_EPOCH);
        let reason = ReplanReason::new(
            ReplanTriggerKind::ToolCallCountExceeded,
            "n=13 vs typical 5",
        );
        let replanned = ExecutionPlan::new(
            prior.task_id,
            prior.contract_revision,
            None,
            OffsetDateTime::UNIX_EPOCH,
        )
        .with_replan_linkage(prior.id, reason.clone());

        assert_eq!(replanned.replaces, Some(prior.id));
        assert_eq!(replanned.replan_reason, Some(reason));
        assert_eq!(
            replanned.contract_revision, prior.contract_revision,
            "a replan must never alter the contract revision it assumes"
        );
    }
}
