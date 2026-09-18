//! Concurrency and crash/restart-recovery integration tests for the
//! atomic reservation ledger (HORO-1141 failure cases 1 and 2):
//! "two or more subagents racing for the final budget" and "reservation
//! issued then process crashes".
//!
//! Each thread opens its own [`LedgerStore`] against the same on-disk
//! SQLite file — `Connection` is not `Sync`, so a real multi-thread race
//! must exercise separate connections, exactly the shape a hook process
//! and a daemon process (or several concurrent daemon-served requests in
//! a future thread-per-connection model) would produce. Atomicity here
//! rests entirely on SQLite's own `BEGIN IMMEDIATE` locking plus the
//! `busy_timeout` `LedgerStore::open` configures — not on anything the
//! daemon's own single-threaded accept loop provides.

use std::path::PathBuf;
use std::sync::Arc;

use libra_governor_domain::{
    AutonomyBoundary, CompletionContract, CompletionCriterion, CompletionReserveBasis,
    CompletionReserveEstimate, Confidence, ConstraintMode, Policy, ResourceAmount, ResourceBound,
    TaskId, TaskIdentity, TimeBound,
};
use libra_governor_ledger::{LedgerStore, ReserveOutcome, ReserveRequest};
use time::OffsetDateTime;

fn now() -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH
}

fn thousand_token_policy() -> Policy {
    Policy::validated(
        "test",
        ResourceBound {
            mode: ConstraintMode::Hard,
            target: ResourceAmount::Tokens(1000),
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

/// Sets up a fresh on-disk ledger with a 1000-token hard limit and a
/// 200-token Completion Reserve (800 tokens of optional headroom), and
/// returns the file path plus the `TaskId` every thread will race
/// against.
fn setup_shared_ledger(path: &PathBuf) -> TaskId {
    let mut store = LedgerStore::open(path).unwrap();
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
    let policy = thousand_token_policy();
    let reserve = CompletionReserveEstimate {
        amount: ResourceAmount::Tokens(200),
        basis: CompletionReserveBasis::PolicyTarget,
        fraction: 0.2,
        required_criteria_count: 1,
    };
    store
        .initialize_task_budget(task_id, &policy, &reserve, now())
        .unwrap();
    task_id
}

fn assert_budget_invariant(store: &LedgerStore, task_id: TaskId) {
    let budget = store.task_budget(task_id).unwrap().unwrap();
    let reservations = store.reservations_for_task(task_id).unwrap();
    let outstanding_draw: f64 = reservations
        .iter()
        .map(|r| r.outstanding_draw().as_f64())
        .sum();
    assert_eq!(
        budget.completion_reserve.as_f64(),
        budget.initial_completion_reserve.as_f64() - outstanding_draw,
        "completion_reserve must equal initial minus outstanding draws after concurrent access"
    );
}

/// Failure case 1: eight threads race for the 800 tokens of optional
/// headroom, each requesting exactly 200 (a clean 4-way split). Exactly
/// 4 must be granted and 4 refused — never more granted than the budget
/// allows, never a lost update, never an error.
#[test]
fn concurrent_optional_reservations_never_double_spend_the_shared_envelope() {
    let dir = tempfile::tempdir().unwrap();
    let path = Arc::new(dir.path().join("ledger.sqlite3"));
    let task_id = setup_shared_ledger(&path);

    let handles: Vec<_> = (0..8)
        .map(|i| {
            let path = Arc::clone(&path);
            std::thread::spawn(move || {
                let mut store = LedgerStore::open(path.as_path()).unwrap();
                store
                    .reserve(ReserveRequest {
                        task_id,
                        session_id: &format!("subagent-{i}"),
                        plan_id: None,
                        class: libra_governor_domain::ReservationClass::OptionalWork,
                        amount: ResourceAmount::Tokens(200),
                        idempotency_key: &format!("race-key-{i}"),
                        now: now(),
                        ttl_secs: 900,
                    })
                    .unwrap()
            })
        })
        .collect();

    let outcomes: Vec<ReserveOutcome> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let granted = outcomes
        .iter()
        .filter(|o| matches!(o, ReserveOutcome::Granted(_)))
        .count();
    let insufficient = outcomes
        .iter()
        .filter(|o| matches!(o, ReserveOutcome::Insufficient { .. }))
        .count();

    assert_eq!(
        granted, 4,
        "exactly 4 of 8 requests for 200/800 must be granted"
    );
    assert_eq!(insufficient, 4);

    let store = LedgerStore::open(path.as_path()).unwrap();
    let active_total: f64 = store
        .reservations_for_task(task_id)
        .unwrap()
        .iter()
        .filter(|r| r.state == libra_governor_domain::ReservationState::Active)
        .map(|r| r.amount.as_f64())
        .sum();
    assert_eq!(
        active_total, 800.0,
        "active reservations must sum to exactly the headroom, never more"
    );
    assert_budget_invariant(&store, task_id);
}

/// The `RequiredWork` variant of the same race: threads compete for
/// capacity that includes the protected Completion Reserve. Asserts the
/// stronger invariant that `Σ drawn_from_reserve` never exceeds the
/// reserve that existed at the start — the reserve itself cannot be
/// double-drawn by a race either.
#[test]
fn concurrent_required_work_reservations_never_over_draw_the_completion_reserve() {
    let dir = tempfile::tempdir().unwrap();
    let path = Arc::new(dir.path().join("ledger.sqlite3"));
    let task_id = setup_shared_ledger(&path);

    // 10 threads each request 150 tokens of RequiredWork against a
    // 1000-token hard limit (800 optional headroom + 200 reserve) —
    // total demand of 1500 against 1000 available, forcing contention
    // over both ordinary headroom and the reserve.
    let handles: Vec<_> = (0..10)
        .map(|i| {
            let path = Arc::clone(&path);
            std::thread::spawn(move || {
                let mut store = LedgerStore::open(path.as_path()).unwrap();
                store
                    .reserve(ReserveRequest {
                        task_id,
                        session_id: &format!("subagent-{i}"),
                        plan_id: None,
                        class: libra_governor_domain::ReservationClass::RequiredWork,
                        amount: ResourceAmount::Tokens(150),
                        idempotency_key: &format!("race-key-{i}"),
                        now: now(),
                        ttl_secs: 900,
                    })
                    .unwrap()
            })
        })
        .collect();

    let outcomes: Vec<ReserveOutcome> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let granted = outcomes
        .iter()
        .filter(|o| matches!(o, ReserveOutcome::Granted(_)))
        .count();
    // floor(1000 / 150) = 6 can be granted before the 7th (would total
    // 1050 > 1000) is refused.
    assert_eq!(granted, 6);

    let store = LedgerStore::open(path.as_path()).unwrap();
    let reservations = store.reservations_for_task(task_id).unwrap();
    let total_drawn: f64 = reservations
        .iter()
        .map(|r| r.drawn_from_reserve.as_f64())
        .sum();
    assert!(
        total_drawn <= 200.0 + 1e-9,
        "no race may draw more than the 200-token reserve that existed, got {total_drawn}"
    );
    let active_total: f64 = reservations
        .iter()
        .filter(|r| r.state == libra_governor_domain::ReservationState::Active)
        .map(|r| r.amount.as_f64())
        .sum();
    assert_eq!(
        active_total, 900.0,
        "6 * 150 = 900, exactly what fits under the 1000 hard limit"
    );
    assert_budget_invariant(&store, task_id);
}

/// Failure case 2: a reservation is issued, then the holding process
/// "crashes" — simulated by opening a store, reserving, and dropping the
/// connection without settling (a real crash leaves exactly this on-disk
/// state, since SQLite's WAL mode makes every committed transaction
/// durable regardless of how the process exits afterward). A fresh
/// process then reopens the ledger and runs reconciliation.
#[test]
fn a_reservation_left_active_by_a_crashed_process_is_reclaimed_on_reconciliation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    let task_id = setup_shared_ledger(&path);
    let reservation_id;

    {
        let mut store = LedgerStore::open(&path).unwrap();
        let ReserveOutcome::Granted(reservation) = store
            .reserve(ReserveRequest {
                task_id,
                session_id: "crashed-subagent",
                plan_id: None,
                class: libra_governor_domain::ReservationClass::RequiredWork,
                amount: ResourceAmount::Tokens(900),
                idempotency_key: "crash-key",
                now: now(),
                ttl_secs: 60,
            })
            .unwrap()
        else {
            panic!("expected Granted");
        };
        reservation_id = reservation.id;
        // Dropped here without settling or releasing — simulates the
        // process crashing mid-task.
    }

    // A fresh "process" reopens the same ledger file.
    let mut reopened = LedgerStore::open(&path).unwrap();
    let before = reopened
        .reservations_for_task(task_id)
        .unwrap()
        .into_iter()
        .find(|r| r.id == reservation_id)
        .unwrap();
    assert_eq!(
        before.state,
        libra_governor_domain::ReservationState::Active
    );

    // Reconciliation before the TTL elapses: nothing reclaimed yet.
    let too_early = reopened
        .expire_stale_reservations(now() + time::Duration::seconds(30))
        .unwrap();
    assert!(too_early.is_empty());

    // Reconciliation past the TTL: the crashed reservation is reclaimed
    // and its drawn Completion Reserve is restored.
    let reclaimed = reopened
        .expire_stale_reservations(now() + time::Duration::seconds(61))
        .unwrap();
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].id, reservation_id);
    assert_eq!(
        reclaimed[0].state,
        libra_governor_domain::ReservationState::Expired
    );

    let budget = reopened.task_budget(task_id).unwrap().unwrap();
    assert_eq!(budget.completion_reserve, ResourceAmount::Tokens(200));
    assert_budget_invariant(&reopened, task_id);

    // A fresh reservation can now be granted — the crashed one's
    // capacity is genuinely back, not just marked dead.
    let recovered = reopened
        .reserve(ReserveRequest {
            task_id,
            session_id: "recovery-subagent",
            plan_id: None,
            class: libra_governor_domain::ReservationClass::RequiredWork,
            amount: ResourceAmount::Tokens(900),
            idempotency_key: "recovery-key",
            now: now() + time::Duration::seconds(62),
            ttl_secs: 900,
        })
        .unwrap();
    assert!(matches!(recovered, ReserveOutcome::Granted(_)));
}
