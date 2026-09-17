//! Acceptance-level integration tests for `LedgerStore`, covering the
//! HORO-1124 test matrix: stable multi-session task identity, idempotent
//! event insertion, contract version/plan association, crash/reopen
//! persistence, privacy-safe serialization, and full trajectory
//! reconstruction.

use libra_governor_domain::{
    CompletionContract, CompletionCriterion, ExecutionEvent, ExecutionEventKind, ExecutionOutcome,
    ExecutionPlan, ExecutionReceipt, ExternalRef, ResourceAmount, TaskIdentity,
};
use libra_governor_ledger::LedgerStore;
use time::OffsetDateTime;
use uuid::Uuid;

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_757_000_000).unwrap()
}

#[test]
fn multiple_sessions_attach_to_one_stable_task_identity() {
    let mut store = LedgerStore::open_in_memory().unwrap();
    let identity = TaskIdentity::new(Some(ExternalRef::Jira("HORO-1124".to_string())));
    store.insert_task(&identity, now()).unwrap();

    let session_a = ExecutionEvent::new(
        identity.id,
        now(),
        ExecutionEventKind::SessionStarted {
            session_id: "session-a".to_string(),
        },
    );
    let session_b = ExecutionEvent::new(
        identity.id,
        now() + time::Duration::minutes(90),
        ExecutionEventKind::SessionStarted {
            session_id: "session-b".to_string(),
        },
    );
    store.insert_event(Uuid::new_v4(), &session_a).unwrap();
    store.insert_event(Uuid::new_v4(), &session_b).unwrap();

    let trajectory = store.task_trajectory(identity.id).unwrap();
    assert_eq!(trajectory.task, identity);
    assert_eq!(trajectory.events.len(), 2);
    let session_ids: Vec<String> = trajectory
        .events
        .iter()
        .map(|e| match &e.kind {
            ExecutionEventKind::SessionStarted { session_id } => session_id.clone(),
            other => panic!("unexpected event kind: {other:?}"),
        })
        .collect();
    assert_eq!(session_ids, vec!["session-a", "session-b"]);
}

#[test]
fn reinserting_the_same_event_id_is_idempotent() {
    let mut store = LedgerStore::open_in_memory().unwrap();
    let identity = TaskIdentity::new(None);
    store.insert_task(&identity, now()).unwrap();

    let event = ExecutionEvent::new(
        identity.id,
        now(),
        ExecutionEventKind::UserPromptSubmitted {
            session_id: "session-a".to_string(),
        },
    );
    let event_id = Uuid::new_v4();
    store.insert_event(event_id, &event).unwrap();
    store.insert_event(event_id, &event).unwrap();

    let trajectory = store.task_trajectory(identity.id).unwrap();
    assert_eq!(
        trajectory.events.len(),
        1,
        "re-applying the same event id must not duplicate the event"
    );
}

#[test]
fn plan_ties_to_the_exact_contract_revision_it_assumed() {
    let mut store = LedgerStore::open_in_memory().unwrap();
    let identity = TaskIdentity::new(None);
    store.insert_task(&identity, now()).unwrap();

    let v1 = CompletionContract::first(vec![CompletionCriterion::required("tests pass")]);
    store.insert_contract(identity.id, &v1, now()).unwrap();
    let v2 = v1.next_revision(vec![
        CompletionCriterion::required("tests pass"),
        CompletionCriterion::optional("docs updated"),
    ]);
    store.insert_contract(identity.id, &v2, now()).unwrap();

    let plan = ExecutionPlan::new(identity.id, v1.revision, Some("snap-1".to_string()), now());
    store.insert_plan(&plan).unwrap();

    let trajectory = store.task_trajectory(identity.id).unwrap();
    assert_eq!(
        trajectory.contracts.len(),
        2,
        "changing the contract must not mutate history"
    );
    assert_eq!(trajectory.plans.len(), 1);
    assert_eq!(
        trajectory.plans[0].contract_revision, 1,
        "the plan must remain tied to the contract revision it assumed, not the latest one"
    );
}

#[test]
fn data_survives_closing_and_reopening_the_same_database_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");

    let identity = TaskIdentity::new(Some(ExternalRef::GitHub(
        "https://github.com/horonomy/libra-governor/pull/1".to_string(),
    )));
    {
        let mut store = LedgerStore::open(&path).unwrap();
        store.insert_task(&identity, now()).unwrap();
        let event = ExecutionEvent::new(
            identity.id,
            now(),
            ExecutionEventKind::SessionStarted {
                session_id: "session-a".to_string(),
            },
        );
        store.insert_event(Uuid::new_v4(), &event).unwrap();
        // Store (and its connection) is dropped at the end of this block.
    }

    let reopened = LedgerStore::open(&path).unwrap();
    let trajectory = reopened.task_trajectory(identity.id).unwrap();
    assert_eq!(trajectory.task, identity);
    assert_eq!(trajectory.events.len(), 1);
}

#[test]
fn execution_event_kind_has_no_field_for_raw_prompt_or_output_content() {
    // Structural proof, not a convention check: ToolInvoked only accepts a
    // tool name, so there is no field to (mis)use for raw arguments or
    // output, and this test would fail to compile if one were added
    // without updating the test to populate it.
    let event = ExecutionEventKind::ToolInvoked {
        session_id: "session-a".to_string(),
        tool_name: "Bash".to_string(),
    };
    let json = serde_json::to_value(&event).unwrap();
    let fields: Vec<&str> = json
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        fields.len(),
        3,
        "ToolInvoked must only ever serialize kind/session_id/tool_name: {fields:?}"
    );
    assert!(!fields.contains(&"raw_output"));
    assert!(!fields.contains(&"prompt"));
    assert!(!fields.contains(&"content"));
}

#[test]
fn receipt_query_reconstructs_full_task_trajectory_from_sqlite_alone() {
    let mut store = LedgerStore::open_in_memory().unwrap();
    let identity = TaskIdentity::new(Some(ExternalRef::Jira("HORO-1124".to_string())));
    store.insert_task(&identity, now()).unwrap();

    let contract = CompletionContract::first(vec![CompletionCriterion::required("tests pass")]);
    store
        .insert_contract(identity.id, &contract, now())
        .unwrap();

    let started = ExecutionEvent::new(
        identity.id,
        now(),
        ExecutionEventKind::SessionStarted {
            session_id: "session-a".to_string(),
        },
    );
    let ended = ExecutionEvent::new(
        identity.id,
        now() + time::Duration::minutes(30),
        ExecutionEventKind::SessionEnded {
            session_id: "session-a".to_string(),
        },
    );
    store.insert_event(Uuid::new_v4(), &started).unwrap();
    store.insert_event(Uuid::new_v4(), &ended).unwrap();

    let plan = ExecutionPlan::new(identity.id, contract.revision, None, now());
    store.insert_plan(&plan).unwrap();

    let receipt = ExecutionReceipt::new(
        identity.id,
        contract.revision,
        plan.id,
        1800,
        vec![ResourceAmount::Tokens(12_000)],
        ExecutionOutcome::Completed {
            evidence: vec!["https://github.com/horonomy/libra-governor/pull/1".to_string()],
        },
        now() + time::Duration::minutes(31),
    );
    store.insert_receipt(&receipt).unwrap();

    let trajectory = store.task_trajectory(identity.id).unwrap();
    assert_eq!(trajectory.task, identity);
    assert_eq!(trajectory.events.len(), 2);
    assert_eq!(trajectory.events[0].kind, started.kind);
    assert_eq!(trajectory.events[1].kind, ended.kind);
    assert_eq!(trajectory.contracts, vec![contract]);
    assert_eq!(trajectory.plans, vec![plan]);
    assert_eq!(trajectory.receipts, vec![receipt]);
}
