//! Regenerates `docs/api/examples/*.json` — the golden wire-contract
//! bodies `crates/extension/tests/contract_goldens.rs` asserts byte-for-
//! byte against. Run with `cargo run -p libra-governor-extension --example
//! dump_goldens` after any change to a wire type's field set, then review
//! the diff.
//!
//! Every id/timestamp here is a fixed, documented constant — never
//! `Uuid::new_v4()`/`OffsetDateTime::now_utc()` — so the output is
//! reproducible and diffable across runs.

use libra_governor_domain::{
    Admission, ApprovalRequest, Confidence, ConstraintOutcome, ExternalRef, PlanId, Priority,
    ResourceAmount, TaskId,
};
use libra_governor_extension::{
    AdmissionEventData, ApprovalEventData, BusinessContextEventRef, BusinessContextRequest,
    EventEnvelope, EventKind, OutcomeEventData, PolicyWebhookRequest, ReplanEventData,
};
use time::OffsetDateTime;
use uuid::Uuid;

fn fixed_task_id() -> TaskId {
    TaskId(Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap())
}
fn fixed_plan_id() -> PlanId {
    PlanId(Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap())
}
fn fixed_event_id() -> Uuid {
    Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap()
}
fn fixed_time() -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(1_800_000_000)
}

fn write(name: &str, json: String) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/api/examples")
        .join(name);
    std::fs::write(&path, json + "\n").unwrap();
    println!("wrote {}", path.display());
}

fn main() {
    write(
        "business_context_request.json",
        serde_json::to_string_pretty(&BusinessContextRequest::new(
            "00000000-0000-0000-0000-000000000004".to_string(),
            fixed_task_id(),
            "sess-1".to_string(),
            "a1b2c3d4e5f6".to_string(),
            std::path::PathBuf::from("/home/user/repo"),
            fixed_time(),
        ))
        .unwrap(),
    );

    write(
        "business_context_response.json",
        serde_json::to_string_pretty(&serde_json::json!({
            "schema_version": "libra.extension.v1",
            "provider_id": "example-provider",
            "priority": "high",
            "deadline": fixed_time().checked_add(time::Duration::hours(4)).unwrap().format(&time::format_description::well_known::Rfc3339).unwrap(),
            "cost_center": "eng-platform",
            "advisory_criteria": ["verify the fix against the linked ticket's reproduction steps"],
            "external_refs": [
                {"kind": "jira", "value": "HORO-1174"}
            ]
        }))
        .unwrap(),
    );

    write(
        "policy_webhook_request.json",
        serde_json::to_string_pretty(&PolicyWebhookRequest::new(
            "00000000-0000-0000-0000-000000000005".to_string(),
            fixed_task_id(),
            fixed_plan_id(),
            "sess-1".to_string(),
            "balanced".to_string(),
            "policy-v1".to_string(),
            vec![ApprovalRequest::Resource {
                projected: ResourceAmount::Tokens(150_000),
                target: ResourceAmount::Tokens(100_000),
                elastic_ceiling: Some(ResourceAmount::Tokens(125_000)),
                hard_ceiling: ResourceAmount::Tokens(200_000),
            }],
            ResourceAmount::Tokens(150_000),
            2400,
            Confidence::Medium,
            fixed_time(),
        ))
        .unwrap(),
    );

    write(
        "policy_webhook_response.json",
        serde_json::to_string_pretty(&serde_json::json!({
            "schema_version": "libra.extension.v1",
            "provider_id": "example-provider",
            "verdict": "approve",
            "reason": null
        }))
        .unwrap(),
    );

    write(
        "event_admission.json",
        serde_json::to_string_pretty(&EventEnvelope::new(
            EventKind::Admission,
            "0.0.1",
            AdmissionEventData {
                task_id: fixed_task_id(),
                plan_id: fixed_plan_id(),
                session_id: "sess-1".to_string(),
                admission: Admission::Admit,
                resource_outcome: ConstraintOutcome::Admit,
                time_outcome: ConstraintOutcome::Admit,
                confidence_ok: true,
                projected_resource: ResourceAmount::Tokens(90_000),
                projected_duration_secs: 1800,
                policy_name: "balanced".to_string(),
                policy_schema_version: "policy-v1".to_string(),
                business_context: Some(BusinessContextEventRef {
                    provider_id: "example-provider".to_string(),
                    applied: true,
                }),
                external_approval: None,
            },
            fixed_time(),
        ).with_event_id(fixed_event_id()))
        .unwrap(),
    );

    write(
        "event_replan.json",
        serde_json::to_string_pretty(&EventEnvelope::new(
            EventKind::Replan,
            "0.0.1",
            ReplanEventData {
                task_id: fixed_task_id(),
                prior_plan_id: fixed_plan_id(),
                new_plan_id: PlanId(Uuid::parse_str("00000000-0000-0000-0000-000000000006").unwrap()),
                trigger: libra_governor_domain::ReplanTriggerKind::ToolCallCountExceeded,
                detail: "9 tool calls since the last replan vs. typical 5".to_string(),
                auto_replan_count: 1,
                remaining_duration_p80_secs: Some(1200),
                remaining_confidence: Confidence::Medium,
            },
            fixed_time(),
        ).with_event_id(fixed_event_id()))
        .unwrap(),
    );

    write(
        "event_approval.json",
        serde_json::to_string_pretty(&EventEnvelope::new(
            EventKind::Approval,
            "0.0.1",
            ApprovalEventData {
                task_id: fixed_task_id(),
                plan_id: fixed_plan_id(),
                approval_requests: vec![ApprovalRequest::Resource {
                    projected: ResourceAmount::Tokens(150_000),
                    target: ResourceAmount::Tokens(100_000),
                    elastic_ceiling: Some(ResourceAmount::Tokens(125_000)),
                    hard_ceiling: ResourceAmount::Tokens(200_000),
                }],
                external_approval: None,
            },
            fixed_time(),
        ).with_event_id(fixed_event_id()))
        .unwrap(),
    );

    write(
        "event_outcome.json",
        serde_json::to_string_pretty(&EventEnvelope::new(
            EventKind::Outcome,
            "0.0.1",
            OutcomeEventData {
                task_id: fixed_task_id(),
                plan_id: Some(fixed_plan_id()),
                outcome_kind: "completed".to_string(),
                evidence: vec!["https://ci.example.com/runs/42".to_string()],
                source: "provider".to_string(),
                source_id: Some("example-provider".to_string()),
                attested_at: fixed_time(),
            },
            fixed_time(),
        ).with_event_id(fixed_event_id()))
        .unwrap(),
    );

    write(
        "outcome_record_stdin.json",
        serde_json::to_string_pretty(&serde_json::json!({
            "task_id": fixed_task_id().0.to_string(),
            "plan_id": fixed_plan_id().0.to_string(),
            "source_id": "example-provider",
            "idempotency_key": "ci-run-42",
            "outcome": {
                "kind": "completed",
                "evidence": ["https://ci.example.com/runs/42"]
            }
        }))
        .unwrap(),
    );

    let _ = ExternalRef::Jira("HORO-1174".to_string());
    let _ = Priority::High;
}
