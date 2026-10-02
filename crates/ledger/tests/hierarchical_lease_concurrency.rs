//! Concurrency and crash/expiry integration tests for the hierarchical
//! resource-account tree (HORO-1668). Mirrors the idiom in
//! `reservation_concurrency.rs`: each thread opens its own [`LedgerStore`]
//! against the same on-disk file — `Connection` is not `Sync`, so a real
//! multi-thread race must exercise separate connections.

use std::path::PathBuf;
use std::sync::Arc;

use libra_governor_domain::{
    AccountLevel, AutonomyBoundary, CompletionContract, CompletionCriterion,
    CompletionReserveBasis, CompletionReserveEstimate, Confidence, ConstraintMode, Policy,
    ResourceAmount, ResourceBound, ResourceKind, TaskId, TaskIdentity, TimeBound,
};
use libra_governor_ledger::{
    AccountError, EnsureAccountOutcome, GrantSubleaseOutcome, GrantSubleaseRequest, LedgerStore,
    ReserveOutcome, ReserveRequest, SettleOutcome,
};
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

fn setup_task(store: &mut LedgerStore, reserve_amount: u64) -> TaskId {
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
        amount: ResourceAmount::Tokens(reserve_amount),
        basis: CompletionReserveBasis::PolicyTarget,
        fraction: reserve_amount as f64 / 1000.0,
        required_criteria_count: 1,
    };
    store
        .initialize_task_budget(task_id, &policy, &reserve, now())
        .unwrap();
    task_id
}

// --- Nested sublease reduces parent capacity, oversubscription refused -----

#[test]
fn nested_sublease_reduces_parent_capacity_immediately() {
    let mut store = LedgerStore::open_in_memory().unwrap();
    let task_id = setup_task(&mut store, 200); // 800 optional headroom
    let task_account = libra_governor_domain::AccountId::for_task(task_id);

    let session = store
        .ensure_child_account(task_account, AccountLevel::Session, "sess-1", now())
        .unwrap()
        .unwrap();
    assert!(session.created);

    let granted = store
        .grant_sublease(GrantSubleaseRequest {
            parent_account_id: task_account,
            child_account_id: session.account.account_id,
            child_natural_key: "sess-1",
            amount: ResourceAmount::Tokens(500),
            idempotency_key: "lease-1",
            now: now(),
            ttl_secs: 900,
        })
        .unwrap()
        .unwrap();
    assert!(matches!(granted, GrantSubleaseOutcome::Granted(_)));

    let session_capacity = store
        .account_capacity(session.account.account_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        session_capacity.granted_capacity,
        Some(ResourceAmount::Tokens(500))
    );

    // A second sublease attempting to draw the rest of the task's
    // (800 - 500 = 300) remaining optional headroom succeeds; a third
    // one past that is refused — the parent's capacity was reduced by
    // the first grant, immediately and atomically.
    let agent_a = store
        .ensure_child_account(task_account, AccountLevel::Session, "sess-2", now())
        .unwrap()
        .unwrap();
    let second = store
        .grant_sublease(GrantSubleaseRequest {
            parent_account_id: task_account,
            child_account_id: agent_a.account.account_id,
            child_natural_key: "sess-2",
            amount: ResourceAmount::Tokens(300),
            idempotency_key: "lease-2",
            now: now(),
            ttl_secs: 900,
        })
        .unwrap()
        .unwrap();
    assert!(matches!(second, GrantSubleaseOutcome::Granted(_)));

    let agent_b = store
        .ensure_child_account(task_account, AccountLevel::Session, "sess-3", now())
        .unwrap()
        .unwrap();
    let third = store
        .grant_sublease(GrantSubleaseRequest {
            parent_account_id: task_account,
            child_account_id: agent_b.account.account_id,
            child_natural_key: "sess-3",
            amount: ResourceAmount::Tokens(1),
            idempotency_key: "lease-3",
            now: now(),
            ttl_secs: 900,
        })
        .unwrap()
        .unwrap();
    assert!(matches!(third, GrantSubleaseOutcome::Insufficient { .. }));
}

#[test]
fn nested_agent_cannot_oversubscribe_its_parents_lease() {
    let mut store = LedgerStore::open_in_memory().unwrap();
    let task_id = setup_task(&mut store, 200);
    let task_account = libra_governor_domain::AccountId::for_task(task_id);

    let session = store
        .ensure_child_account(task_account, AccountLevel::Session, "sess-1", now())
        .unwrap()
        .unwrap()
        .account;
    store
        .grant_sublease(GrantSubleaseRequest {
            parent_account_id: task_account,
            child_account_id: session.account_id,
            child_natural_key: "sess-1",
            amount: ResourceAmount::Tokens(100),
            idempotency_key: "lease-session",
            now: now(),
            ttl_secs: 900,
        })
        .unwrap()
        .unwrap();

    let agent = store
        .ensure_child_account(session.account_id, AccountLevel::Agent, "agent-1", now())
        .unwrap()
        .unwrap()
        .account;

    // The session only has 100 tokens; a sub-agent asking for 101 must
    // be refused, not silently oversubscribed.
    let outcome = store
        .grant_sublease(GrantSubleaseRequest {
            parent_account_id: session.account_id,
            child_account_id: agent.account_id,
            child_natural_key: "agent-1",
            amount: ResourceAmount::Tokens(101),
            idempotency_key: "lease-agent",
            now: now(),
            ttl_secs: 900,
        })
        .unwrap()
        .unwrap();
    assert!(matches!(outcome, GrantSubleaseOutcome::Insufficient { .. }));

    let within_budget = store
        .grant_sublease(GrantSubleaseRequest {
            parent_account_id: session.account_id,
            child_account_id: agent.account_id,
            child_natural_key: "agent-1",
            amount: ResourceAmount::Tokens(100),
            idempotency_key: "lease-agent-2",
            now: now(),
            ttl_secs: 900,
        })
        .unwrap()
        .unwrap();
    assert!(matches!(within_budget, GrantSubleaseOutcome::Granted(_)));
}

#[test]
fn replayed_sublease_with_the_same_idempotency_key_returns_the_existing_lease() {
    let mut store = LedgerStore::open_in_memory().unwrap();
    let task_id = setup_task(&mut store, 200);
    let task_account = libra_governor_domain::AccountId::for_task(task_id);
    let session = store
        .ensure_child_account(task_account, AccountLevel::Session, "sess-1", now())
        .unwrap()
        .unwrap()
        .account;

    let first = store
        .grant_sublease(GrantSubleaseRequest {
            parent_account_id: task_account,
            child_account_id: session.account_id,
            child_natural_key: "sess-1",
            amount: ResourceAmount::Tokens(500),
            idempotency_key: "replay-key",
            now: now(),
            ttl_secs: 900,
        })
        .unwrap()
        .unwrap();
    let GrantSubleaseOutcome::Granted(first_reservation) = first else {
        panic!("expected Granted");
    };

    let second = store
        .grant_sublease(GrantSubleaseRequest {
            parent_account_id: task_account,
            child_account_id: session.account_id,
            child_natural_key: "sess-1",
            amount: ResourceAmount::Tokens(999), // different amount — ignored on replay
            idempotency_key: "replay-key",
            now: now(),
            ttl_secs: 900,
        })
        .unwrap()
        .unwrap();
    let GrantSubleaseOutcome::AlreadyGranted(second_reservation) = second else {
        panic!("expected AlreadyGranted, got {second:?}");
    };
    assert_eq!(first_reservation.id, second_reservation.id);
    assert_eq!(second_reservation.amount, ResourceAmount::Tokens(500));
}

#[test]
fn child_lease_ttl_is_clamped_to_its_funding_lease_expiry() {
    let mut store = LedgerStore::open_in_memory().unwrap();
    let task_id = setup_task(&mut store, 200);
    let task_account = libra_governor_domain::AccountId::for_task(task_id);
    let session = store
        .ensure_child_account(task_account, AccountLevel::Session, "sess-1", now())
        .unwrap()
        .unwrap()
        .account;
    // Task-level accounts have no funding lease of their own, so this
    // grant (session funded directly from the task) is unclamped.
    store
        .grant_sublease(GrantSubleaseRequest {
            parent_account_id: task_account,
            child_account_id: session.account_id,
            child_natural_key: "sess-1",
            amount: ResourceAmount::Tokens(500),
            idempotency_key: "lease-session",
            now: now(),
            ttl_secs: 100, // session's own funding lease expires at now()+100s
        })
        .unwrap()
        .unwrap();

    let agent = store
        .ensure_child_account(session.account_id, AccountLevel::Agent, "agent-1", now())
        .unwrap()
        .unwrap()
        .account;

    // A sub-agent lease requesting a longer TTL than its parent's own
    // funding lease must be refused.
    let refused = store
        .grant_sublease(GrantSubleaseRequest {
            parent_account_id: session.account_id,
            child_account_id: agent.account_id,
            child_natural_key: "agent-1",
            amount: ResourceAmount::Tokens(100),
            idempotency_key: "lease-agent",
            now: now(),
            ttl_secs: 200, // exceeds the session's own 100s funding lease
        })
        .unwrap();
    assert_eq!(refused, Err(AccountError::ChildExpiryExceedsFunding));

    let accepted = store
        .grant_sublease(GrantSubleaseRequest {
            parent_account_id: session.account_id,
            child_account_id: agent.account_id,
            child_natural_key: "agent-1",
            amount: ResourceAmount::Tokens(100),
            idempotency_key: "lease-agent-2",
            now: now(),
            ttl_secs: 50,
        })
        .unwrap()
        .unwrap();
    assert!(matches!(accepted, GrantSubleaseOutcome::Granted(_)));
}

#[test]
fn a_sublease_in_a_different_resource_kind_is_refused() {
    let mut store = LedgerStore::open_in_memory().unwrap();
    let task_id = setup_task(&mut store, 200);
    let task_account = libra_governor_domain::AccountId::for_task(task_id);
    let session = store
        .ensure_child_account(task_account, AccountLevel::Session, "sess-1", now())
        .unwrap()
        .unwrap()
        .account;

    let outcome = store
        .grant_sublease(GrantSubleaseRequest {
            parent_account_id: task_account,
            child_account_id: session.account_id,
            child_natural_key: "sess-1",
            amount: ResourceAmount::UsdCents(100), // task budget is Tokens
            idempotency_key: "lease-1",
            now: now(),
            ttl_secs: 900,
        })
        .unwrap();
    assert_eq!(
        outcome,
        Err(AccountError::ResourceKindMismatch {
            expected: ResourceKind::Tokens,
            actual: ResourceKind::Usd,
        })
    );
}

#[test]
fn settling_an_expired_lease_records_the_spend_as_a_visible_overrun_not_a_silent_drop() {
    let mut store = LedgerStore::open_in_memory().unwrap();
    let task_id = setup_task(&mut store, 200);

    let ReserveOutcome::Granted(reservation) = store
        .reserve(ReserveRequest {
            task_id,
            session_id: "sess-1",
            plan_id: None,
            class: libra_governor_domain::ReservationClass::OptionalWork,
            amount: ResourceAmount::Tokens(300),
            idempotency_key: "key-1",
            now: now(),
            ttl_secs: 1,
        })
        .unwrap()
    else {
        panic!("expected Granted");
    };

    // Expire it (the TTL elapsed — simulate a crash: the holder never
    // settled before expiry).
    let expired = store
        .expire_stale_reservations(now() + time::Duration::seconds(10))
        .unwrap();
    assert_eq!(expired.len(), 1);
    assert_eq!(
        expired[0].state,
        libra_governor_domain::ReservationState::Expired
    );

    // A late-arriving actual-cost report must not be silently discarded —
    // this is the real pre-existing defect this ticket fixes (previously
    // `settle()` returned `AlreadyFinal` here and wrote nothing).
    let outcome = store
        .settle(
            reservation.id,
            Some(ResourceAmount::Tokens(250)),
            now() + time::Duration::seconds(20),
        )
        .unwrap();
    let SettleOutcome::Settled {
        reservation: settled,
        ..
    } = outcome
    else {
        panic!("expected Settled (late), got {outcome:?}");
    };
    assert!(settled.settled_after_expiry);
    assert_eq!(settled.settled_amount, Some(ResourceAmount::Tokens(250)));

    // A second late settlement is still idempotent — a real actual cost
    // was recorded once, replaying it does not re-apply anything.
    let replay = store
        .settle(
            reservation.id,
            Some(ResourceAmount::Tokens(999)),
            now() + time::Duration::seconds(30),
        )
        .unwrap();
    assert!(matches!(replay, SettleOutcome::AlreadyFinal(_)));
}

#[test]
fn expiring_a_funding_lease_cascade_expires_its_child_account() {
    let mut store = LedgerStore::open_in_memory().unwrap();
    let task_id = setup_task(&mut store, 200);
    let task_account = libra_governor_domain::AccountId::for_task(task_id);
    let session = store
        .ensure_child_account(task_account, AccountLevel::Session, "sess-1", now())
        .unwrap()
        .unwrap()
        .account;
    store
        .grant_sublease(GrantSubleaseRequest {
            parent_account_id: task_account,
            child_account_id: session.account_id,
            child_natural_key: "sess-1",
            amount: ResourceAmount::Tokens(500),
            idempotency_key: "lease-session",
            now: now(),
            ttl_secs: 10,
        })
        .unwrap()
        .unwrap();

    store
        .expire_stale_reservations(now() + time::Duration::seconds(20))
        .unwrap();

    let reopened = store.account(session.account_id).unwrap().unwrap();
    assert_eq!(reopened.state, libra_governor_domain::AccountState::Expired);

    // A new sublease against the now-expired session account must be
    // refused — a crashed/reclaimed account cannot be leased from again.
    let agent = store
        .ensure_child_account(session.account_id, AccountLevel::Agent, "agent-1", now())
        .unwrap()
        .unwrap()
        .account;
    let outcome = store
        .grant_sublease(GrantSubleaseRequest {
            parent_account_id: session.account_id,
            child_account_id: agent.account_id,
            child_natural_key: "agent-1",
            amount: ResourceAmount::Tokens(1),
            idempotency_key: "lease-agent",
            now: now() + time::Duration::seconds(20),
            ttl_secs: 5,
        })
        .unwrap();
    assert_eq!(outcome, Err(AccountError::ParentNotOpen));
}

#[test]
fn native_reservations_carry_the_task_account_id_the_legacy_backfill_also_uses() {
    // Migration 0011's backfill sets every pre-existing reservation's
    // `account_id` to `task_id` and `legacy_pre_0011 = 1` (see the
    // migration file). This test confirms the identity half of that
    // guarantee holds for the ordinary (non-legacy) path too: a reader
    // that only knows `COALESCE(account_id, task_id)` (the idempotency
    // index's own expression) gets the same value regardless of which
    // path wrote the row.
    let mut store = LedgerStore::open_in_memory().unwrap();
    let task_id = setup_task(&mut store, 200);
    let ReserveOutcome::Granted(reservation) = store
        .reserve(ReserveRequest {
            task_id,
            session_id: "sess-1",
            plan_id: None,
            class: libra_governor_domain::ReservationClass::OptionalWork,
            amount: ResourceAmount::Tokens(300),
            idempotency_key: "key-1",
            now: now(),
            ttl_secs: 900,
        })
        .unwrap()
    else {
        panic!("expected Granted");
    };
    assert_eq!(
        reservation.account_id,
        libra_governor_domain::AccountId::for_task(task_id)
    );
    assert!(!reservation.legacy_pre_0011);

    let reread = store.get_reservation(reservation.id).unwrap().unwrap();
    assert_eq!(reread, *reservation);
}

/// Multi-threaded: N session accounts racing to lease from one task's
/// remaining optional headroom must never collectively oversubscribe it
/// — `BEGIN IMMEDIATE` serializes the racing grants, exactly the
/// guarantee [`reservation_concurrency.rs`] already exercises for
/// `reserve()` itself.
#[test]
fn concurrent_sublease_grants_never_oversubscribe_the_parent() {
    let dir = tempfile::tempdir().unwrap();
    let path: PathBuf = dir.path().join("ledger.sqlite3");
    let (task_id, task_account) = {
        let mut store = LedgerStore::open(&path).unwrap();
        let task_id = setup_task(&mut store, 200); // 800 optional headroom
        (task_id, libra_governor_domain::AccountId::for_task(task_id))
    };
    let _ = task_id;

    let path = Arc::new(path);
    let threads: Vec<_> = (0..8)
        .map(|i| {
            let path = Arc::clone(&path);
            std::thread::spawn(move || {
                let mut store = LedgerStore::open(path.as_path()).unwrap();
                let natural_key = format!("sess-{i}");
                let EnsureAccountOutcome { account, .. } = store
                    .ensure_child_account(task_account, AccountLevel::Session, &natural_key, now())
                    .unwrap()
                    .unwrap();
                store
                    .grant_sublease(GrantSubleaseRequest {
                        parent_account_id: task_account,
                        child_account_id: account.account_id,
                        child_natural_key: &natural_key,
                        amount: ResourceAmount::Tokens(200), // 8 * 200 = 1600 > 800 available
                        idempotency_key: &format!("lease-{i}"),
                        now: now(),
                        ttl_secs: 900,
                    })
                    .unwrap()
            })
        })
        .collect();

    let outcomes: Vec<Result<GrantSubleaseOutcome, AccountError>> =
        threads.into_iter().map(|t| t.join().unwrap()).collect();
    let granted = outcomes
        .iter()
        .filter(|o| matches!(o, Ok(GrantSubleaseOutcome::Granted(_))))
        .count();
    // 800 / 200 = exactly 4 can be granted; never more.
    assert_eq!(
        granted, 4,
        "oversubscription: granted {granted} of 8 racers for only 4 slots"
    );

    let store = LedgerStore::open(path.as_path()).unwrap();
    let capacity = store.account_capacity(task_account).unwrap().unwrap();
    let (active, settled): (f64, f64) = {
        let reservations = store.reservations_for_account(task_account).unwrap();
        let active: f64 = reservations
            .iter()
            .filter(|r| r.state == libra_governor_domain::ReservationState::Active)
            .map(|r| r.amount.as_f64())
            .sum();
        let settled: f64 = reservations
            .iter()
            .filter_map(|r| r.settled_amount)
            .map(|a| a.as_f64())
            .sum();
        (active, settled)
    };
    assert!(
        active + settled + capacity.protected_reserve.unwrap().as_f64()
            <= capacity.granted_capacity.unwrap().as_f64() + 1e-6,
        "committed capacity must never exceed what was granted"
    );
}
