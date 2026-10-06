//! Owner IPC. Native context and identity must come from the same local
//! acquisition. Socket permissions are the authority boundary; an adapter's
//! diagnostic or externally normalized claim is not native authentication.
use crate::{BudgetSnapshot, FinalizeOutcome, PreflightResult};
use libra_governor_domain::{AssociationUnavailable, ExecutionIdentity, ExecutionTarget};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionOwnerRequest {
    pub association_version: u32,
    #[serde(deserialize_with = "strict_identity")]
    pub identity: ExecutionIdentity,
    pub native_context: NativeExecutionContext,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeExecutionContext {
    #[serde(deserialize_with = "strict_identity")]
    pub identity: ExecutionIdentity,
    pub operation: ExecutionOperation,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionOperation {
    Prompt {
        task_hint: String,
        cwd: PathBuf,
        supersedes_turn: Option<String>,
    },
    Tool {
        native_call_id: String,
        tool_name: String,
    },
    Stop {
        model: Option<String>,
        transcript_path: Option<String>,
    },
    Query {},
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ExecutionOwnerOutcome {
    Unavailable {
        reason: AssociationUnavailable,
    },
    Resolved {
        target: ExecutionTarget,
        budget: Option<BudgetSnapshot>,
    },
    Applied {
        target: ExecutionTarget,
        effect: Box<ExecutionEffect>,
    },
    Duplicate {
        target: ExecutionTarget,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "effect", rename_all = "snake_case")]
pub enum ExecutionEffect {
    Prompt { result: Box<PreflightResult> },
    Tool,
    Stop { result: Box<FinalizeOutcome> },
}

// The shared v1 envelope deliberately accepts unknown fields for record
// compatibility. Owner IPC must instead reject any unhandled richer dimension.
fn strict_identity<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<ExecutionIdentity, D::Error> {
    let value = serde_json::Value::deserialize(deserializer)?;
    let object = value
        .as_object()
        .ok_or_else(|| serde::de::Error::custom("identity must be an object"))?;
    let allowed = [
        "envelope_version",
        "observed_at",
        "host_id",
        "tool_provider",
        "tool_instance_id",
        "provider_session_id",
        "agent_id",
        "turn_id",
        "lineage_status",
        "parent_agent_id",
        "session_lineage_id",
        "event_id",
        "repo_id",
        "worktree_id",
    ];
    if object
        .keys()
        .any(|field| !allowed.contains(&field.as_str()))
    {
        return Err(serde::de::Error::custom("unsupported identity dimension"));
    }
    serde_json::from_value(value).map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;
    use libra_governor_domain::ExecutionIdentityBuilder;
    #[test]
    fn full_owner_identity_roundtrips_and_unhandled_richer_identity_is_rejected() {
        let identity = ExecutionIdentityBuilder::new("fixture-host", "codex")
            .provider_session_id("session")
            .agent_id("agent")
            .turn_id("turn")
            .tool_instance_id("instance")
            .session_lineage_id("explicit-lineage")
            .repo_id("repo")
            .worktree_id("worktree")
            .build_at(time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap())
            .unwrap();
        let request = ExecutionOwnerRequest {
            association_version: 1,
            identity: identity.clone(),
            native_context: NativeExecutionContext {
                identity,
                operation: ExecutionOperation::Query {},
            },
        };
        let wire = serde_json::to_value(&request).unwrap();
        assert_eq!(
            serde_json::from_value::<ExecutionOwnerRequest>(wire.clone()).unwrap(),
            request
        );
        for field in ["native_context", "operation"] {
            let mut richer = wire.clone();
            let target = if field == "native_context" {
                &mut richer["native_context"]
            } else {
                &mut richer["native_context"]["operation"]
            };
            target["unhandled_context"] = serde_json::json!("cannot-be-discarded");
            assert!(serde_json::from_value::<ExecutionOwnerRequest>(richer).is_err());
        }
        for field in ["identity", "native_context"] {
            let mut richer = wire.clone();
            let target = if field == "identity" {
                &mut richer["identity"]
            } else {
                &mut richer["native_context"]["identity"]
            };
            target["future_native_dimension"] = serde_json::json!("cannot-be-discarded");
            assert!(serde_json::from_value::<ExecutionOwnerRequest>(richer).is_err());
        }
    }
    #[test]
    fn owner_request_rejects_unhandled_outer_context() {
        let identity = ExecutionIdentityBuilder::new("fixture-host", "codex")
            .provider_session_id("session")
            .agent_id("agent")
            .turn_id("turn")
            .build_at(time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap())
            .unwrap();
        let envelope = crate::RequestEnvelope {
            protocol_version: crate::PROTOCOL_VERSION,
            request: crate::Request::ExecutionOwner {
                event: Box::new(ExecutionOwnerRequest {
                    association_version: 1,
                    identity: identity.clone(),
                    native_context: NativeExecutionContext {
                        identity,
                        operation: ExecutionOperation::Query {},
                    },
                }),
            },
        };
        let wire = serde_json::to_value(envelope).unwrap();
        assert!(serde_json::from_value::<crate::RequestEnvelope>(wire.clone()).is_ok());
        assert_eq!(
            wire["request"]["event"]["native_context"]["operation"],
            serde_json::json!({"operation": "query"})
        );
        let mut richer_query = wire.clone();
        richer_query["request"]["event"]["native_context"]["operation"]["extra"] =
            serde_json::json!("cannot-be-discarded");
        assert!(serde_json::from_value::<crate::RequestEnvelope>(richer_query).is_err());
        for nested in [false, true] {
            let mut richer = wire.clone();
            let object = if nested {
                &mut richer["request"]
            } else {
                &mut richer
            };
            object["unhandled_native_context"] = serde_json::json!("cannot-be-discarded");
            assert!(serde_json::from_value::<crate::RequestEnvelope>(richer).is_err());
        }
    }
}
