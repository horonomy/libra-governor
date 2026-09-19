//! Asserts `docs/api/examples/*.json` byte-for-byte against a fresh
//! construction of each wire/event type, using the identical fixed
//! ids/timestamps as `crates/extension/examples/dump_goldens.rs`
//! (deliberately duplicated rather than shared — an example binary and a
//! test crate don't share a compilation unit, and the duplication is
//! small and self-checking: any drift fails this test).
//!
//! Regenerate the golden files with:
//! `cargo run -p libra-governor-extension --example dump_goldens`

use libra_governor_domain::{
    Admission, ApprovalRequest, Confidence, ConstraintOutcome, PlanId, ResourceAmount, TaskId,
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

fn golden_path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/api/examples")
        .join(name)
}

fn assert_matches_golden(name: &str, json: String) {
    let expected = std::fs::read_to_string(golden_path(name))
        .unwrap_or_else(|e| panic!("failed to read golden file {name}: {e}"));
    let actual = json + "\n";
    assert_eq!(
        actual, expected,
        "{name} no longer matches its golden fixture — if this change is \
         intentional, regenerate with `cargo run -p libra-governor-extension \
         --example dump_goldens` and review the diff before committing"
    );
}

#[test]
fn business_context_request_matches_golden() {
    let value = BusinessContextRequest::new(
        "00000000-0000-0000-0000-000000000004".to_string(),
        fixed_task_id(),
        "sess-1".to_string(),
        "a1b2c3d4e5f6".to_string(),
        std::path::PathBuf::from("/home/user/repo"),
        fixed_time(),
    );
    assert_matches_golden(
        "business_context_request.json",
        serde_json::to_string_pretty(&value).unwrap(),
    );
}

#[test]
fn policy_webhook_request_matches_golden() {
    let value = PolicyWebhookRequest::new(
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
    );
    assert_matches_golden(
        "policy_webhook_request.json",
        serde_json::to_string_pretty(&value).unwrap(),
    );
}

#[test]
fn event_admission_matches_golden() {
    let value = EventEnvelope::new(
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
    )
    .with_event_id(fixed_event_id());
    assert_matches_golden(
        "event_admission.json",
        serde_json::to_string_pretty(&value).unwrap(),
    );
}

#[test]
fn event_replan_matches_golden() {
    let value = EventEnvelope::new(
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
    )
    .with_event_id(fixed_event_id());
    assert_matches_golden(
        "event_replan.json",
        serde_json::to_string_pretty(&value).unwrap(),
    );
}

#[test]
fn event_approval_matches_golden() {
    let value = EventEnvelope::new(
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
    )
    .with_event_id(fixed_event_id());
    assert_matches_golden(
        "event_approval.json",
        serde_json::to_string_pretty(&value).unwrap(),
    );
}

#[test]
fn event_outcome_matches_golden() {
    let value = EventEnvelope::new(
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
    )
    .with_event_id(fixed_event_id());
    assert_matches_golden(
        "event_outcome.json",
        serde_json::to_string_pretty(&value).unwrap(),
    );
}
