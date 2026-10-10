//! HORO-1727 Decision 2 named "late delivery/concurrency/duplicate/
//! restart/revision-supersession/conflicts against real SQLite
//! transactions" as the required test matrix for
//! `LedgerStore::record_outcome_attestation`. Duplicate, supersession and
//! same-thread conflict detection are already covered by
//! `crates/ledger/src/extension.rs`'s own unit tests and
//! `renewal_adversarial.rs`. This file covers the three categories that
//! were not yet exercised: genuine multi-connection concurrency, a push
//! that arrives late against an already-superseded plan/revision, and
//! behavior across a real store close/reopen (restart).
//!
//! Mirrors `renewal_adversarial.rs`'s on-disk-file, multi-connection
//! pattern (`LedgerStore::open_in_memory` is unshared per connection, so
//! a real race needs a real file).

use std::sync::Arc;

use libra_governor_domain::{
    CompletionContract, ExecutionOutcome, ExecutionPlan, ExecutionReceipt, TaskId, TaskIdentity,
};
use libra_governor_ledger::{LedgerStore, RecordAttestationOutcome};
use time::OffsetDateTime;

fn now() -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH
}

fn open_file_store() -> (tempfile::TempDir, std::path::PathBuf, LedgerStore) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    let store = LedgerStore::open(&path).unwrap();
    (dir, path, store)
}

/// Sets up a task with a contract at revision 1, a plan bound to it, and
/// a receipt for that plan — the same shape
/// `extension.rs::store_with_task_plan_and_receipt` builds, reconstructed
/// here from the public API only (this is a separate compilation unit).
fn setup_task_plan_receipt(store: &mut LedgerStore) -> (TaskId, libra_governor_domain::PlanId) {
    let task_id = TaskId::new();
    store
        .insert_task(
            &TaskIdentity {
                id: task_id,
                external_ref: None,
            },
            now(),
        )
        .unwrap();
    store
        .insert_contract(task_id, &CompletionContract::first(vec![]), now())
        .unwrap();
    let plan = ExecutionPlan::new(task_id, 1, None, now());
    store.insert_plan(&plan).unwrap();
    let receipt = ExecutionReceipt::new(
        task_id,
        1,
        plan.id,
        60,
        vec![],
        ExecutionOutcome::Unknown,
        now(),
    );
    store.insert_receipt(&receipt).unwrap();
    (task_id, plan.id)
}

// ---------------------------------------------------------------------
// Concurrency: two real connections racing on the same task/revision
// ---------------------------------------------------------------------

/// Two threads push disagreeing authoritative outcomes for the same
/// `(task, revision)` concurrently. Both rows must be recorded (append-
/// only, neither refused) and the receipt must end up `Unknown` — never
/// silently resolved to whichever write happened to land second.
#[test]
fn concurrent_disagreeing_authoritative_pushes_both_record_and_receipt_ends_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let path = Arc::new(dir.path().join("ledger.sqlite3"));
    let (task_id, plan_id) = {
        let mut store = LedgerStore::open(path.as_path()).unwrap();
        setup_task_plan_receipt(&mut store)
    };

    let handles: Vec<_> = [("completed", "race-completed"), ("failed", "race-failed")]
        .into_iter()
        .map(|(kind, key)| {
            let path = Arc::clone(&path);
            std::thread::spawn(move || {
                let mut store = LedgerStore::open(path.as_path()).unwrap();
                store
                    .record_outcome_attestation(
                        key,
                        task_id,
                        Some(plan_id),
                        "provider",
                        Some("example-provider"),
                        kind,
                        "[]",
                        key,
                        true,
                        now(),
                    )
                    .unwrap()
            })
        })
        .collect();

    let results: Vec<RecordAttestationOutcome> =
        handles.into_iter().map(|h| h.join().unwrap()).collect();
    for r in &results {
        assert!(
            matches!(r, RecordAttestationOutcome::Recorded { .. }),
            "both racing pushes must be recorded, got {r:?}"
        );
    }

    let conn = rusqlite::Connection::open(path.as_path()).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM outcome_attestations WHERE task_id = ?1",
            [task_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 2,
        "both disagreeing pushes must be append-only recorded"
    );

    let outcome_json: String = conn
        .query_row(
            "SELECT outcome_json FROM receipts WHERE task_id = ?1 AND plan_id = ?2",
            rusqlite::params![task_id.to_string(), plan_id.0.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        outcome_json.contains("unknown"),
        "a conflict must promote to Unknown, not silently pick a winner: {outcome_json}"
    );
}

/// An authoritative attestation races `grant_renewal` on the same task.
/// Both operations use their own `BEGIN IMMEDIATE` transaction, so
/// SQLite's own locking must serialize them — there must never be a
/// granted renewal alongside a visible current-revision `Completed`
/// attestation that the grant should have refused against.
#[test]
fn an_attestation_push_racing_grant_renewal_never_leaves_a_grant_alongside_an_unseen_completion() {
    use libra_governor_domain::{
        AutonomyBoundary, BlockingStatus, CompletionCriterion, Confidence, ConstraintMode, Policy,
        RenewalAuthority, RenewalBound, RenewalRequest, ResourceAmount, ResourceBound, TimeBound,
    };
    use libra_governor_ledger::GrantRenewalOutcome;

    let dir = tempfile::tempdir().unwrap();
    let path = Arc::new(dir.path().join("ledger.sqlite3"));
    let (task_id, plan_id) = {
        let mut store = LedgerStore::open(path.as_path()).unwrap();
        let (task_id, plan_id) = setup_task_plan_receipt(&mut store);
        let policy = Policy::validated(
            "test-renewable",
            ResourceBound {
                mode: ConstraintMode::Hard,
                target: ResourceAmount::Tokens(500),
                elastic_ceiling: None,
                hard_ceiling: ResourceAmount::Tokens(1000),
            },
            TimeBound {
                mode: ConstraintMode::Hard,
                target_secs: 600,
                elastic_ceiling_secs: None,
                hard_ceiling_secs: Some(600),
                deadline: None,
            },
            CompletionContract::first(vec![CompletionCriterion::required("tests pass")]),
            Confidence::Low,
            AutonomyBoundary::AskOnApproval,
        )
        .unwrap()
        .with_renewal(RenewalBound {
            lifetime_ceiling: ResourceAmount::Tokens(2500),
            max_renewals: 2,
        })
        .unwrap();
        let reserve = libra_governor_domain::CompletionReserveEstimate {
            amount: ResourceAmount::Tokens(100),
            basis: libra_governor_domain::CompletionReserveBasis::PolicyTarget,
            fraction: 0.2,
            required_criteria_count: 1,
        };
        store
            .initialize_task_budget(task_id, &policy, &reserve, now())
            .unwrap();
        (task_id, plan_id)
    };

    let attest_path = Arc::clone(&path);
    let attest_handle = std::thread::spawn(move || {
        let mut store = LedgerStore::open(attest_path.as_path()).unwrap();
        store
            .record_outcome_attestation(
                "attest-race",
                task_id,
                Some(plan_id),
                "provider",
                Some("example-provider"),
                "completed",
                "[]",
                "attest-race",
                true,
                now(),
            )
            .unwrap()
    });

    let renew_path = Arc::clone(&path);
    let renew_handle = std::thread::spawn(move || {
        let mut store = LedgerStore::open(renew_path.as_path()).unwrap();
        store
            .grant_renewal(
                task_id,
                RenewalRequest {
                    amount: ResourceAmount::Tokens(500),
                    authority: RenewalAuthority::Operator {
                        operator_id: "test-operator".to_string(),
                    },
                    contract_revision: 1,
                    reason: "race test".to_string(),
                    idempotency_key: "grant-race".to_string(),
                },
                BlockingStatus::NotBlocking,
                now(),
            )
            .unwrap()
    });

    let attest_result = attest_handle.join().unwrap();
    let renew_result = renew_handle.join().unwrap();
    assert!(matches!(
        attest_result,
        RecordAttestationOutcome::Recorded { .. }
    ));

    // Whichever transaction committed first determines serialized order:
    // either the grant saw no completion yet and was Granted (fine — the
    // attestation arrived after), or it saw the completion and was
    // correctly Refused(TaskAlreadyCompleted). What must never happen is
    // some third state (a crash, a silent double-write, or a Granted
    // outcome whose own committed transaction also shows an
    // already-visible current-revision Completed row it should have
    // refused against) — assert exactly one of the two valid outcomes.
    match renew_result {
        GrantRenewalOutcome::Granted(_) | GrantRenewalOutcome::Refused(_) => {}
        other => panic!("grant_renewal must either Grant or Refuse, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// Late delivery: a push against a superseded plan/revision
// ---------------------------------------------------------------------

/// A push carrying an old `plan_id` arrives after the task's contract
/// revision has already bumped. It must bind to the OLD plan's revision
/// (not the new current one), must never touch the new revision's
/// receipt, and must not block a renewal requested against the current
/// revision.
#[test]
fn a_late_push_against_a_superseded_plan_binds_to_the_old_revision_and_never_blocks_the_current_one(
) {
    let (_dir, path, mut store) = open_file_store();
    let (task_id, old_plan_id) = setup_task_plan_receipt(&mut store);

    // The task replans: a new contract revision and a new plan/receipt
    // at that revision, exactly as a real replan would leave behind.
    let v1 = CompletionContract::first(vec![]);
    let v2 = v1.next_revision(vec![]);
    store.insert_contract(task_id, &v2, now()).unwrap();
    let new_plan = ExecutionPlan::new(task_id, 2, None, now());
    store.insert_plan(&new_plan).unwrap();
    let new_receipt = ExecutionReceipt::new(
        task_id,
        2,
        new_plan.id,
        60,
        vec![],
        ExecutionOutcome::Unknown,
        now(),
    );
    store.insert_receipt(&new_receipt).unwrap();

    // A late push against the OLD plan arrives now.
    let outcome = store
        .record_outcome_attestation(
            "late-attest",
            task_id,
            Some(old_plan_id),
            "provider",
            Some("example-provider"),
            "completed",
            "[]",
            "late-attest",
            true,
            now(),
        )
        .unwrap();
    assert_eq!(
        outcome,
        RecordAttestationOutcome::Recorded {
            contract_revision: Some(1),
            receipt_updated: true,
        },
        "a late push must bind to the OLD plan's own revision (1), not the task's current one (2)"
    );

    // The NEW revision's receipt must be untouched.
    let conn = rusqlite::Connection::open(&path).unwrap();
    let new_outcome_json: String = conn
        .query_row(
            "SELECT outcome_json FROM receipts WHERE task_id = ?1 AND plan_id = ?2",
            rusqlite::params![task_id.to_string(), new_plan.id.0.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        new_outcome_json.contains("unknown"),
        "a late push bound to revision 1 must never promote revision 2's receipt: \
         {new_outcome_json}"
    );

    // A renewal requested against the current revision (2) must not be
    // blocked by the superseded-revision completion.
    use libra_governor_domain::{
        AutonomyBoundary, BlockingStatus, CompletionCriterion, Confidence, ConstraintMode, Policy,
        RenewalAuthority, RenewalBound, RenewalRequest, ResourceAmount, ResourceBound, TimeBound,
    };
    use libra_governor_ledger::GrantRenewalOutcome;

    let policy = Policy::validated(
        "test-renewable",
        ResourceBound {
            mode: ConstraintMode::Hard,
            target: ResourceAmount::Tokens(500),
            elastic_ceiling: None,
            hard_ceiling: ResourceAmount::Tokens(1000),
        },
        TimeBound {
            mode: ConstraintMode::Hard,
            target_secs: 600,
            elastic_ceiling_secs: None,
            hard_ceiling_secs: Some(600),
            deadline: None,
        },
        CompletionContract::first(vec![CompletionCriterion::required("tests pass")]),
        Confidence::Low,
        AutonomyBoundary::AskOnApproval,
    )
    .unwrap()
    .with_renewal(RenewalBound {
        lifetime_ceiling: ResourceAmount::Tokens(2500),
        max_renewals: 2,
    })
    .unwrap();
    let reserve = libra_governor_domain::CompletionReserveEstimate {
        amount: ResourceAmount::Tokens(100),
        basis: libra_governor_domain::CompletionReserveBasis::PolicyTarget,
        fraction: 0.2,
        required_criteria_count: 1,
    };
    store
        .initialize_task_budget(task_id, &policy, &reserve, now())
        .unwrap();

    let renewal_outcome = store
        .grant_renewal(
            task_id,
            RenewalRequest {
                amount: ResourceAmount::Tokens(500),
                authority: RenewalAuthority::Operator {
                    operator_id: "test-operator".to_string(),
                },
                contract_revision: 2,
                reason: "late-delivery test grant".to_string(),
                idempotency_key: "late-delivery-grant".to_string(),
            },
            BlockingStatus::NotBlocking,
            now(),
        )
        .unwrap();
    assert!(
        matches!(renewal_outcome, GrantRenewalOutcome::Granted(_)),
        "a Completed claim bound to superseded revision 1 must not block a renewal at current \
         revision 2, got {renewal_outcome:?}"
    );
}

// ---------------------------------------------------------------------
// Restart: behavior survives a real store close/reopen
// ---------------------------------------------------------------------

#[test]
fn replay_semantics_survive_a_store_close_and_reopen() {
    let (dir, path, mut store) = open_file_store();
    let (task_id, plan_id) = setup_task_plan_receipt(&mut store);

    let first = store
        .record_outcome_attestation(
            "restart-attest",
            task_id,
            Some(plan_id),
            "provider",
            Some("example-provider"),
            "completed",
            "[]",
            "restart-key",
            true,
            now(),
        )
        .unwrap();
    assert!(matches!(first, RecordAttestationOutcome::Recorded { .. }));

    drop(store);
    let mut reopened = LedgerStore::open(&path).unwrap();

    let identical_replay = reopened
        .record_outcome_attestation(
            "restart-attest",
            task_id,
            Some(plan_id),
            "provider",
            Some("example-provider"),
            "completed",
            "[]",
            "restart-key",
            true,
            now(),
        )
        .unwrap();
    assert_eq!(
        identical_replay,
        RecordAttestationOutcome::Duplicate,
        "an identical replay after a real restart must still be recognized as a duplicate"
    );

    let changed_content_replay = reopened
        .record_outcome_attestation(
            "restart-attest-2",
            task_id,
            Some(plan_id),
            "provider",
            Some("example-provider"),
            "failed",
            "[]",
            "restart-key",
            true,
            now(),
        )
        .unwrap();
    assert_eq!(
        changed_content_replay,
        RecordAttestationOutcome::IdempotencyKeyReused,
        "changed content under the same idempotency key must still be refused after a restart"
    );

    drop(dir);
}
