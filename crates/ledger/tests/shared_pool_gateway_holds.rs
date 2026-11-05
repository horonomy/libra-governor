//! HORO-2275 (S11c): ledger behaviour for `charge_source = 'gateway'`
//! shared-pool holds -- expire-as-charge, the startup orphan sweep, and
//! lane isolation. Design: docs/design/s11-gateway-governor-held-hard-cap.md
//! section S11c.
//!
//! Evidence class: ledger-level, `TestOwnedLive` -- every pool and row is
//! created by the test; nothing is fed from a production source.
//!
//! Everything here is inert until a gateway row exists:
//! [`no_gateway_rows_leave_every_sweep_and_lane_query_unchanged`] pins that.

use std::sync::{Arc, Barrier};

use libra_governor_domain::{
    ChargeSource, ExecutorKind, PoolId, PrincipalId, QuotaUnit, ReservationState,
    SharedPoolReservation,
};
use libra_governor_ledger::{
    CapacitySource, ExecutorAttribution, LedgerStore, SharedPoolChargeRequest,
    SharedPoolReserveOutcome, SharedPoolReserveRequest, SharedPoolSettleOutcome,
};
use time::{Duration, OffsetDateTime};

const TTL: u64 = 60;

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()
}

fn pool_id() -> PoolId {
    PoolId("metered:local-budget/gateway-holds".to_string())
}

fn owner() -> PrincipalId {
    PrincipalId("local-budget".to_string())
}

fn new_store(capacity: u64) -> (tempfile::TempDir, LedgerStore) {
    let dir = tempfile::tempdir().unwrap();
    let mut store = LedgerStore::open(dir.path().join("ledger.sqlite3")).unwrap();
    store
        .ensure_pool(&pool_id(), &QuotaUnit::UsdCents, capacity, now())
        .unwrap();
    (dir, store)
}

fn reserve_with(
    store: &mut LedgerStore,
    session: &str,
    amount: u64,
    key: &str,
    attribution: ExecutorAttribution<'_>,
) -> SharedPoolReservation {
    let outcome = store
        .reserve_shared(SharedPoolReserveRequest {
            pool_id: &pool_id(),
            principal_id: &owner(),
            session_id: session,
            amount,
            unit: &QuotaUnit::UsdCents,
            idempotency_key: key,
            now: now(),
            ttl_secs: TTL,
            attribution,
        })
        .unwrap();
    match outcome {
        SharedPoolReserveOutcome::Granted(r) => *r,
        other => panic!("expected Granted, got {other:?}"),
    }
}

fn gateway_hold(
    store: &mut LedgerStore,
    session: &str,
    amount: u64,
    key: &str,
) -> SharedPoolReservation {
    reserve_with(
        store,
        session,
        amount,
        key,
        ExecutorAttribution::unattributed(Some(ChargeSource::Gateway)),
    )
}

fn after_ttl() -> OffsetDateTime {
    now() + Duration::seconds(TTL as i64 + 1)
}

fn get(store: &LedgerStore, r: &SharedPoolReservation) -> SharedPoolReservation {
    store.get_shared_reservation(r.id).unwrap().unwrap()
}

/// Every column of every `shared_pool_reservations` row, ordered, as text.
fn dump_reservations(dir: &tempfile::TempDir) -> Vec<String> {
    let conn = rusqlite::Connection::open(dir.path().join("ledger.sqlite3")).unwrap();
    let mut stmt = conn
        .prepare("SELECT * FROM shared_pool_reservations ORDER BY id")
        .unwrap();
    let cols = stmt.column_count();
    stmt.query_map([], |row| {
        let mut parts = Vec::with_capacity(cols);
        for i in 0..cols {
            let v: rusqlite::types::Value = row.get(i)?;
            parts.push(format!("{v:?}"));
        }
        Ok(parts.join("|"))
    })
    .unwrap()
    .map(|r| r.unwrap())
    .collect()
}

#[test]
fn expired_gateway_hold_is_booked_at_amount_with_unknown_usage_and_remaining_unchanged() {
    let (_dir, mut store) = new_store(100);
    let held = gateway_hold(&mut store, "s", 30, "gw:boot-a:req-1");
    let before = store.pool_admission(&pool_id(), now()).unwrap().unwrap();
    assert_eq!(
        (before.settled, before.active, before.remaining()),
        (0, 30, 70)
    );

    let swept = store
        .expire_stale_shared_pool_reservations(after_ttl())
        .unwrap();
    assert_eq!(swept.len(), 1);

    let row = get(&store, &held);
    assert_eq!(row.state, ReservationState::Settled);
    assert_eq!(row.settled_amount, Some(30));
    assert_eq!(row.usage_known, Some(false));
    assert_eq!(row.charge_source, Some(ChargeSource::Gateway));
    let after = store
        .pool_admission(&pool_id(), after_ttl())
        .unwrap()
        .unwrap();
    assert_eq!(
        (after.settled, after.active, after.remaining()),
        (30, 0, 70)
    );
}

#[test]
fn non_gateway_hold_still_expires_and_frees_capacity() {
    let (_dir, mut store) = new_store(100);
    let held = reserve_with(
        &mut store,
        "s",
        30,
        "turn-1",
        ExecutorAttribution::main_executor(Some(ChargeSource::TurnHold)),
    );
    store
        .expire_stale_shared_pool_reservations(after_ttl())
        .unwrap();
    assert_eq!(get(&store, &held).state, ReservationState::Expired);
    let after = store
        .pool_admission(&pool_id(), after_ttl())
        .unwrap()
        .unwrap();
    assert_eq!((after.settled, after.active), (0, 0));
}

#[test]
fn late_settle_of_an_expire_booked_gateway_hold_is_already_final() {
    let (_dir, mut store) = new_store(100);
    let held = gateway_hold(&mut store, "s", 30, "gw:boot-a:req-1");
    store
        .expire_stale_shared_pool_reservations(after_ttl())
        .unwrap();

    let outcome = store.settle_shared(held.id, Some(5), after_ttl()).unwrap();
    let SharedPoolSettleOutcome::AlreadyFinal(row) = outcome else {
        panic!("expected AlreadyFinal, got {outcome:?}");
    };
    assert_eq!(row.settled_amount, Some(30));
    let after = store
        .pool_admission(&pool_id(), after_ttl())
        .unwrap()
        .unwrap();
    assert_eq!(after.settled, 30);
}

#[test]
fn heartbeat_set_never_includes_a_gateway_hold() {
    let (_dir, mut store) = new_store(100);
    let gw = gateway_hold(&mut store, "s", 10, "gw:boot-a:req-1");
    let turn = reserve_with(
        &mut store,
        "s",
        10,
        "turn-1",
        ExecutorAttribution::main_executor(Some(ChargeSource::TurnHold)),
    );
    let renewable = store
        .active_shared_holds_for_session(&owner(), "s", "metered:local-budget/", now())
        .unwrap();
    let ids: Vec<_> = renewable.iter().map(|r| r.id).collect();
    assert_eq!(ids, vec![turn.id]);
    assert!(!ids.contains(&gw.id));
}

#[test]
fn gateway_hold_is_never_the_lane_hold_nor_in_lane_history() {
    let (_dir, mut store) = new_store(100);
    // Same lane as the gateway row: session "s", no agent, unattributed.
    let gw = gateway_hold(&mut store, "s", 10, "gw:boot-a:req-1");
    let lane = store
        .active_shared_hold_for_lane(
            &pool_id(),
            &owner(),
            "s",
            None,
            ExecutorKind::Unattributed,
            now(),
        )
        .unwrap();
    assert_eq!(lane, None, "admit must not see or reuse a gateway row");
    let history = store
        .shared_lane_holds(
            &owner(),
            "s",
            None,
            ExecutorKind::Unattributed,
            "metered:local-budget/",
        )
        .unwrap();
    assert!(history.iter().all(|r| r.id != gw.id));

    // A real hold in the same lane is still found alongside it.
    let turn = reserve_with(
        &mut store,
        "s",
        10,
        "turn-1",
        ExecutorAttribution::unattributed(Some(ChargeSource::TurnHold)),
    );
    let lane = store
        .active_shared_hold_for_lane(
            &pool_id(),
            &owner(),
            "s",
            None,
            ExecutorKind::Unattributed,
            now(),
        )
        .unwrap()
        .map(|r| r.id);
    assert_eq!(lane, Some(turn.id));
}

#[test]
fn orphan_sweep_books_old_boot_rows_and_leaves_the_current_boot_untouched() {
    let (_dir, mut store) = new_store(100);
    let old = gateway_hold(&mut store, "s1", 20, "gw:boot-old:req-1");
    let current = gateway_hold(&mut store, "s2", 20, "gw:boot-new:req-2");
    let turn = reserve_with(
        &mut store,
        "s3",
        10,
        "turn-1",
        ExecutorAttribution::main_executor(Some(ChargeSource::TurnHold)),
    );

    let booked = store
        .settle_orphaned_gateway_holds("gw:boot-new:", now())
        .unwrap();
    assert_eq!(booked.len(), 1);
    assert_eq!(booked[0].id, old.id);

    let old_row = get(&store, &old);
    assert_eq!(old_row.state, ReservationState::Settled);
    assert_eq!(old_row.settled_amount, Some(20));
    assert_eq!(old_row.usage_known, Some(false));
    assert_eq!(get(&store, &current).state, ReservationState::Active);
    assert_eq!(get(&store, &turn).state, ReservationState::Active);
    let admission = store.pool_admission(&pool_id(), now()).unwrap().unwrap();
    assert_eq!((admission.settled, admission.active), (20, 30));

    // A late settle from the old process's draining stream is a no-op.
    let late = store.settle_shared(old.id, Some(3), now()).unwrap();
    assert!(matches!(late, SharedPoolSettleOutcome::AlreadyFinal(_)));
}

#[test]
fn orphan_sweep_books_an_old_boot_row_even_when_its_ttl_has_not_passed() {
    let (_dir, mut store) = new_store(100);
    let old = gateway_hold(&mut store, "s", 20, "gw:boot-old:req-1");
    // `now()` is well inside the TTL: only the boot id marks it an orphan.
    assert!(get(&store, &old).expires_at > now());
    store
        .settle_orphaned_gateway_holds("gw:boot-new:", now())
        .unwrap();
    assert_eq!(get(&store, &old).state, ReservationState::Settled);
}

#[test]
fn orphan_sweep_with_a_malformed_prefix_books_nothing() {
    let (_dir, mut store) = new_store(100);
    let live = gateway_hold(&mut store, "s", 20, "gw:boot-new:req-1");
    for prefix in ["", "gw:", "gw::", "boot-new:", "gw:boot-new"] {
        let booked = store.settle_orphaned_gateway_holds(prefix, now()).unwrap();
        assert!(booked.is_empty(), "prefix {prefix:?}");
        assert_eq!(get(&store, &live).state, ReservationState::Active);
    }
}

#[test]
fn sweeps_run_twice_book_once() {
    let (_dir, mut store) = new_store(100);
    gateway_hold(&mut store, "s1", 20, "gw:boot-old:req-1");
    gateway_hold(&mut store, "s2", 25, "gw:boot-new:req-2");

    assert_eq!(
        store
            .settle_orphaned_gateway_holds("gw:boot-new:", now())
            .unwrap()
            .len(),
        1
    );
    assert!(store
        .settle_orphaned_gateway_holds("gw:boot-new:", now())
        .unwrap()
        .is_empty());

    assert_eq!(
        store
            .expire_stale_shared_pool_reservations(after_ttl())
            .unwrap()
            .len(),
        1
    );
    assert!(store
        .expire_stale_shared_pool_reservations(after_ttl())
        .unwrap()
        .is_empty());

    let admission = store
        .pool_admission(&pool_id(), after_ttl())
        .unwrap()
        .unwrap();
    assert_eq!((admission.settled, admission.active), (45, 0));
}

#[test]
fn no_gateway_rows_leave_every_sweep_and_lane_query_unchanged() {
    let (dir, mut store) = new_store(1_000);
    reserve_with(
        &mut store,
        "s",
        10,
        "legacy-null",
        ExecutorAttribution::default(),
    );
    reserve_with(
        &mut store,
        "s",
        10,
        "turn",
        ExecutorAttribution::main_executor(Some(ChargeSource::TurnHold)),
    );
    reserve_with(
        &mut store,
        "s",
        10,
        "sub",
        ExecutorAttribution::agent("a1", Some(ChargeSource::SubagentTranscript)),
    );
    store
        .charge_shared(SharedPoolChargeRequest {
            pool_id: &pool_id(),
            principal_id: &owner(),
            session_id: "s",
            amount: 7,
            unit: &QuotaUnit::UsdCents,
            idempotency_key: "charge-1",
            now: now(),
            attribution: ExecutorAttribution::main_executor(Some(ChargeSource::TurnHold)),
            capacity_source: CapacitySource::LocalConfigured { cap: 1_000 },
            usage_known: true,
        })
        .unwrap();

    let before = dump_reservations(&dir);
    let session_before = store
        .active_shared_holds_for_session(&owner(), "s", "metered:local-budget/", now())
        .unwrap();
    assert_eq!(session_before.len(), 3, "all three non-gateway holds renew");

    assert!(store
        .settle_orphaned_gateway_holds("gw:boot-new:", now())
        .unwrap()
        .is_empty());
    assert_eq!(dump_reservations(&dir), before, "orphan sweep wrote");

    // The expire sweep keeps its pre-S11c outcome for non-gateway rows.
    let swept = store
        .expire_stale_shared_pool_reservations(after_ttl())
        .unwrap();
    assert_eq!(swept.len(), 3);
    assert!(swept.iter().all(|r| r.state == ReservationState::Expired));
    assert!(swept.iter().all(|r| r.settled_amount.is_none()));
}

#[test]
fn reserve_racing_expire_never_lets_settled_plus_active_exceed_the_cap() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    let cap = 100u64;
    let amount = 10u64;
    {
        let mut store = LedgerStore::open(&path).unwrap();
        store
            .ensure_pool(&pool_id(), &QuotaUnit::UsdCents, cap, now())
            .unwrap();
    }

    let threads = 16usize;
    let barrier = Arc::new(Barrier::new(threads));
    let handles: Vec<_> = (0..threads)
        .map(|i| {
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let mut store = LedgerStore::open(&path).unwrap();
                barrier.wait();
                let mut granted = 0u64;
                for n in 0..6 {
                    if i % 2 == 0 {
                        let key = format!("gw:boot-a:{i}-{n}");
                        let outcome = store
                            .reserve_shared(SharedPoolReserveRequest {
                                pool_id: &pool_id(),
                                principal_id: &owner(),
                                session_id: &format!("s{i}"),
                                amount,
                                unit: &QuotaUnit::UsdCents,
                                idempotency_key: &key,
                                now: now(),
                                ttl_secs: TTL,
                                attribution: ExecutorAttribution::unattributed(Some(
                                    ChargeSource::Gateway,
                                )),
                            })
                            .unwrap();
                        if matches!(outcome, SharedPoolReserveOutcome::Granted(_)) {
                            granted += amount;
                        }
                    } else {
                        store
                            .expire_stale_shared_pool_reservations(after_ttl())
                            .unwrap();
                    }
                    let a = store.pool_admission(&pool_id(), now()).unwrap().unwrap();
                    assert!(
                        a.settled + a.active <= cap,
                        "settled {} + active {} exceeded cap {cap}",
                        a.settled,
                        a.active
                    );
                }
                granted
            })
        })
        .collect();
    let granted: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();

    let mut store = LedgerStore::open(&path).unwrap();
    store
        .expire_stale_shared_pool_reservations(after_ttl())
        .unwrap();
    let a = store
        .pool_admission(&pool_id(), after_ttl())
        .unwrap()
        .unwrap();
    assert_eq!(a.active, 0);
    assert_eq!(a.settled, granted, "every grant ended up booked");
    assert!(a.settled <= cap);
    assert!(granted > 0, "vacuous unless something was granted");
}
