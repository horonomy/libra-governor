//! [`EventEnvelope`]/[`EventKind`] and the four per-kind event payload
//! structs delivered to the `events` surface (HORO-1174).
//!
//! # Privacy — scalars, ids, and closed-set enums only
//!
//! No payload struct here has a field for a `cwd`, a path, a prompt, or a
//! tool-output body — see the `*_field_set_pins_no_privacy_leaking_field`
//! tests below, one per kind, mirroring `TaskFeatures`'s own field-set
//! pinning discipline.
//!
//! # No `tool_invoked` event kind — deliberate non-goal
//!
//! `Request::ToolInvoked` promises no perceptible latency on every tool
//! call (see `crates/cli/src/client.rs::fire_and_forget`'s docs and
//! `crates/daemon/src/server.rs::handle_tool_invoked`'s own comment on
//! this exact contract). A durable enqueue on every tool call — even a
//! cheap SQLite insert — is still I/O on a path that must stay as close
//! to zero added latency as the fire-and-forget write itself allows, and
//! `handle_tool_invoked` already does more local computation per call
//! than the daemon can spare a webhook enqueue on top of. Every tool call
//! that matters downstream already surfaces through the `replan` event
//! kind when (and only when) a material replan actually happens.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use libra_governor_domain::{
    Admission, ApprovalRequest, ConstraintOutcome, PlanId, ResourceAmount, TaskId,
};

use crate::config::WIRE_SCHEMA_VERSION;

/// Which event kind an [`EventEnvelope`] carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Admission,
    Replan,
    Approval,
    Outcome,
}

impl EventKind {
    /// Parses a `config.json` `extensions.events.kinds` entry. Unknown
    /// strings are dropped (fail-open on the config surface) rather than
    /// aborting the whole `[extensions]` block over one typo.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "admission" => Some(Self::Admission),
            "replan" => Some(Self::Replan),
            "approval" => Some(Self::Approval),
            "outcome" => Some(Self::Outcome),
            _ => None,
        }
    }

    pub fn as_dedupe_key_prefix(&self) -> &'static str {
        match self {
            Self::Admission => "admission",
            Self::Replan => "replan",
            Self::Approval => "approval",
            Self::Outcome => "outcome",
        }
    }
}

/// A provider-observable reference to a fetched business context — never
/// the full [`libra_governor_domain::BusinessContextSummary`] (which
/// carries advisory criteria and a cost center a provider does not need
/// echoed back to it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BusinessContextEventRef {
    pub provider_id: String,
    pub applied: bool,
}

/// A provider-observable reference to an external approval verdict.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExternalApprovalEventRef {
    pub provider_id: String,
    /// `"approve"` / `"reject"` / `"abstain"` — a closed-set scalar, not
    /// the full [`libra_governor_domain::ExternalVerdict`] (which for
    /// `Reject` carries a free-text reason we do not echo back here).
    pub verdict: String,
}

/// `admission` event payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdmissionEventData {
    pub task_id: TaskId,
    pub plan_id: PlanId,
    pub session_id: String,
    /// The final admission, after any policy-webhook resolution.
    pub admission: Admission,
    pub resource_outcome: ConstraintOutcome,
    pub time_outcome: ConstraintOutcome,
    pub confidence_ok: bool,
    pub projected_resource: ResourceAmount,
    pub projected_duration_secs: u64,
    pub policy_name: String,
    pub policy_schema_version: String,
    pub business_context: Option<BusinessContextEventRef>,
    pub external_approval: Option<ExternalApprovalEventRef>,
}

/// `replan` event payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplanEventData {
    pub task_id: TaskId,
    pub prior_plan_id: PlanId,
    pub new_plan_id: PlanId,
    pub trigger: libra_governor_domain::ReplanTriggerKind,
    pub detail: String,
    pub auto_replan_count: u32,
    pub remaining_duration_p80_secs: Option<u64>,
    pub remaining_confidence: libra_governor_domain::Confidence,
}

/// `approval` event payload — emitted only when the final admission is
/// still `ApprovalRequired` (a human must still act).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalEventData {
    pub task_id: TaskId,
    pub plan_id: PlanId,
    pub approval_requests: Vec<ApprovalRequest>,
    pub external_approval: Option<ExternalApprovalEventRef>,
}

/// `outcome` event payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutcomeEventData {
    pub task_id: TaskId,
    pub plan_id: Option<PlanId>,
    /// The closed-set tag of the recorded `ExecutionOutcome` —
    /// `"completed"` / `"failed"` / `"aborted"` / `"unknown"`.
    pub outcome_kind: String,
    /// Evidence *references* only (URLs, ids) — never inlined content,
    /// same discipline as `ExecutionOutcome::evidence()`.
    pub evidence: Vec<String>,
    /// The closed-set tag of the recorded `AttestationSource` —
    /// `"provider"` / `"governor_local"` / `"agent"`.
    pub source: String,
    pub source_id: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub attested_at: OffsetDateTime,
}

/// The envelope every delivered event is wrapped in.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope<T> {
    pub schema_version: String,
    pub event_id: Uuid,
    pub event_kind: EventKind,
    #[serde(with = "time::serde::rfc3339")]
    pub occurred_at: OffsetDateTime,
    pub daemon_version: String,
    pub data: T,
}

impl<T: Serialize> EventEnvelope<T> {
    /// Builds a fresh envelope with a newly minted `event_id`. The
    /// caller (the daemon) serializes this once, at enqueue time, and
    /// stores the exact bytes in `webhook_deliveries.payload_json` — a
    /// retry resends those bytes unchanged; only the HMAC headers are
    /// recomputed per attempt (see `crate::sign` docs).
    pub fn new(
        kind: EventKind,
        daemon_version: impl Into<String>,
        data: T,
        now: OffsetDateTime,
    ) -> Self {
        Self {
            schema_version: WIRE_SCHEMA_VERSION.to_string(),
            event_id: Uuid::new_v4(),
            event_kind: kind,
            occurred_at: now,
            daemon_version: daemon_version.into(),
            data,
        }
    }

    /// Overrides the `event_id` [`Self::new`] minted. Exists for
    /// reproducible golden-fixture generation
    /// (`crates/extension/examples/dump_goldens.rs`) and tests — normal
    /// production use always keeps the freshly minted id.
    pub fn with_event_id(mut self, event_id: Uuid) -> Self {
        self.event_id = event_id;
        self
    }

    pub fn to_json_bytes(&self) -> serde_json::Result<Vec<u8>> {
        serde_json::to_vec(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libra_governor_domain::{Confidence, ReplanTriggerKind};
    use std::collections::BTreeSet;

    fn field_set(value: &impl Serialize) -> BTreeSet<String> {
        let json = serde_json::to_value(value).unwrap();
        json.as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>()
    }

    fn assert_no_privacy_leaking_field(fields: &BTreeSet<String>) {
        for forbidden in ["cwd", "path", "prompt", "body", "tool_output", "file_paths"] {
            assert!(
                !fields.contains(forbidden),
                "event payload must never carry a `{forbidden}` field: {fields:?}"
            );
        }
    }

    #[test]
    fn admission_event_field_set_pins_no_privacy_leaking_field() {
        let data = AdmissionEventData {
            task_id: TaskId::new(),
            plan_id: PlanId::new(),
            session_id: "sess-1".to_string(),
            admission: Admission::Admit,
            resource_outcome: ConstraintOutcome::Admit,
            time_outcome: ConstraintOutcome::Admit,
            confidence_ok: true,
            projected_resource: ResourceAmount::Tokens(1000),
            projected_duration_secs: 600,
            policy_name: "balanced".to_string(),
            policy_schema_version: "policy-v1".to_string(),
            business_context: None,
            external_approval: None,
        };
        let fields = field_set(&data);
        assert_no_privacy_leaking_field(&fields);
        let expected: BTreeSet<String> = [
            "task_id",
            "plan_id",
            "session_id",
            "admission",
            "resource_outcome",
            "time_outcome",
            "confidence_ok",
            "projected_resource",
            "projected_duration_secs",
            "policy_name",
            "policy_schema_version",
            "business_context",
            "external_approval",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert_eq!(fields, expected);
    }

    #[test]
    fn replan_event_field_set_pins_no_privacy_leaking_field() {
        let data = ReplanEventData {
            task_id: TaskId::new(),
            prior_plan_id: PlanId::new(),
            new_plan_id: PlanId::new(),
            trigger: ReplanTriggerKind::ToolCallCountExceeded,
            detail: "resource projection exceeded target by >material threshold".to_string(),
            auto_replan_count: 1,
            remaining_duration_p80_secs: Some(300),
            remaining_confidence: Confidence::Medium,
        };
        assert_no_privacy_leaking_field(&field_set(&data));
    }

    #[test]
    fn approval_event_field_set_pins_no_privacy_leaking_field() {
        let data = ApprovalEventData {
            task_id: TaskId::new(),
            plan_id: PlanId::new(),
            approval_requests: vec![],
            external_approval: None,
        };
        assert_no_privacy_leaking_field(&field_set(&data));
    }

    #[test]
    fn outcome_event_field_set_pins_no_privacy_leaking_field() {
        let data = OutcomeEventData {
            task_id: TaskId::new(),
            plan_id: Some(PlanId::new()),
            outcome_kind: "completed".to_string(),
            evidence: vec!["https://ci.example.com/1".to_string()],
            source: "provider".to_string(),
            source_id: Some("example-provider".to_string()),
            attested_at: OffsetDateTime::UNIX_EPOCH,
        };
        assert_no_privacy_leaking_field(&field_set(&data));
    }

    #[test]
    fn envelope_round_trips_through_json() {
        let data = OutcomeEventData {
            task_id: TaskId::new(),
            plan_id: None,
            outcome_kind: "unknown".to_string(),
            evidence: vec![],
            source: "governor_local".to_string(),
            source_id: None,
            attested_at: OffsetDateTime::UNIX_EPOCH,
        };
        let envelope = EventEnvelope::new(
            EventKind::Outcome,
            "0.0.1",
            data,
            OffsetDateTime::UNIX_EPOCH,
        );
        let bytes = envelope.to_json_bytes().unwrap();
        let round_tripped: EventEnvelope<OutcomeEventData> =
            serde_json::from_slice(&bytes).unwrap();
        assert_eq!(round_tripped.event_id, envelope.event_id);
        assert_eq!(round_tripped.schema_version, WIRE_SCHEMA_VERSION);
    }

    #[test]
    fn event_kind_parse_is_lenient_to_unknown_strings() {
        assert_eq!(EventKind::parse("admission"), Some(EventKind::Admission));
        assert_eq!(EventKind::parse("not-a-real-kind"), None);
    }
}
