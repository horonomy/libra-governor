//! Exact positions for the Execution Identity owner's durable association.
//! This is a selector over identity v1, not a second identity envelope.

use crate::{ExecutionIdentity, LineageStatus, PlanId, TaskId};
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
        // HORO-1714 decision A (2026-10-10): session+turn are the minimum
        // position a lane can be keyed on -- unlike `Scope::TurnTask`'s
        // cache key, this deliberately does NOT require `agent_id`. An
        // agent-absent event is a legitimate, explicitly represented lane
        // (see the `lineage_status` check below), never a reason to refuse
        // outright. Checked directly rather than via `cache_key` because
        // `Scope` has no variant for "session+turn, agent optional".
        identity
            .provider_session_id()
            .filter(|s| !s.is_empty())
            .ok_or(AssociationUnavailable::Unsupported)?;
        identity
            .turn_id()
            .filter(|t| !t.is_empty())
            .ok_or(AssociationUnavailable::Unsupported)?;
        // An agent-absent event must never be inferred as Root: that
        // inference is exactly the heuristic this contract exists to
        // prevent (see `LineageStatus::Unknown`'s own doc comment). Any
        // lineage claim without a reported `agent_id` is refused, not
        // silently coerced to `Unknown` or accepted as `Root`.
        if identity.agent_id().is_none() && identity.lineage_status() != LineageStatus::Unknown {
            return Err(AssociationUnavailable::Unsupported);
        }
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
        // Persistent bytes must not depend on dependency map-order features.
        object.sort_keys();
        let exact =
            serde_json::to_string(&wire).map_err(|_| AssociationUnavailable::Unsupported)?;
        let object = wire
            .as_object_mut()
            .ok_or(AssociationUnavailable::Unsupported)?;
        object.remove("turn_id");
        object.sort_keys();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ExecutionIdentityBuilder;

    #[test]
    fn durable_position_keeps_the_same_encoding_across_json_map_features() {
        let identity = ExecutionIdentityBuilder::new("host", "codex")
            .provider_session_id("session")
            .agent_id("agent")
            .turn_id("turn")
            .tool_instance_id("instance")
            .child_lineage("parent")
            .session_lineage_id("lineage")
            .repo_id("repo")
            .worktree_id("worktree")
            .build_at(time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap())
            .unwrap();
        let position = ExecutionPosition::from_identity(&identity).unwrap();
        assert_eq!(position.lane, r#"["host","codex","session","agent"]"#);
        assert_eq!(position.turn, "turn");
        assert_eq!(
            position.exact,
            r#"{"agent_id":"agent","envelope_version":1,"host_id":"host","lineage_status":"child","parent_agent_id":"parent","provider_session_id":"session","repo_id":"repo","session_lineage_id":"lineage","tool_instance_id":"instance","tool_provider":"codex","turn_id":"turn","worktree_id":"worktree"}"#
        );
        assert_eq!(
            position.context,
            r#"{"agent_id":"agent","envelope_version":1,"host_id":"host","lineage_status":"child","parent_agent_id":"parent","provider_session_id":"session","repo_id":"repo","session_lineage_id":"lineage","tool_instance_id":"instance","tool_provider":"codex","worktree_id":"worktree"}"#
        );
    }

    #[test]
    fn agent_absent_event_is_accepted_as_its_own_distinct_lane_not_refused() {
        // HORO-1714 decision A: session+turn alone is enough to establish a
        // position; agent_id is genuinely optional, unlike `Scope::TurnTask`.
        let identity = ExecutionIdentityBuilder::new("host", "codex")
            .provider_session_id("session")
            .turn_id("turn")
            .build_at(time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap())
            .unwrap();
        let position = ExecutionPosition::from_identity(&identity).unwrap();
        // JSON `null`, never the literal string `"null"` -- the array's own
        // type system keeps an agent-absent lane permanently distinct from
        // an agent that happened to be *named* "null".
        assert_eq!(position.lane, r#"["host","codex","session",null]"#);
        assert_ne!(position.lane, r#"["host","codex","session","null"]"#);
        assert_eq!(position.turn, "turn");
    }

    #[test]
    fn agent_absent_event_claiming_root_lineage_is_refused_not_inferred() {
        // Decision A: missing agent_id must never be read as Root. The
        // builder can't construct lineage_status=Root without an agent_id
        // being physically present in the wire (Root/Child both require it
        // downstream in the real adapters), but this proves the owner's own
        // check refuses the combination directly, independent of whether
        // any caller could even construct it today.
        let identity = ExecutionIdentityBuilder::new("host", "codex")
            .provider_session_id("session")
            .turn_id("turn")
            .root_lineage()
            .build_at(time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap())
            .unwrap();
        assert!(identity.agent_id().is_none());
        assert_eq!(identity.lineage_status(), LineageStatus::Root);
        let result = ExecutionPosition::from_identity(&identity);
        assert_eq!(result, Err(AssociationUnavailable::Unsupported));
    }

    #[test]
    fn missing_session_or_turn_is_unsupported_even_with_an_agent_id() {
        let identity = ExecutionIdentityBuilder::new("host", "codex")
            .agent_id("agent")
            .build_at(time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap())
            .unwrap();
        assert_eq!(
            ExecutionPosition::from_identity(&identity),
            Err(AssociationUnavailable::Unsupported)
        );
    }
}
