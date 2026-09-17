//! [`ExecutionPlan`] — the identity/versioning linkage an estimate assumes.
//!
//! This does not contain estimator math (P50/P90 numbers land in
//! HORO-1126); it exists so that once an estimate *is* produced, it can be
//! tied back unambiguously to the exact task, contract revision, and
//! reconnaissance snapshot it was computed against.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::task_identity::TaskId;

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionPlan {
    pub id: PlanId,
    pub task_id: TaskId,
    /// The [`crate::CompletionContract`] revision this plan assumed.
    pub contract_revision: u32,
    /// Opaque reference to the reconnaissance/feature snapshot used, if
    /// any was taken.
    pub recon_snapshot_ref: Option<String>,
    pub created_at: OffsetDateTime,
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
        }
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
}
