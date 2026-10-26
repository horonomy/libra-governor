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
            usage_known: true,
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

#[test]
fn read_only_open_serves_dry_run_and_export_and_cannot_mutate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    let mut writer = LedgerStore::open(&path).unwrap();
    seed_closed(&mut writer, "monthly-2026-01");
    drop(writer);
    let before = std::fs::read(&path).unwrap();

    let mut ro = LedgerStore::open_read_only(&path).unwrap();
    let dry = ro
        .prune_closed_periods(PruneRequest {
            keep_months: 1,
            now: now(),
            time_zone: &utc(),
            dry_run: true,
        })
        .unwrap();
    assert_eq!(dry.pools_selected, 1);
    assert_eq!(dry.rows_selected, 3);
    assert!(ro
        .shared_pool_export(&pool("monthly-2026-01"))
        .unwrap()
        .is_some());
    // The apply path cannot write through a read-only handle.
    assert!(ro
        .prune_closed_periods(PruneRequest {
            keep_months: 1,
            now: now(),
            time_zone: &utc(),
            dry_run: false,
        })
        .is_err());
    drop(ro);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "main db file untouched"
    );
    assert_eq!(
        row_count(&LedgerStore::open(&path).unwrap(), &pool("monthly-2026-01")),
        3
    );
}

#[test]
fn read_only_open_refuses_a_schema_that_is_not_current() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    drop(LedgerStore::open(&path).unwrap());
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute("DELETE FROM schema_migrations WHERE version = (SELECT MAX(version) FROM schema_migrations)", [])
        .unwrap();
    drop(conn);
    let err = LedgerStore::open_read_only(&path).err().unwrap();
    assert!(matches!(
        err,
        libra_governor_ledger::LedgerError::SchemaNotCurrent { .. }
    ));
}

#[test]
fn prune_keeps_each_subagent_family_total_outside_the_pruned_rows() {
    let (_dir, mut store) = new_store();
    let old = pool("monthly-2026-07");
    ensure(&mut store, &old);
    let fam = "subagent-transcript:2:s1|4:aaa1|";
    for (key, amount) in [(format!("{fam}c100"), 100), (format!("{fam}c250"), 150)] {
        charge(
            &mut store,
            &old,
            "s1",
            amount,
            &key,
            ExecutorAttribution::main_executor(Some(ChargeSource::SubagentTranscript)),
        );
    }
    // Not a subagent-transcript family: never folded into the floor.
    charge(
        &mut store,
        &old,
        "s1",
        7,
        "charge:2:s1|2:k1",
        ExecutorAttribution::main_executor(Some(ChargeSource::TurnHold)),
    );
    let total = |store: &LedgerStore, prefix: &str| {
        store
            .shared_charges_total_by_key_prefix(&owner(), prefix, "metered:local-budget/")
            .unwrap()
    };
    assert_eq!(total(&store, fam), 250);
    let prefix = "metered:local-budget/";
    assert_eq!(store.shared_retention_horizon(prefix).unwrap(), None);
    assert_eq!(prune(&mut store, 1, true).pools_pruned, 0);
    assert_eq!(
        store.shared_retention_horizon(prefix).unwrap(),
        None,
        "a dry run records no horizon"
    );
    assert_eq!(prune(&mut store, 1, false).pools_pruned, 1);
    assert_eq!(
        store.shared_retention_horizon(prefix).unwrap(),
        Some(old.clone())
    );
    assert_eq!(row_count(&store, &old), 0);
    assert_eq!(
        total(&store, fam),
        250,
        "the family total survives the prune"
    );
    assert_eq!(
        total(&store, "charge:2:s1|"),
        0,
        "other charges are not kept"
    );

    // A later pruned pool adds to the same family; a retained row is not
    // counted twice (the floor holds only pruned rows).
    let older = pool("monthly-2026-06");
    ensure(&mut store, &older);
    charge(
        &mut store,
        &older,
        "s1",
        50,
        &format!("{fam}c300"),
        ExecutorAttribution::main_executor(Some(ChargeSource::SubagentTranscript)),
    );
    assert_eq!(total(&store, fam), 300);
    assert_eq!(prune(&mut store, 1, false).pools_pruned, 1);
    assert_eq!(total(&store, fam), 300);
    assert_eq!(
        store.shared_retention_horizon(prefix).unwrap(),
        Some(old.clone()),
        "the horizon never moves backwards"
    );
    // A dry run changes nothing.
    assert_eq!(prune(&mut store, 1, true).pools_pruned, 0);
    assert_eq!(total(&store, fam), 300);
}

#[test]
fn a_prune_that_fails_midway_leaves_no_floor_and_no_horizon() {
    let (dir, mut store) = new_store();
    let old = pool("monthly-2026-07");
    ensure(&mut store, &old);
    charge(
        &mut store,
        &old,
        "s1",
        100,
        "subagent-transcript:2:s1|4:aaa1|c100",
        ExecutorAttribution::main_executor(Some(ChargeSource::SubagentTranscript)),
    );
    // Force the last statement of the prune transaction (the pool delete)
    // to fail, after the floor, horizon and reservation deletes ran.
    let raw = rusqlite::Connection::open(dir.path().join("ledger.sqlite3")).unwrap();
    raw.execute_batch(
        "CREATE TRIGGER boom BEFORE DELETE ON quota_pools
         BEGIN SELECT RAISE(ABORT, 'boom'); END;",
    )
    .unwrap();
    let failed = store.prune_closed_periods(PruneRequest {
        keep_months: 1,
        now: now(),
        time_zone: &utc(),
        dry_run: false,
    });
    assert!(failed.is_err(), "the forced failure surfaces");
    let count = |table: &str| -> i64 {
        raw.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(count("shared_charge_floor"), 0, "no floor row survives");
    assert_eq!(count("shared_retention_horizon"), 0, "no horizon survives");
    assert_eq!(row_count(&store, &old), 1, "the charge row is intact");

    raw.execute_batch("DROP TRIGGER boom").unwrap();
    assert_eq!(prune(&mut store, 1, false).pools_pruned, 1);
    assert_eq!(count("shared_charge_floor"), 1);
    assert_eq!(count("shared_retention_horizon"), 1);
}

#[test]
fn forgetting_a_holds_prompts_frees_the_id_for_a_retry_and_touches_no_other_hold() {
    use libra_governor_ledger::SharedPoolRenewOutcome;
    let (_dir, mut store) = new_store();
    let id = pool("monthly-2026-07");
    ensure(&mut store, &id);
    let mk = |store: &mut LedgerStore, session: &str, key: &str| {
        hold(
            store,
            &id,
            session,
            10,
            key,
            (now(), 3600),
            ExecutorAttribution::default(),
        )
    };
    let h1 = mk(&mut store, "s1", "hold-1");
    let h2 = mk(&mut store, "s2", "hold-2");
    for (h, session) in [(h1, "s1"), (h2, "s2")] {
        assert!(matches!(
            store
                .renew_shared_for_prompt(h, &owner(), session, now(), 3600, Some("p1"))
                .unwrap(),
            SharedPoolRenewOutcome::Renewed(_)
        ));
    }
    store.forget_hold_prompts(h1).unwrap();
    assert_eq!(store.hold_for_prompt(&owner(), "s1", "p1").unwrap(), None);
    assert_eq!(
        store.hold_for_prompt(&owner(), "s2", "p1").unwrap(),
        Some(h2),
        "another hold's mapping is untouched"
    );
}

#[test]
fn a_renewal_records_its_prompt_atomically_and_a_prune_keeps_it() {
    use libra_governor_ledger::SharedPoolRenewOutcome;
    let (_dir, mut store) = new_store();
    let id = pool("monthly-2026-07");
    ensure(&mut store, &id);
    let h = hold(
        &mut store,
        &id,
        "s1",
        10,
        "hold-1",
        (now(), 3600),
        ExecutorAttribution::default(),
    );
    let renew = |store: &mut LedgerStore, prompt: &str| {
        store
            .renew_shared_for_prompt(h, &owner(), "s1", now(), 3600, Some(prompt))
            .unwrap()
    };
    assert!(matches!(
        renew(&mut store, "p2"),
        SharedPoolRenewOutcome::Renewed(_)
    ));
    assert!(matches!(
        renew(&mut store, "p3"),
        SharedPoolRenewOutcome::Renewed(_)
    ));
    let got = |store: &LedgerStore, p: &str| store.hold_for_prompt(&owner(), "s1", p).unwrap();
    assert_eq!((got(&store, "p2"), got(&store, "p3")), (Some(h), Some(h)));
    assert_eq!(got(&store, "p9"), None);
    assert_eq!(
        store.hold_for_prompt(&owner(), "other", "p2").unwrap(),
        None,
        "scoped to the session"
    );

    // A refused renewal (the hold is no longer active) records nothing.
    store.release_shared(h, now()).unwrap();
    assert!(matches!(
        renew(&mut store, "p4"),
        SharedPoolRenewOutcome::Refused(_)
    ));
    assert_eq!(got(&store, "p4"), None, "a refused renewal leaves no row");

    // The mapping outlives the pruned hold.
    assert_eq!(prune(&mut store, 1, false).pools_pruned, 1);
    assert_eq!(row_count(&store, &id), 0);
    assert_eq!(got(&store, "p2"), Some(h));
}

#[test]
fn a_grant_records_its_prompt_in_its_own_transaction_and_a_refusal_records_nothing() {
    let (_dir, mut store) = new_store();
    let id = pool("monthly-2026-10");
    ensure(&mut store, &id);
    let reserve = |store: &mut LedgerStore, amount: u64, key: &str, prompt: &str| {
        store
            .reserve_shared_metered_for_prompt(
                MeteredReserveRequest {
                    base: SharedPoolReserveRequest {
                        pool_id: &id,
                        principal_id: &owner(),
                        session_id: "s1",
                        amount,
                        unit: &QuotaUnit::UsdCents,
                        idempotency_key: key,
                        now: now(),
                        ttl_secs: 3600,
                        attribution: ExecutorAttribution::default(),
                    },
                    session_cap: None,
                    capacity_source: cap(),
                },
                Some(prompt),
            )
            .unwrap()
    };
    let granted = match reserve(&mut store, 10, "k1", "p1") {
        MeteredReserveOutcome::Granted(h) => h.id,
        other => panic!("{other:?}"),
    };
    let got = |store: &LedgerStore, p: &str| store.hold_for_prompt(&owner(), "s1", p).unwrap();
    assert_eq!(got(&store, "p1"), Some(granted));
    // A retry of the same key resolves to the same live hold.
    assert!(matches!(
        reserve(&mut store, 10, "k1", "p1b"),
        MeteredReserveOutcome::AlreadyGranted(_)
    ));
    assert_eq!(got(&store, "p1b"), Some(granted));
    // Over the cap: refused, and no prompt row is left behind.
    assert!(matches!(
        reserve(&mut store, 5_000_000, "k2", "p2"),
        MeteredReserveOutcome::Insufficient { .. }
    ));
    assert_eq!(got(&store, "p2"), None);
}

#[test]
fn a_failed_prompt_insert_rolls_the_whole_grant_back_and_leaves_the_store_usable() {
    let (dir, mut store) = new_store();
    let id = pool("monthly-2026-10");
    ensure(&mut store, &id);
    let raw = rusqlite::Connection::open(dir.path().join("ledger.sqlite3")).unwrap();
    raw.execute_batch(
        "CREATE TRIGGER boom BEFORE INSERT ON shared_hold_prompts
         BEGIN SELECT RAISE(ABORT, 'boom'); END;",
    )
    .unwrap();
    let reserve = |store: &mut LedgerStore, key: &str, prompt: &str| {
        store.reserve_shared_metered_for_prompt(
            MeteredReserveRequest {
                base: SharedPoolReserveRequest {
                    pool_id: &id,
                    principal_id: &owner(),
                    session_id: "s1",
                    amount: 10,
                    unit: &QuotaUnit::UsdCents,
                    idempotency_key: key,
                    now: now(),
                    ttl_secs: 3600,
                    attribution: ExecutorAttribution::default(),
                },
                session_cap: None,
                capacity_source: cap(),
            },
            Some(prompt),
        )
    };
    assert!(
        reserve(&mut store, "k1", "p1").is_err(),
        "the insert failure surfaces"
    );
    let count = |table: &str| -> i64 {
        raw.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(
        count("shared_pool_reservations"),
        0,
        "the hold was rolled back"
    );
    assert_eq!(count("shared_hold_prompts"), 0);
    assert_eq!(row_count(&store, &id), 0);

    // The connection is back in autocommit: a later grant works and is durable.
    raw.execute_batch("DROP TRIGGER boom").unwrap();
    assert!(matches!(
        reserve(&mut store, "k1", "p1").unwrap(),
        MeteredReserveOutcome::Granted(_)
    ));
    assert_eq!(count("shared_pool_reservations"), 1);
    assert_eq!(count("shared_hold_prompts"), 1);
}
