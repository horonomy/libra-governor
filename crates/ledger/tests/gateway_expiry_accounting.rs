//! HORO-2286 characterization: what the task-budget ledger books for a
//! gateway request whose reservation expires while the request was already
//! forwarded.
//!
//! These tests PIN CURRENT BEHAVIOUR; they do not endorse it. The gateway
//! side (what it settles on a truncated stream) is pinned in
//! `crates/gateway/tests/truncation_accounting.rs`.
//!
//! The gateway's reservation is `OptionalWork` with no plan id and a
//! 600 s TTL (`DEFAULT_GATEWAY_RESERVATION_TTL_SECS`); the task path has
//! no renewal, so a request still streaming at TTL is reclaimed by
//! `expire_stale_reservations` while the provider is still generating.

use libra_governor_domain::{
    AutonomyBoundary, CompletionContract, CompletionCriterion, CompletionReserveBasis,
    CompletionReserveEstimate, Confidence, ConstraintMode, Policy, ReservationClass,
    ReservationState, ResourceAmount, ResourceBound, TaskId, TaskIdentity, TimeBound,
};
use libra_governor_ledger::{LedgerStore, ReserveOutcome, ReserveRequest, SettleOutcome};
use time::OffsetDateTime;

const TTL_SECS: u64 = 600;

fn t0() -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH
}

fn after_ttl() -> OffsetDateTime {
    t0() + time::Duration::seconds(TTL_SECS as i64 + 1)
}

/// A 1000-token hard budget with a 200-token Completion Reserve.
fn ledger_with_task() -> (LedgerStore, TaskId) {
    let mut store = LedgerStore::open_in_memory().unwrap();
    let task_id = TaskId::new();
    store
        .insert_task(
            &TaskIdentity {
                id: task_id,
                external_ref: None,
            },
            t0(),
        )
        .unwrap();
    let policy = Policy::validated(
        "test",
        ResourceBound {
            mode: ConstraintMode::Hard,
            target: ResourceAmount::Tokens(1000),
            elastic_ceiling: None,
            hard_ceiling: ResourceAmount::Tokens(1000),
        },
        TimeBound {
            mode: ConstraintMode::Hard,
            target_secs: 3600,
            elastic_ceiling_secs: None,
            hard_ceiling_secs: Some(3600),
            deadline: None,
        },
        CompletionContract::first(vec![CompletionCriterion::required("tests pass")]),
        Confidence::Low,
        AutonomyBoundary::AskOnApproval,
    )
    .unwrap();
    let reserve = CompletionReserveEstimate {
        amount: ResourceAmount::Tokens(200),
        basis: CompletionReserveBasis::PolicyTarget,
        fraction: 0.2,
        required_criteria_count: 1,
    };
    store
        .initialize_task_budget(task_id, &policy, &reserve, t0())
        .unwrap();
    (store, task_id)
}

fn gateway_reserve(
    store: &mut LedgerStore,
    task_id: TaskId,
    tokens: u64,
    key: &str,
) -> libra_governor_domain::Reservation {
    match store
        .reserve(ReserveRequest {
            task_id,
            session_id: "sess-1",
            plan_id: None,
            class: ReservationClass::OptionalWork,
            amount: ResourceAmount::Tokens(tokens),
            idempotency_key: key,
            now: t0(),
            ttl_secs: TTL_SECS,
        })
        .unwrap()
    {
        ReserveOutcome::Granted(r) => *r,
        other => panic!("expected a grant, got {other:?}"),
    }
}

fn optional_headroom(store: &LedgerStore, task_id: TaskId) -> f64 {
    store
        .available(task_id, ReservationClass::OptionalWork)
        .unwrap()
        .unwrap()
        .value
}

#[test]
fn a_reservation_expiring_mid_flight_is_never_charged_and_its_capacity_returns_in_full() {
    let (mut store, task_id) = ledger_with_task();
    let reservation = gateway_reserve(&mut store, task_id, 500, "gw:a");
    assert_eq!(optional_headroom(&store, task_id), 300.0);

    let expired = store.expire_stale_reservations(after_ttl()).unwrap();

    assert_eq!(expired.len(), 1);
    let row = store.get_reservation(reservation.id).unwrap().unwrap();
    assert_eq!(row.state, ReservationState::Expired);
    assert_eq!(row.settled_amount, None, "no charge is booked at expiry");
    assert_eq!(row.usage_known, None);
    assert_eq!(
        optional_headroom(&store, task_id),
        800.0,
        "all 500 reserved tokens are available again"
    );
    let snapshot = store.budget_snapshot(task_id).unwrap().unwrap();
    assert_eq!(snapshot.remaining().value, 1000.0);
}

#[test]
fn capacity_freed_by_expiry_can_be_reserved_again_while_the_forwarded_request_is_still_running() {
    let (mut store, task_id) = ledger_with_task();
    let first = gateway_reserve(&mut store, task_id, 500, "gw:first");
    store.expire_stale_reservations(after_ttl()).unwrap();

    // The first request is still streaming at the provider. A second
    // request can take the whole optional headroom (800).
    let second = gateway_reserve(&mut store, task_id, 800, "gw:second");

    assert_eq!(second.state, ReservationState::Active);
    assert_eq!(optional_headroom(&store, task_id), 0.0);
    assert_eq!(
        store.get_reservation(first.id).unwrap().unwrap().state,
        ReservationState::Expired
    );
}

#[test]
fn a_late_settle_after_expiry_books_the_actual_spend_and_can_push_the_budget_past_its_hard_limit() {
    let (mut store, task_id) = ledger_with_task();
    let first = gateway_reserve(&mut store, task_id, 500, "gw:first");
    store.expire_stale_reservations(after_ttl()).unwrap();
    let second = gateway_reserve(&mut store, task_id, 800, "gw:second");

    // The first request finally finishes and the pump settles it at 450.
    let outcome = store
        .settle(first.id, Some(ResourceAmount::Tokens(450)), after_ttl())
        .unwrap();

    let SettleOutcome::Settled { reservation, .. } = outcome else {
        panic!("a late actual is recorded, not dropped");
    };
    assert_eq!(reservation.state, ReservationState::Settled);
    assert!(reservation.settled_after_expiry);
    assert_eq!(
        reservation.settled_amount,
        Some(ResourceAmount::Tokens(450))
    );
    // 450 settled + 800 active against a 1000-token hard limit.
    let snapshot = store.budget_snapshot(task_id).unwrap().unwrap();
    assert_eq!(snapshot.remaining().value, -250.0);
    assert_eq!(
        store.get_reservation(second.id).unwrap().unwrap().state,
        ReservationState::Active
    );
}

#[test]
fn a_reservation_whose_pump_never_settles_is_never_charged_at_all() {
    // Process death, a swallowed settle error, or a panicked request task:
    // the row simply stays active until expiry, and nothing is ever booked
    // for a request that was forwarded and may have been billed.
    let (mut store, task_id) = ledger_with_task();
    let reservation = gateway_reserve(&mut store, task_id, 500, "gw:crash");

    store.expire_stale_reservations(after_ttl()).unwrap();

    let row = store.get_reservation(reservation.id).unwrap().unwrap();
    assert_eq!(row.state, ReservationState::Expired);
    assert_eq!(row.settled_amount, None);
    let snapshot = store.budget_snapshot(task_id).unwrap().unwrap();
    assert_eq!(snapshot.remaining().value, 1000.0, "spend recorded: zero");
}
