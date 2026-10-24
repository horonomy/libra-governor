//! HORO-2219 (S10): retention prune and the export read model for the
//! local monthly shared budget.
//!
//! Evidence class: ledger-level, `TestOwnedLive` -- every pool and row is
//! created by the test; nothing is fed from a production source.

use std::collections::BTreeMap;

use libra_governor_domain::{
    ChargeSource, Confidence, ExecutorKind, GaugeReading, IanaTimeZone, PoolId, PrincipalId,
    QuotaAmount, QuotaUnit,
};
use libra_governor_ledger::{
    CapacitySource, ExecutorAttribution, LedgerStore, MeteredReserveOutcome, MeteredReserveRequest,
    PruneDisposition, PruneRequest, RetentionError, SharedPoolChargeRequest,
    SharedPoolReserveRequest,
};
use time::OffsetDateTime;

/// 2026-10-24T12:00:00Z -- the "current" month is 2026-10 in UTC.
fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_792_843_200).unwrap()
}

fn utc() -> IanaTimeZone {
    IanaTimeZone::new("UTC").unwrap()
}

fn owner() -> PrincipalId {
    PrincipalId("local-budget".to_string())
}

fn pool(alias: &str) -> PoolId {
    PoolId(format!("metered:local-budget/{alias}"))
}

fn new_store() -> (tempfile::TempDir, LedgerStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = LedgerStore::open(dir.path().join("ledger.sqlite3")).unwrap();
    (dir, store)
}

fn ensure(store: &mut LedgerStore, id: &PoolId) {
    store
        .ensure_pool(id, &QuotaUnit::UsdCents, 1_000_000, now())
        .unwrap();
}

fn cap() -> CapacitySource {
    CapacitySource::LocalConfigured { cap: 1_000_000 }
}

/// Settled spend recorded directly.
fn charge(
    store: &mut LedgerStore,
    id: &PoolId,
    session: &str,
    amount: u64,
    key: &str,
    attribution: ExecutorAttribution<'_>,
) {
    store
        .charge_shared(SharedPoolChargeRequest {
            pool_id: id,
            principal_id: &owner(),
            session_id: session,
            amount,
            unit: &QuotaUnit::UsdCents,
            idempotency_key: key,
            now: now(),
            attribution,
            capacity_source: cap(),
        })
        .unwrap();
}

/// An active hold (returns its id) that expires `ttl_secs` after `at`.
fn hold(
    store: &mut LedgerStore,
    id: &PoolId,
    session: &str,
    amount: u64,
    key: &str,
    (at, ttl_secs): (OffsetDateTime, u64),
    attribution: ExecutorAttribution<'_>,
) -> libra_governor_domain::SharedPoolReservationId {
    match store
        .reserve_shared_metered(MeteredReserveRequest {
            base: SharedPoolReserveRequest {
                pool_id: id,
                principal_id: &owner(),
                session_id: session,
                amount,
                unit: &QuotaUnit::UsdCents,
                idempotency_key: key,
                now: at,
                ttl_secs,
                attribution,
            },
            session_cap: None,
            capacity_source: cap(),
        })
        .unwrap()
    {
        MeteredReserveOutcome::Granted(r) => r.id,
        other => panic!("expected Granted, got {other:?}"),
    }
}

fn row_count(store: &LedgerStore, id: &PoolId) -> u64 {
    store
        .shared_pool_export(id)
        .unwrap()
        .map_or(0, |e| e.total_rows)
}

fn prune(store: &mut LedgerStore, keep: u32, dry_run: bool) -> libra_governor_ledger::PruneReport {
    store
        .prune_closed_periods(PruneRequest {
            keep_months: keep,
            now: now(),
            time_zone: &utc(),
            dry_run,
        })
        .unwrap()
}

/// Seeds `alias` with one settled, one released and one expired row (3
/// closed rows, no active hold).
fn seed_closed(store: &mut LedgerStore, alias: &str) {
    let id = pool(alias);
    ensure(store, &id);
    charge(
        store,
        &id,
        "s1",
        100,
        "c1",
        ExecutorAttribution::main_executor(Some(ChargeSource::TurnHold)),
    );
    let released = hold(
        store,
        &id,
        "s2",
        10,
        "h-rel",
        (now(), 3600),
        ExecutorAttribution::default(),
    );
    store.release_shared(released, now()).unwrap();
    hold(
        store,
        &id,
        "s3",
        10,
        "h-exp",
        (now(), 1),
        ExecutorAttribution::default(),
    );
    let reaped = store
        .expire_stale_shared_pool_reservations(now() + time::Duration::seconds(60))
        .unwrap();
    assert_eq!(reaped.len(), 1);
}

#[test]
fn refuses_zero_keep_months() {
    let (_d, mut store) = new_store();
    let err = store
        .prune_closed_periods(PruneRequest {
            keep_months: 0,
            now: now(),
            time_zone: &utc(),
            dry_run: true,
        })
        .unwrap_err();
    assert!(matches!(err, RetentionError::KeepMonthsZero));
}

#[test]
fn dry_run_selects_exactly_what_apply_deletes_and_writes_nothing() {
    let (_d, mut store) = new_store();
    // Current 2026-10, keep 2: 2026-07 and older are eligible; 08, 09, 10 stay.
    for alias in [
        "monthly-2026-05",
        "monthly-2026-07",
        "monthly-2026-08",
        "monthly-2026-09",
        "monthly-2026-10",
    ] {
        seed_closed(&mut store, alias);
    }

    let dry = prune(&mut store, 2, true);
    assert!(dry.dry_run);
    assert_eq!(dry.pools_considered, 5);
    assert_eq!(dry.pools_selected, 2);
    assert_eq!(dry.pools_pruned, 0);
    assert_eq!(dry.rows_selected, 6);
    assert_eq!(dry.rows_deleted, 0);
    assert_eq!(dry.rows_kept, 15);
    for alias in ["monthly-2026-05", "monthly-2026-07"] {
        assert_eq!(row_count(&store, &pool(alias)), 3, "dry-run kept {alias}");
    }

    let applied = prune(&mut store, 2, false);
    assert!(!applied.dry_run);
    assert_eq!(applied.pools_considered, 5);
    assert_eq!(applied.pools_selected, 2);
    assert_eq!(applied.pools_pruned, 2);
    assert_eq!(applied.rows_deleted, 6);
    assert_eq!(applied.rows_kept, 9);
    let pruned: Vec<_> = applied
        .pools
        .iter()
        .filter(|p| p.disposition == PruneDisposition::Pruned)
        .map(|p| p.pool_id.as_str())
        .collect();
    assert_eq!(
        pruned,
        [
            "metered:local-budget/monthly-2026-05",
            "metered:local-budget/monthly-2026-07"
        ]
    );
    assert!(store
        .quota_pool(&pool("monthly-2026-05"))
        .unwrap()
        .is_none());
    assert!(store
        .quota_pool(&pool("monthly-2026-07"))
        .unwrap()
        .is_none());
    for alias in ["monthly-2026-08", "monthly-2026-09", "monthly-2026-10"] {
        assert_eq!(row_count(&store, &pool(alias)), 3, "kept {alias}");
    }

    // Idempotent: a re-run deletes nothing.
    let again = prune(&mut store, 2, false);
    assert_eq!(again.pools_considered, 3);
    assert_eq!(again.pools_pruned, 0);
    assert_eq!(again.rows_deleted, 0);
    assert_eq!(again.rows_kept, 9);
}

#[test]
fn keep_months_boundary_keeps_exactly_n_closed_months() {
    for (keep, oldest_kept, newest_pruned) in [
        (1u32, "monthly-2026-09", "monthly-2026-08"),
        (3, "monthly-2026-07", "monthly-2026-06"),
    ] {
        let (_d, mut store) = new_store();
        seed_closed(&mut store, oldest_kept);
        seed_closed(&mut store, newest_pruned);
        let report = prune(&mut store, keep, false);
        assert_eq!(report.pools_pruned, 1, "keep={keep}");
        assert!(store.quota_pool(&pool(oldest_kept)).unwrap().is_some());
        assert!(store.quota_pool(&pool(newest_pruned)).unwrap().is_none());
    }
}

#[test]
fn never_prunes_current_or_future_period() {
    let (_d, mut store) = new_store();
    seed_closed(&mut store, "monthly-2026-10");
    seed_closed(&mut store, "monthly-2026-11");
    seed_closed(&mut store, "monthly-2030-01");
    let report = prune(&mut store, 1, false);
    assert_eq!(report.pools_pruned, 0);
    assert_eq!(report.rows_deleted, 0);
    assert!(report
        .pools
        .iter()
        .all(|p| p.disposition == PruneDisposition::Retained));
}

#[test]
fn time_zone_decides_which_month_is_current() {
    let (_d, mut store) = new_store();
    seed_closed(&mut store, "monthly-2026-08");
    // 2026-09-30T11:30Z is 2026-10-01 00:30 in Auckland but still September in UTC.
    let at = OffsetDateTime::from_unix_timestamp(1_790_767_800).unwrap();
    let auckland = IanaTimeZone::new("Pacific/Auckland").unwrap();
    let dry_utc = store
        .prune_closed_periods(PruneRequest {
            keep_months: 1,
            now: at,
            time_zone: &utc(),
            dry_run: true,
        })
        .unwrap();
    // UTC: current = 2026-09, August is the kept closed month.
    assert_eq!(dry_utc.pools_selected, 0);
    let dry_nz = store
        .prune_closed_periods(PruneRequest {
            keep_months: 1,
            now: at,
            time_zone: &auckland,
            dry_run: true,
        })
        .unwrap();
    // Auckland: current = 2026-10, August is two months back and eligible.
    assert_eq!(dry_nz.pools_selected, 1);
}

#[test]
fn pool_with_an_active_hold_is_never_pruned() {
    let (_d, mut store) = new_store();
    seed_closed(&mut store, "monthly-2026-01");
    let id = pool("monthly-2026-01");
    // Unexpired hold.
    hold(
        &mut store,
        &id,
        "live",
        5,
        "h-live",
        (now(), 86_400),
        ExecutorAttribution::default(),
    );
    // Expired but not yet reaped: still `active`, still blocks.
    seed_closed(&mut store, "monthly-2026-02");
    hold(
        &mut store,
        &pool("monthly-2026-02"),
        "stale",
        5,
        "h-stale",
        (now() - time::Duration::days(30), 1),
        ExecutorAttribution::default(),
    );

    for dry_run in [true, false] {
        let report = prune(&mut store, 1, dry_run);
        assert_eq!(report.pools_pruned, 0);
        assert_eq!(report.rows_deleted, 0);
        assert!(report
            .pools
            .iter()
            .all(|p| p.disposition == PruneDisposition::SkippedActiveHold));
    }
    assert_eq!(row_count(&store, &id), 4);
    assert_eq!(row_count(&store, &pool("monthly-2026-02")), 4);
}

#[test]
fn unrecognised_and_foreign_pools_are_untouched() {
    let (_d, mut store) = new_store();
    for raw in [
        "metered:local-budget/weekly-2020-01",
        "metered:local-budget/monthly-2020-1",
        "metered:local-budget/monthly-2020-13",
        "metered:local-budget/monthly-2020-01-extra",
        "metered:acme/monthly-2020-01",
        "launch:monthly-2020-01",
    ] {
        let id = PoolId(raw.to_string());
        ensure(&mut store, &id);
        charge(&mut store, &id, "s", 7, "c", ExecutorAttribution::default());
    }
    let report = prune(&mut store, 1, false);
    // Only the four `local-budget` pools are examined at all.
    assert_eq!(report.pools_considered, 4);
    assert_eq!(report.pools_pruned, 0);
    assert_eq!(report.rows_deleted, 0);
    assert!(report
        .pools
        .iter()
        .all(|p| p.disposition == PruneDisposition::SkippedUnrecognisedAlias));
    for raw in [
        "metered:local-budget/weekly-2020-01",
        "metered:acme/monthly-2020-01",
        "launch:monthly-2020-01",
    ] {
        assert_eq!(row_count(&store, &PoolId(raw.to_string())), 1, "{raw}");
    }
}

#[test]
fn provider_snapshot_row_does_not_block_pruning_and_is_removed() {
    let (_d, mut store) = new_store();
    seed_closed(&mut store, "monthly-2026-01");
    let id = pool("monthly-2026-01");
    store
        .ingest_pool_provider_snapshot(
            &id,
            now(),
            Some(now() + time::Duration::hours(1)),
            &GaugeReading::Used {
                used: QuotaAmount::new(QuotaUnit::UsdCents, 1),
                limit: Some(10),
            },
            Confidence::High,
            now(),
        )
        .unwrap();
    let report = prune(&mut store, 1, false);
    assert_eq!(report.pools_pruned, 1);
    assert!(store.pool_provider_snapshot(&id).unwrap().is_none());
}

#[test]
fn prune_beside_reserve_and_settle_on_another_connection_neither_deadlocks_nor_corrupts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    let mut setup = LedgerStore::open(&path).unwrap();
    for m in 1..=6 {
        seed_closed(&mut setup, &format!("monthly-2026-{m:02}"));
    }
    let current = pool("monthly-2026-10");
    ensure(&mut setup, &current);
    drop(setup);

    let writer_path = path.clone();
    let writer_pool = current.clone();
    let writer = std::thread::spawn(move || {
        let mut store = LedgerStore::open(&writer_path).unwrap();
        let mut settled = 0u64;
        for i in 0..150u64 {
            let id = hold(
                &mut store,
                &writer_pool,
                "live",
                10,
                &format!("k{i}"),
                (now(), 3600),
                ExecutorAttribution::main_executor(Some(ChargeSource::TurnHold)),
            );
            store.settle_shared(id, Some(10), now()).unwrap();
            settled += 10;
        }
        settled
    });
    let pruner_path = path.clone();
    let pruner = std::thread::spawn(move || {
        let mut store = LedgerStore::open(&pruner_path).unwrap();
        let mut deleted = 0;
        for _ in 0..40 {
            deleted += store
                .prune_closed_periods(PruneRequest {
                    keep_months: 1,
                    now: now(),
                    time_zone: &utc(),
                    dry_run: false,
                })
                .unwrap()
                .rows_deleted;
        }
        deleted
    });
    let settled = writer.join().unwrap();
    let deleted = pruner.join().unwrap();

    assert_eq!(
        deleted, 18,
        "6 old pools x 3 closed rows, each exactly once"
    );
    let store = LedgerStore::open(&path).unwrap();
    let export = store.shared_pool_export(&current).unwrap().unwrap();
    assert_eq!(export.total_used, settled);
    assert_eq!(export.total_reserved, 0);
    let conn = rusqlite::Connection::open(&path).unwrap();
    let integrity: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(integrity, "ok");
    let fk_violations: i64 = conn
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(fk_violations, 0);
}

#[test]
fn export_executor_sums_equal_pool_totals_across_agent_shapes() {
    let (_d, mut store) = new_store();
    let id = pool("monthly-2026-09");
    ensure(&mut store, &id);
    charge(
        &mut store,
        &id,
        "sess-a",
        100,
        "c1",
        ExecutorAttribution::main_executor(Some(ChargeSource::TurnHold)),
    );
    charge(
        &mut store,
        &id,
        "sess-a",
        40,
        "c2",
        ExecutorAttribution::agent("agent-1", Some(ChargeSource::SubagentTranscript)),
    );
    charge(
        &mut store,
        &id,
        "sess-a",
        7,
        "c3",
        ExecutorAttribution::unattributed(Some(ChargeSource::Gateway)),
    );
    // Legacy-shaped row: no attribution at all.
    charge(
        &mut store,
        &id,
        "sess-b",
        3,
        "c4",
        ExecutorAttribution::default(),
    );
    hold(
        &mut store,
        &id,
        "sess-b",
        25,
        "h1",
        (now(), 3600),
        ExecutorAttribution::main_executor(Some(ChargeSource::TurnHold)),
    );

    let export = store.shared_pool_export(&id).unwrap().unwrap();
    let usage = store.shared_pool_executor_usage(&id).unwrap();
    assert_eq!(export.total_used, usage.total_used);
    assert_eq!(export.total_reserved, usage.total_reserved);
    assert_eq!(export.total_used, 150);
    assert_eq!(export.total_reserved, 25);
    assert_eq!(export.total_rows, 5);
    assert_eq!(
        export.executors.iter().map(|e| e.used).sum::<u64>(),
        export.total_used
    );
    assert_eq!(
        export.executors.iter().map(|e| e.reserved).sum::<u64>(),
        export.total_reserved
    );

    let by: BTreeMap<(String, Option<String>, ExecutorKind), _> = export
        .executors
        .iter()
        .map(|e| {
            (
                (e.session_id.clone(), e.agent_id.clone(), e.executor_kind),
                e,
            )
        })
        .collect();
    let main = by[&("sess-a".into(), None, ExecutorKind::AgentAbsent)];
    assert_eq!((main.used, main.reserved), (100, 0));
    let agent = by[&("sess-a".into(), Some("agent-1".into()), ExecutorKind::Agent)];
    assert_eq!(agent.used, 40);
    assert_eq!(agent.by_charge_source["subagent_transcript"].used, 40);
    let unattributed = by[&("sess-a".into(), None, ExecutorKind::Unattributed)];
    assert_eq!(unattributed.used, 7);
    // Legacy no-attribution row reads as unattributed, charge source unrecorded.
    let legacy = by[&("sess-b".into(), None, ExecutorKind::Unattributed)];
    assert_eq!(legacy.used, 3);
    assert_eq!(legacy.by_charge_source["unrecorded"].rows, 1);
    let sess_b_main = by[&("sess-b".into(), None, ExecutorKind::AgentAbsent)];
    assert_eq!((sess_b_main.used, sess_b_main.reserved), (0, 25));
    assert_eq!(sess_b_main.rows_active, 1);
}

#[test]
fn export_of_a_missing_pool_is_none() {
    let (_d, store) = new_store();
    assert!(store
        .shared_pool_export(&pool("monthly-2026-09"))
        .unwrap()
        .is_none());
}
