//! Exact positions for the Execution Identity owner's durable association.
//! This is a selector over identity v1, not a second identity envelope.

use crate::{ExecutionIdentity, LineageStatus, PlanId, Scope, TaskId};
use serde::{Deserialize, Serialize};

pub const EXECUTION_ASSOCIATION_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssociationUnavailable {
    Missing,
    Ambiguous,
    Stale,
    Unsupported,
    ReplayConflict,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionTarget {
    pub task_id: TaskId,
    pub plan_id: PlanId,
    pub initial_plan_id: PlanId,
    pub lineage_status: LineageStatus,
    pub parent_agent_id: Option<String>,
}

/// Injective encodings keep absent optional dimensions distinct from values,
/// including values containing separators. Observation/event IDs do not own
/// an execution position and cannot provide replay protection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionPosition {
    pub lane: String,
    pub turn: String,
    pub exact: String,
    pub context: String,
}

impl ExecutionPosition {
    pub fn from_identity(identity: &ExecutionIdentity) -> Result<Self, AssociationUnavailable> {
        identity
            .cache_key(Scope::TurnTask)
            .map_err(|_| AssociationUnavailable::Unsupported)?;
        let mut wire =
            serde_json::to_value(identity).map_err(|_| AssociationUnavailable::Unsupported)?;
        let object = wire
            .as_object_mut()
            .ok_or(AssociationUnavailable::Unsupported)?;
        // Empty optional IDs are not native evidence, even where the shared
        // envelope allows them as a historical wire value.
        for field in [
            "tool_instance_id",
            "provider_session_id",
            "agent_id",
            "turn_id",
            "parent_agent_id",
            "session_lineage_id",
            "repo_id",
            "worktree_id",
        ] {
            if object.get(field).and_then(|v| v.as_str()) == Some("") {
                return Err(AssociationUnavailable::Unsupported);
            }
        }
        let lane = serde_json::to_string(&serde_json::json!([
            identity.host_id(),
            identity.tool_provider(),
            identity.provider_session_id(),
            identity.agent_id()
        ]))
        .map_err(|_| AssociationUnavailable::Unsupported)?;
        object.remove("observed_at");
        object.remove("event_id");
        let exact =
            serde_json::to_string(&wire).map_err(|_| AssociationUnavailable::Unsupported)?;
        wire.as_object_mut()
            .ok_or(AssociationUnavailable::Unsupported)?
            .remove("turn_id");
        let context =
            serde_json::to_string(&wire).map_err(|_| AssociationUnavailable::Unsupported)?;
        Ok(Self {
            context,
            lane,
            turn: identity
                .turn_id()
                .ok_or(AssociationUnavailable::Unsupported)?
                .to_owned(),
            exact,
        })
    }
}
