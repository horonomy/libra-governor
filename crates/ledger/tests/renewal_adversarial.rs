//! Independent adversarial verification of the bounded, auditable
//! task-budget renewal mechanism (HORO-1727), mirroring
//! `shared_pool_adversarial.rs`'s discipline: these cases are chosen to
//! falsify the mechanism, not to restate what the implementer already
//! believes works.
//!
//! Every test here calls `LedgerStore::grant_renewal` directly — the
//! method has no production caller (see `renewal_not_wired_live.rs`), so
//! "the real admission seam" for this ticket's scope is the ledger API
//! itself, exercised the same way `reservation_concurrency.rs` exercises
//! `reserve`/`available` directly rather than through the daemon.
//!
//! Tests that need to inspect `task_budget_renewals` with raw SQL (the
//! append-only trigger, the `CHECK(amount > 0)` constraint, counting
//! rows) open a second `rusqlite::Connection` against the same on-disk
//! file, mirroring `shared_pool_adversarial.rs`'s own "raw connection, no
//! LedgerStore" pattern — `LedgerStore::open_in_memory` is a private,
//! unshared database per connection, so there is no raw connection to
//! open a second handle to for those cases.

use std::sync::Arc;

use libra_governor_domain::{
    AutonomyBoundary, BlockingStatus, CompletionContract, CompletionCriterion,
    CompletionReserveBasis, CompletionReserveEstimate, Confidence, ConstraintMode,
    IndeterminateReason, Policy, RenewalAuthority, RenewalBound, RenewalRefusal, RenewalRequest,
    ReservationClass, ResourceAmount, ResourceBound, TaskId, TaskIdentity, TimeBound,
};
use libra_governor_ledger::{
    GrantRenewalOutcome, LedgerStore, OutcomeAttestationInsert, ReserveOutcome, ReserveRequest,
    SettleOutcome,
};
use time::OffsetDateTime;

fn now() -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH
}

/// 1,000-token hard ceiling, renewals enabled up to a 2,500-token
/// lifetime ceiling, at most 2 grants.
fn renewable_policy() -> Policy {
    Policy::validated(
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
    .unwrap()
}

/// Same shape, but with no renewal bound at all — the default on every
/// policy today.
fn non_renewable_policy() -> Policy {
    Policy::validated(
        "test-non-renewable",
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
}

fn setup(store: &mut LedgerStore, policy: &Policy) -> TaskId {
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
    let reserve = CompletionReserveEstimate {
        amount: ResourceAmount::Tokens(100),
        basis: CompletionReserveBasis::PolicyTarget,
        fraction: 0.2,
        required_criteria_count: 1,
    };
    store
        .initialize_task_budget(task_id, policy, &reserve, now())
        .unwrap();
    task_id
}

fn request(idempotency_key: &str) -> RenewalRequest {
    RenewalRequest {
        amount: ResourceAmount::Tokens(500),
        authority: RenewalAuthority::Operator {
            operator_id: "test-operator".to_string(),
        },
        contract_revision: 1,
        reason: "adversarial test grant".to_string(),
        idempotency_key: idempotency_key.to_string(),
    }
}

/// Opens a fresh on-disk store in its own temp directory, returning the
/// path too so a test can open a second raw connection against the same
/// file.
fn open_file_store() -> (tempfile::TempDir, std::path::PathBuf, LedgerStore) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    let store = LedgerStore::open(&path).unwrap();
    (dir, path, store)
}

fn renewal_row_count(path: &std::path::Path, task_id: TaskId) -> i64 {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM task_budget_renewals WHERE task_id = ?1",
        [task_id.to_string()],
        |row| row.get(0),
    )
    .unwrap()
}

// ---------------------------------------------------------------------
// Headroom-exact
// ---------------------------------------------------------------------

#[test]
fn a_grant_raises_available_headroom_by_exactly_the_granted_amount() {
    let (_dir, _path, mut store) = open_file_store();
    let task_id = setup(&mut store, &renewable_policy());

    let before_required = store
        .available(task_id, ReservationClass::RequiredWork)
        .unwrap()
        .unwrap();
    let before_optional = store
        .available(task_id, ReservationClass::OptionalWork)
        .unwrap()
        .unwrap();

    let outcome = store
        .grant_renewal(
            task_id,
            request("grant-1"),
            BlockingStatus::NotBlocking,
            now(),
        )
        .unwrap();
    assert!(matches!(outcome, GrantRenewalOutcome::Granted(_)));

    let after_required = store
        .available(task_id, ReservationClass::RequiredWork)
        .unwrap()
        .unwrap();
    let after_optional = store
        .available(task_id, ReservationClass::OptionalWork)
        .unwrap()
        .unwrap();

    assert_eq!(
        after_required.value - before_required.value,
        500.0,
        "granting 500 tokens must raise RequiredWork headroom by exactly 500"
    );
    assert_eq!(
        after_optional.value - before_optional.value,
        500.0,
        "granting 500 tokens must raise OptionalWork headroom by exactly 500 too"
    );
}

#[test]
fn nothing_else_changes_when_a_grant_is_made() {
    let (_dir, _path, mut store) = open_file_store();
    let task_id = setup(&mut store, &renewable_policy());
    let before = store.task_budget(task_id).unwrap().unwrap();

    store
        .grant_renewal(
            task_id,
            request("grant-1"),
            BlockingStatus::NotBlocking,
            now(),
        )
        .unwrap();

    let after = store.task_budget(task_id).unwrap().unwrap();
    assert_eq!(
        before.hard_limit, after.hard_limit,
        "hard_limit never moves"
    );
    assert_eq!(
        before.completion_reserve, after.completion_reserve,
        "a grant must not touch the completion reserve"
    );
    assert_eq!(after.renewed_capacity, ResourceAmount::Tokens(500));
}

// ---------------------------------------------------------------------
// Each refusal gate fires independently, nothing written on refusal
// ---------------------------------------------------------------------

#[test]
fn refuses_when_renewals_are_disabled() {
    let (_dir, path, mut store) = open_file_store();
    let task_id = setup(&mut store, &non_renewable_policy());

    let outcome = store
        .grant_renewal(task_id, request("k1"), BlockingStatus::NotBlocking, now())
        .unwrap();
    assert_eq!(
        outcome,
        GrantRenewalOutcome::Refused(RenewalRefusal::RenewalsDisabled)
    );
    assert_eq!(renewal_row_count(&path, task_id), 0);
}

#[test]
fn refuses_once_max_renewals_is_reached() {
    let (_dir, path, mut store) = open_file_store();
    let task_id = setup(&mut store, &renewable_policy());

    for i in 0..2 {
        let outcome = store
            .grant_renewal(
                task_id,
                request(&format!("k{i}")),
                BlockingStatus::NotBlocking,
                now(),
            )
            .unwrap();
        assert!(matches!(outcome, GrantRenewalOutcome::Granted(_)));
    }

    let outcome = store
        .grant_renewal(
            task_id,
            request("k-third"),
            BlockingStatus::NotBlocking,
            now(),
        )
        .unwrap();
    assert_eq!(
        outcome,
        GrantRenewalOutcome::Refused(RenewalRefusal::MaxRenewalsExceeded { current: 2, max: 2 })
    );
    assert_eq!(renewal_row_count(&path, task_id), 2);
}

#[test]
fn refuses_a_grant_exceeding_a_single_hard_ceiling_allocation() {
    let (_dir, path, mut store) = open_file_store();
    let task_id = setup(&mut store, &renewable_policy());

    let mut oversized = request("k1");
    oversized.amount = ResourceAmount::Tokens(1_500); // hard_ceiling is 1,000
    let outcome = store
        .grant_renewal(task_id, oversized, BlockingStatus::NotBlocking, now())
        .unwrap();
    assert_eq!(
        outcome,
        GrantRenewalOutcome::Refused(RenewalRefusal::GrantExceedsHardCeiling {
            amount: ResourceAmount::Tokens(1_500),
            hard_ceiling: ResourceAmount::Tokens(1_000),
        })
    );
    assert_eq!(renewal_row_count(&path, task_id), 0);
}

#[test]
fn refuses_a_grant_that_would_exceed_the_lifetime_ceiling() {
    let (_dir, path, mut store) = open_file_store();
    let task_id = setup(&mut store, &renewable_policy());

    // effective_hard_limit starts at 1,000. lifetime_ceiling is 2,500.
    // Granting 1,000 twice reaches 3,000 > 2,500 on the second grant.
    let mut first = request("k1");
    first.amount = ResourceAmount::Tokens(1_000);
    let outcome = store
        .grant_renewal(task_id, first, BlockingStatus::NotBlocking, now())
        .unwrap();
    assert!(matches!(outcome, GrantRenewalOutcome::Granted(_)));

    let mut second = request("k2");
    second.amount = ResourceAmount::Tokens(1_000);
    let outcome = store
        .grant_renewal(task_id, second, BlockingStatus::NotBlocking, now())
        .unwrap();
    assert_eq!(
        outcome,
        GrantRenewalOutcome::Refused(RenewalRefusal::LifetimeCeilingExceeded {
            effective_after: ResourceAmount::Tokens(3_000),
            lifetime_ceiling: ResourceAmount::Tokens(2_500),
        })
    );
    assert_eq!(renewal_row_count(&path, task_id), 1);
}

#[test]
fn refuses_when_upstream_quota_is_blocking() {
    let (_dir, path, mut store) = open_file_store();
    let task_id = setup(&mut store, &renewable_policy());

    let outcome = store
        .grant_renewal(task_id, request("k1"), BlockingStatus::Blocking, now())
        .unwrap();
    assert_eq!(
        outcome,
        GrantRenewalOutcome::Refused(RenewalRefusal::UpstreamQuotaBlocking {
            status: BlockingStatus::Blocking
        })
    );
    assert_eq!(renewal_row_count(&path, task_id), 0);
}

/// "Unknown is not the same as available" — the discipline this
/// campaign applies everywhere else in this codebase, applied here too.
/// `Indeterminate` must be refused exactly like `Blocking`, never
/// silently treated as a safe default.
#[test]
fn refuses_when_upstream_quota_is_indeterminate() {
    let (_dir, path, mut store) = open_file_store();
    let task_id = setup(&mut store, &renewable_policy());

    let status = BlockingStatus::Indeterminate(IndeterminateReason::NoSnapshot);
    let outcome = store
        .grant_renewal(task_id, request("k1"), status.clone(), now())
        .unwrap();
    assert_eq!(
        outcome,
        GrantRenewalOutcome::Refused(RenewalRefusal::UpstreamQuotaBlocking { status })
    );
    assert_eq!(renewal_row_count(&path, task_id), 0);
}

#[test]
fn refuses_when_task_already_has_an_authoritative_completed_attestation() {
    let (_dir, path, mut store) = open_file_store();
    let task_id = setup(&mut store, &renewable_policy());

    store
        .insert_outcome_attestation(OutcomeAttestationInsert {
            id: "attestation-1",
            task_id,
            plan_id: None,
            contract_revision: None,
            source: "governor_local",
            source_id: None,
            outcome_kind: "completed",
            evidence_json: "[]",
            idempotency_key: "finalize-1",
            authoritative: true,
            attested_at: now(),
        })
        .unwrap();

    let outcome = store
        .grant_renewal(task_id, request("k1"), BlockingStatus::NotBlocking, now())
        .unwrap();
    assert_eq!(
        outcome,
        GrantRenewalOutcome::Refused(RenewalRefusal::TaskAlreadyCompleted)
    );
    assert_eq!(renewal_row_count(&path, task_id), 0);
}

/// A non-authoritative attestation must NOT block a renewal — only
/// `Provider`/`GovernorLocal` claims are trustworthy enough to refuse
/// further work against (see `AttestationSource::is_authoritative`).
#[test]
fn a_non_authoritative_completed_attestation_does_not_block_a_grant() {
    let (_dir, _path, mut store) = open_file_store();
    let task_id = setup(&mut store, &renewable_policy());

    store
        .insert_outcome_attestation(OutcomeAttestationInsert {
            id: "attestation-1",
            task_id,
            plan_id: None,
            contract_revision: None,
            source: "agent",
            source_id: None,
            outcome_kind: "completed",
            evidence_json: "[]",
            idempotency_key: "agent-claim-1",
            authoritative: false,
            attested_at: now(),
        })
        .unwrap();

    let outcome = store
        .grant_renewal(task_id, request("k1"), BlockingStatus::NotBlocking, now())
        .unwrap();
    assert!(matches!(outcome, GrantRenewalOutcome::Granted(_)));
}

// ---------------------------------------------------------------------
// Revision-scoped completion (HORO-1727 Decision 2)
// ---------------------------------------------------------------------

#[test]
fn refuses_a_request_authorized_against_a_stale_contract_revision() {
    let (_dir, path, mut store) = open_file_store();
    let task_id = setup(&mut store, &renewable_policy());
    let v1 = CompletionContract::first(vec![CompletionCriterion::required("tests pass")]);
    let v2 = v1.next_revision(vec![CompletionCriterion::required("tests pass, updated")]);
    store.insert_contract(task_id, &v1, now()).unwrap();
    store.insert_contract(task_id, &v2, now()).unwrap();

    let mut stale = request("k1");
    stale.contract_revision = 1; // current is 2
    let outcome = store
        .grant_renewal(task_id, stale, BlockingStatus::NotBlocking, now())
        .unwrap();
    assert_eq!(
        outcome,
        GrantRenewalOutcome::Refused(RenewalRefusal::ContractRevisionMismatch {
            requested: 1,
            current: 2,
        })
    );
    assert_eq!(renewal_row_count(&path, task_id), 0);
}

/// A `Completed` claim against a superseded revision must not keep
/// blocking renewals requested against the task's current revision —
/// the direct behavioral test for HORO-1727 Decision 2's "current
/// revision only" rule.
#[test]
fn a_completed_attestation_at_a_superseded_revision_does_not_block_renewal_at_the_current_revision()
{
    let (_dir, _path, mut store) = open_file_store();
    let task_id = setup(&mut store, &renewable_policy());
    let v1 = CompletionContract::first(vec![CompletionCriterion::required("tests pass")]);
    let v2 = v1.next_revision(vec![CompletionCriterion::required("tests pass, updated")]);
    store.insert_contract(task_id, &v1, now()).unwrap();
    store.insert_contract(task_id, &v2, now()).unwrap();

    store
        .insert_outcome_attestation(OutcomeAttestationInsert {
            id: "attestation-1",
            task_id,
            plan_id: None,
            contract_revision: Some(1),
            source: "governor_local",
            source_id: None,
            outcome_kind: "completed",
            evidence_json: "[]",
            idempotency_key: "finalize-1",
            authoritative: true,
            attested_at: now(),
        })
        .unwrap();

    let mut current = request("k1");
    current.contract_revision = 2;
    let outcome = store
        .grant_renewal(task_id, current, BlockingStatus::NotBlocking, now())
        .unwrap();
    assert!(
        matches!(outcome, GrantRenewalOutcome::Granted(_)),
        "a Completed claim bound to revision 1 must not block a renewal at current revision 2, \
         got {outcome:?}"
    );
}

#[test]
fn refuses_when_the_current_revision_has_disagreeing_authoritative_terminal_outcomes() {
    let (_dir, path, mut store) = open_file_store();
    let task_id = setup(&mut store, &renewable_policy());
    let v1 = CompletionContract::first(vec![CompletionCriterion::required("tests pass")]);
    store.insert_contract(task_id, &v1, now()).unwrap();

    store
        .insert_outcome_attestation(OutcomeAttestationInsert {
            id: "attestation-1",
            task_id,
            plan_id: None,
            contract_revision: Some(1),
            source: "governor_local",
            source_id: None,
            outcome_kind: "completed",
            evidence_json: "[]",
            idempotency_key: "claim-completed",
            authoritative: true,
            attested_at: now(),
        })
        .unwrap();
    store
        .insert_outcome_attestation(OutcomeAttestationInsert {
            id: "attestation-2",
            task_id,
            plan_id: None,
            contract_revision: Some(1),
            source: "governor_local",
            source_id: None,
            outcome_kind: "failed",
            evidence_json: "[]",
            idempotency_key: "claim-failed",
            authoritative: true,
            attested_at: now(),
        })
        .unwrap();

    let mut current = request("k1");
    current.contract_revision = 1;
    let outcome = store
        .grant_renewal(task_id, current, BlockingStatus::NotBlocking, now())
        .unwrap();
    assert_eq!(
        outcome,
        GrantRenewalOutcome::Refused(RenewalRefusal::ConflictingCompletionOutcomes)
    );
    assert_eq!(renewal_row_count(&path, task_id), 0);
}

/// An unbound (legacy, `contract_revision: None`) authoritative
/// `Completed` attestation cannot be proven to apply — or not to apply —
/// to a task's current revision once one exists, so it refuses
/// conservatively rather than either blocking forever or being ignored.
#[test]
fn refuses_conservatively_on_an_unbound_legacy_completed_attestation_once_a_revision_exists() {
    let (_dir, path, mut store) = open_file_store();
    let task_id = setup(&mut store, &renewable_policy());
    let v1 = CompletionContract::first(vec![CompletionCriterion::required("tests pass")]);
    store.insert_contract(task_id, &v1, now()).unwrap();

    store
        .insert_outcome_attestation(OutcomeAttestationInsert {
            id: "attestation-1",
            task_id,
            plan_id: None,
            contract_revision: None,
            source: "governor_local",
            source_id: None,
            outcome_kind: "completed",
            evidence_json: "[]",
            idempotency_key: "finalize-1",
            authoritative: true,
            attested_at: now(),
        })
        .unwrap();

    let mut current = request("k1");
    current.contract_revision = 1;
    let outcome = store
        .grant_renewal(task_id, current, BlockingStatus::NotBlocking, now())
        .unwrap();
    assert_eq!(
        outcome,
        GrantRenewalOutcome::Refused(RenewalRefusal::UnboundLegacyCompletion)
    );
    assert_eq!(renewal_row_count(&path, task_id), 0);
}

// ---------------------------------------------------------------------
// Monotonicity / append-only enforcement
// ---------------------------------------------------------------------

#[test]
fn committed_equals_settled_plus_active_after_any_sequence_of_grants() {
    let (_dir, _path, mut store) = open_file_store();
    let task_id = setup(&mut store, &renewable_policy());

    store
        .reserve(ReserveRequest {
            task_id,
            session_id: "session-1",
            plan_id: None,
            class: ReservationClass::OptionalWork,
            amount: ResourceAmount::Tokens(300),
            idempotency_key: "reserve-1",
            now: now(),
            ttl_secs: 900,
        })
        .unwrap();

    store
        .grant_renewal(
            task_id,
            request("grant-1"),
            BlockingStatus::NotBlocking,
            now(),
        )
        .unwrap();

    let snapshot = store.budget_snapshot(task_id).unwrap().unwrap();
    assert_eq!(
        snapshot.used().as_f64() + snapshot.reserved().as_f64(),
        300.0,
        "committed (settled + active) must equal exactly what was reserved, unaffected by the \
         grant"
    );
    assert_eq!(snapshot.granted(), ResourceAmount::Tokens(500));
    assert_eq!(snapshot.total(), ResourceAmount::Tokens(1_500));
}

#[test]
fn raw_update_against_the_renewals_table_is_rejected_by_the_append_only_trigger() {
    let (_dir, path, mut store) = open_file_store();
    let task_id = setup(&mut store, &renewable_policy());
    store
        .grant_renewal(
            task_id,
            request("grant-1"),
            BlockingStatus::NotBlocking,
            now(),
        )
        .unwrap();
    drop(store);

    let conn = rusqlite::Connection::open(&path).unwrap();
    let result = conn.execute(
        "UPDATE task_budget_renewals SET amount = 999999.0 WHERE task_id = ?1",
        [task_id.to_string()],
    );
    assert!(
        result.is_err(),
        "an UPDATE against task_budget_renewals must be rejected by the append-only trigger"
    );
}

#[test]
fn raw_delete_against_the_renewals_table_is_rejected_by_the_append_only_trigger() {
    let (_dir, path, mut store) = open_file_store();
    let task_id = setup(&mut store, &renewable_policy());
    store
        .grant_renewal(
            task_id,
            request("grant-1"),
            BlockingStatus::NotBlocking,
            now(),
        )
        .unwrap();
    drop(store);

    let conn = rusqlite::Connection::open(&path).unwrap();
    let result = conn.execute(
        "DELETE FROM task_budget_renewals WHERE task_id = ?1",
        [task_id.to_string()],
    );
    assert!(
        result.is_err(),
        "a DELETE against task_budget_renewals must be rejected by the append-only trigger"
    );
}

#[test]
fn a_negative_grant_amount_is_rejected_by_the_check_constraint() {
    let (_dir, path, mut store) = open_file_store();
    let task_id = setup(&mut store, &renewable_policy());
    drop(store);

    let conn = rusqlite::Connection::open(&path).unwrap();
    let result = conn.execute(
        "INSERT INTO task_budget_renewals (
            id, task_id, amount, authority_json, contract_revision, reason,
            idempotency_key, settled_at_grant, active_at_grant, effective_before,
            effective_after, quota_status_json, schema_version, granted_at
         ) VALUES ('bad-row', ?1, -500.0, '{}', 1, 'x', 'bad-key', 0.0, 0.0, 1000.0, 500.0, \
         '{}', 'v1', '2024-01-01T00:00:00Z')",
        [task_id.to_string()],
    );
    assert!(
        result.is_err(),
        "a negative amount must be rejected by the CHECK(amount > 0) constraint"
    );
}

/// Reuses the existing late-settlement-after-expiry pattern, with a
/// renewal grant present — settled spend must still correctly increase
/// after expiry regardless of whether the task's ceiling was ever
/// extended.
#[test]
fn late_settlement_after_expiry_still_increases_settled_spend_with_a_grant_present() {
    let (_dir, _path, mut store) = open_file_store();
    let task_id = setup(&mut store, &renewable_policy());

    store
        .grant_renewal(
            task_id,
            request("grant-1"),
            BlockingStatus::NotBlocking,
            now(),
        )
        .unwrap();

    let reservation = match store
        .reserve(ReserveRequest {
            task_id,
            session_id: "session-1",
            plan_id: None,
            class: ReservationClass::RequiredWork,
            amount: ResourceAmount::Tokens(100),
            idempotency_key: "reserve-late",
            now: now(),
            ttl_secs: 1,
        })
        .unwrap()
    {
        ReserveOutcome::Granted(r) => *r,
        other => panic!("expected Granted, got {other:?}"),
    };

    let late = now() + time::Duration::seconds(10);
    let outcome = store
        .settle(reservation.id, Some(ResourceAmount::Tokens(150)), late)
        .unwrap();
    match outcome {
        SettleOutcome::Settled { .. } => {}
        other => panic!("expected Settled, got {other:?}"),
    }

    let snapshot = store.budget_snapshot(task_id).unwrap().unwrap();
    assert_eq!(snapshot.used(), ResourceAmount::Tokens(150));
}

// ---------------------------------------------------------------------
// Zero-renewal backward compatibility
// ---------------------------------------------------------------------

#[test]
fn zero_renewal_rows_produces_identical_available_and_snapshot_figures() {
    let (_dir_a, _path_a, mut without_renewal_feature) = open_file_store();
    let task_a = setup(&mut without_renewal_feature, &non_renewable_policy());

    let (_dir_b, _path_b, mut with_renewal_feature_but_unused) = open_file_store();
    let task_b = setup(&mut with_renewal_feature_but_unused, &renewable_policy());

    without_renewal_feature
        .reserve(ReserveRequest {
            task_id: task_a,
            session_id: "s",
            plan_id: None,
            class: ReservationClass::OptionalWork,
            amount: ResourceAmount::Tokens(300),
            idempotency_key: "r1",
            now: now(),
            ttl_secs: 900,
        })
        .unwrap();
    with_renewal_feature_but_unused
        .reserve(ReserveRequest {
            task_id: task_b,
            session_id: "s",
            plan_id: None,
            class: ReservationClass::OptionalWork,
            amount: ResourceAmount::Tokens(300),
            idempotency_key: "r1",
            now: now(),
            ttl_secs: 900,
        })
        .unwrap();

    let a = without_renewal_feature
        .available(task_a, ReservationClass::RequiredWork)
        .unwrap()
        .unwrap();
    let b = with_renewal_feature_but_unused
        .available(task_b, ReservationClass::RequiredWork)
        .unwrap()
        .unwrap();
    assert_eq!(
        a.value, b.value,
        "a policy carrying an unused RenewalBound must behave identically to one with none, \
         with zero grants"
    );

    let snap_a = without_renewal_feature
        .budget_snapshot(task_a)
        .unwrap()
        .unwrap();
    let snap_b = with_renewal_feature_but_unused
        .budget_snapshot(task_b)
        .unwrap()
        .unwrap();
    assert_eq!(snap_a.remaining().value, snap_b.remaining().value);
    assert_eq!(snap_a.utilization(), snap_b.utilization());
    assert_eq!(snap_b.granted(), ResourceAmount::Tokens(0));
}

// ---------------------------------------------------------------------
// Concurrent-grant race
// ---------------------------------------------------------------------

/// Two concurrent grant requests each individually fit within
/// `lifetime_ceiling` (2,500) but together (1,000 + 1,000 = 2,000 on top
/// of the starting 1,000 effective ceiling = 3,000) would exceed it —
/// exactly one must succeed. Real multi-connection race, mirroring
/// `reservation_concurrency.rs`'s on-disk-file pattern.
#[test]
fn concurrent_grants_that_individually_fit_but_jointly_exceed_lifetime_ceiling_never_both_succeed()
{
    let dir = tempfile::tempdir().unwrap();
    let path = Arc::new(dir.path().join("ledger.sqlite3"));

    let task_id = {
        let mut store = LedgerStore::open(path.as_path()).unwrap();
        setup(&mut store, &renewable_policy())
    };

    let handles: Vec<_> = ["race-a", "race-b"]
        .into_iter()
        .map(|key| {
            let path = Arc::clone(&path);
            std::thread::spawn(move || {
                let mut store = LedgerStore::open(path.as_path()).unwrap();
                let mut req = request(key);
                req.amount = ResourceAmount::Tokens(1_000);
                store
                    .grant_renewal(task_id, req, BlockingStatus::NotBlocking, now())
                    .unwrap()
            })
        })
        .collect();

    let outcomes: Vec<GrantRenewalOutcome> =
        handles.into_iter().map(|h| h.join().unwrap()).collect();
    let granted = outcomes
        .iter()
        .filter(|o| matches!(o, GrantRenewalOutcome::Granted(_)))
        .count();
    let refused = outcomes
        .iter()
        .filter(|o| {
            matches!(
                o,
                GrantRenewalOutcome::Refused(RenewalRefusal::LifetimeCeilingExceeded { .. })
            )
        })
        .count();

    assert_eq!(
        granted, 1,
        "exactly one of two conflicting grants must succeed"
    );
    assert_eq!(refused, 1);
    assert_eq!(renewal_row_count(&path, task_id), 1);
}

// ---------------------------------------------------------------------
// Idempotent replay
// ---------------------------------------------------------------------

#[test]
fn replaying_the_same_idempotency_key_never_double_grants() {
    let (_dir, path, mut store) = open_file_store();
    let task_id = setup(&mut store, &renewable_policy());

    let first = store
        .grant_renewal(
            task_id,
            request("same-key"),
            BlockingStatus::NotBlocking,
            now(),
        )
        .unwrap();
    let second = store
        .grant_renewal(
            task_id,
            request("same-key"),
            BlockingStatus::NotBlocking,
            now(),
        )
        .unwrap();

    let GrantRenewalOutcome::Granted(first_renewal) = first else {
        panic!("expected Granted");
    };
    let GrantRenewalOutcome::AlreadyGranted(second_renewal) = second else {
        panic!("expected AlreadyGranted");
    };
    assert_eq!(first_renewal, second_renewal);
    assert_eq!(renewal_row_count(&path, task_id), 1);
}
