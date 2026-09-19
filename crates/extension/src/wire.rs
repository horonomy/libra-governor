//! Wire request/response types for the business-context and
//! policy-webhook surfaces (HORO-1174).
//!
//! # No field for prompt text — structurally, not by convention
//!
//! [`BusinessContextRequest`] has no field that could carry prompt text,
//! source code, or tool output. This is checked by
//! `business_context_request_field_set_pins_no_prompt_field` below, the
//! same field-set-pinning discipline `TaskFeatures` established.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use libra_governor_domain::{
    ApprovalRequest, Confidence, ExternalRef, PlanId, Priority, ResourceAmount, TaskId,
};

use crate::config::WIRE_SCHEMA_VERSION;

// ---------------------------------------------------------------------
// Business Context surface
// ---------------------------------------------------------------------

/// `POST <business_context_provider.url>` request body.
#[derive(Debug, Clone, Serialize)]
pub struct BusinessContextRequest {
    pub schema_version: String,
    pub request_id: String,
    pub task_id: TaskId,
    pub session_id: String,
    pub repo_key: String,
    pub cwd: std::path::PathBuf,
    #[serde(with = "time::serde::rfc3339")]
    pub occurred_at: OffsetDateTime,
}

impl BusinessContextRequest {
    pub fn new(
        request_id: String,
        task_id: TaskId,
        session_id: String,
        repo_key: String,
        cwd: std::path::PathBuf,
        occurred_at: OffsetDateTime,
    ) -> Self {
        Self {
            schema_version: WIRE_SCHEMA_VERSION.to_string(),
            request_id,
            task_id,
            session_id,
            repo_key,
            cwd,
            occurred_at,
        }
    }
}

/// `POST <business_context_provider.url>` response body.
#[derive(Debug, Clone, Deserialize)]
pub struct BusinessContextResponse {
    pub schema_version: String,
    pub provider_id: String,
    #[serde(default)]
    pub priority: Option<Priority>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub deadline: Option<OffsetDateTime>,
    #[serde(default)]
    pub cost_center: Option<String>,
    #[serde(default)]
    pub advisory_criteria: Vec<String>,
    #[serde(default)]
    pub external_refs: Vec<ExternalRef>,
}

// ---------------------------------------------------------------------
// Policy Webhook surface
// ---------------------------------------------------------------------

/// `POST <policy_webhook.url>` request body. Only ever sent when
/// `decision.admission == ApprovalRequired` — see
/// `crates/daemon/src/server.rs::handle_preflight`.
#[derive(Debug, Clone, Serialize)]
pub struct PolicyWebhookRequest {
    pub schema_version: String,
    pub request_id: String,
    pub task_id: TaskId,
    pub plan_id: PlanId,
    pub session_id: String,
    pub policy_name: String,
    pub policy_schema_version: String,
    pub approval_requests: Vec<ApprovalRequest>,
    pub projected_resource: ResourceAmount,
    pub projected_duration_secs: u64,
    pub confidence: Confidence,
    #[serde(with = "time::serde::rfc3339")]
    pub occurred_at: OffsetDateTime,
}

#[allow(clippy::too_many_arguments)]
impl PolicyWebhookRequest {
    pub fn new(
        request_id: String,
        task_id: TaskId,
        plan_id: PlanId,
        session_id: String,
        policy_name: String,
        policy_schema_version: String,
        approval_requests: Vec<ApprovalRequest>,
        projected_resource: ResourceAmount,
        projected_duration_secs: u64,
        confidence: Confidence,
        occurred_at: OffsetDateTime,
    ) -> Self {
        Self {
            schema_version: WIRE_SCHEMA_VERSION.to_string(),
            request_id,
            task_id,
            plan_id,
            session_id,
            policy_name,
            policy_schema_version,
            approval_requests,
            projected_resource,
            projected_duration_secs,
            confidence,
            occurred_at,
        }
    }
}

/// The raw verdict string on a [`PolicyWebhookResponse`]. Deserializing
/// this into a typed enum means an unrecognized verdict string fails the
/// whole response parse — which the client treats identically to any
/// other malformed response: fail-open as
/// [`libra_governor_domain::ExternalVerdict::Abstain`]. See
/// `crate::client` docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireVerdict {
    Approve,
    Reject,
    Abstain,
}

/// `POST <policy_webhook.url>` response body.
#[derive(Debug, Clone, Deserialize)]
pub struct PolicyWebhookResponse {
    pub schema_version: String,
    pub provider_id: String,
    pub verdict: WireVerdict,
    #[serde(default)]
    pub reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn business_context_request_field_set_pins_no_prompt_field() {
        let request = BusinessContextRequest::new(
            "req-1".to_string(),
            TaskId::new(),
            "sess-1".to_string(),
            "abc123".to_string(),
            std::path::PathBuf::from("/repo"),
            OffsetDateTime::UNIX_EPOCH,
        );
        let json = serde_json::to_value(&request).unwrap();
        let fields: BTreeSet<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        let expected: BTreeSet<&str> = [
            "schema_version",
            "request_id",
            "task_id",
            "session_id",
            "repo_key",
            "cwd",
            "occurred_at",
        ]
        .into_iter()
        .collect();
        assert_eq!(fields, expected);
        for forbidden in ["prompt", "task_hint", "body", "tool_output"] {
            assert!(!fields.contains(forbidden));
        }
    }

    #[test]
    fn business_context_response_deserializes_a_real_provider_shape() {
        let json = r#"{
            "schema_version": "libra.extension.v1",
            "provider_id": "example-provider",
            "priority": "high",
            "deadline": "2026-12-31T00:00:00Z",
            "cost_center": "eng-platform",
            "advisory_criteria": ["check the thing"],
            "external_refs": [{"kind": "jira", "value": "HORO-1174"}]
        }"#;
        let response: BusinessContextResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.provider_id, "example-provider");
        assert_eq!(
            response.advisory_criteria,
            vec!["check the thing".to_string()]
        );
        assert_eq!(response.external_refs.len(), 1);
    }

    #[test]
    fn business_context_response_tolerates_absent_optional_fields() {
        let json = r#"{"schema_version": "libra.extension.v1", "provider_id": "p"}"#;
        let response: BusinessContextResponse = serde_json::from_str(json).unwrap();
        assert!(response.priority.is_none());
        assert!(response.advisory_criteria.is_empty());
    }

    #[test]
    fn policy_webhook_response_deserializes_every_verdict() {
        for (verdict, expected) in [
            ("approve", WireVerdict::Approve),
            ("reject", WireVerdict::Reject),
            ("abstain", WireVerdict::Abstain),
        ] {
            let json = format!(
                r#"{{"schema_version":"libra.extension.v1","provider_id":"p","verdict":"{verdict}","reason":null}}"#
            );
            let response: PolicyWebhookResponse = serde_json::from_str(&json).unwrap();
            assert_eq!(response.verdict, expected);
        }
    }

    #[test]
    fn policy_webhook_response_with_an_unknown_verdict_fails_to_parse() {
        let json = r#"{"schema_version":"libra.extension.v1","provider_id":"p","verdict":"maybe"}"#;
        assert!(serde_json::from_str::<PolicyWebhookResponse>(json).is_err());
    }
}
