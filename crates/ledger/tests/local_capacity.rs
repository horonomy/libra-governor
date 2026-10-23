//! HORO-2194 (S3): a locally configured capacity source and
//! executor-keyed shared-pool rows.
//!
//! Evidence class: ledger-level, `TestOwnedLive` -- every pool here is
//! created by the test. Nothing here is fed from a production source; no
//! production call site constructs a `LocalConfigured` pool yet (that is
//! S4).
//!
//! The multi-process race re-executes this very test binary as 8 real OS
//! processes, each with its own `LedgerStore` on one SQLite file, released
//! together by a gate file (the shape `metered_pool_accounting.rs` 2b
//! uses). Children are killed and reaped on drop and every wait loop is
//! deadline-bounded.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use libra_governor_domain::{
    ChargeSource, Confidence, ExecutorKind, GaugeReading, PoolId, PrincipalId, QuotaAmount,
    QuotaUnit,
};
use libra_governor_ledger::{
    CapacitySource, ExecutorAttribution, LedgerStore, MeteredReserveOutcome, MeteredReserveRequest,
    SharedPoolChargeOutcome, SharedPoolChargeRequest, SharedPoolReserveRequest,
    SharedPoolSettleOutcome,
};
use time::OffsetDateTime;

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()
}

fn pool() -> PoolId {
    PoolId("metered:acme/local".to_string())
}

fn owner() -> PrincipalId {
    PrincipalId("acme".to_string())
}

fn local(cap: u64) -> CapacitySource {
    CapacitySource::LocalConfigured { cap }
}

fn store_with_pool(stored_capacity: u64) -> (tempfile::TempDir, LedgerStore) {
    let dir = tempfile::tempdir().unwrap();
    let mut store = LedgerStore::open(dir.path().join("ledger.sqlite3")).unwrap();
    store
        .ensure_pool(&pool(), &QuotaUnit::UsdCents, stored_capacity, now())
        .unwrap();
    (dir, store)
}

fn reserve(
    store: &mut LedgerStore,
    source: CapacitySource,
    session: &str,
    amount: u64,
    key: &str,
    attribution: ExecutorAttribution<'_>,
) -> MeteredReserveOutcome {
    store
        .reserve_shared_metered(MeteredReserveRequest {
            base: SharedPoolReserveRequest {
                pool_id: &pool(),
                principal_id: &owner(),
                session_id: session,
                amount,
                unit: &QuotaUnit::UsdCents,
                idempotency_key: key,
                now: now(),
                ttl_secs: 3600,
                attribution,
            },
            session_cap: None,
            capacity_source: source,
        })
        .unwrap()
}

fn charge(
    store: &mut LedgerStore,
    source: CapacitySource,
    session: &str,
    amount: u64,
    key: &str,
    attribution: ExecutorAttribution<'_>,
) -> SharedPoolChargeOutcome {
    store
        .charge_shared(SharedPoolChargeRequest {
            pool_id: &pool(),
            principal_id: &owner(),
            session_id: session,
            amount,
            unit: &QuotaUnit::UsdCents,
            idempotency_key: key,
            now: now(),
            attribution,
            capacity_source: source,
        })
        .unwrap()
}

fn granted_id(outcome: MeteredReserveOutcome) -> libra_governor_domain::SharedPoolReservationId {
    match outcome {
        MeteredReserveOutcome::Granted(r) => r.id,
        other => panic!("expected Granted, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// (a) multi-process race against LocalConfigured
// ---------------------------------------------------------------------

const WORKER_ENV: &str = "HORO2194_WORKER";
const WORKER_RESERVES: usize = 3;
const WORKER_AMOUNT: u64 = 3_000;
const RACE_CAP: u64 = 50_000;

/// Not a real test: the body of one racing process. A no-op unless the
/// parent re-executes this binary with `HORO2194_WORKER` set.
#[test]
fn local_capacity_race_worker_entrypoint() {
    let Ok(spec) = std::env::var(WORKER_ENV) else {
        return;
    };
    // spec = "<db>|<ready>|<gate>|<worker-index>"
    let parts: Vec<&str> = spec.split('|').collect();
    let (db, ready, gate, idx) = (parts[0], parts[1], parts[2], parts[3]);
    let mut store = LedgerStore::open(Path::new(db)).unwrap();
    std::fs::write(ready, b"").unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !Path::new(gate).exists() {
        assert!(Instant::now() < deadline, "gate never opened");
        std::thread::sleep(Duration::from_millis(1));
    }
    let session = format!("proc-{idx}");
    for n in 0..WORKER_RESERVES {
        let key = format!("{idx}-{n}");
        let tag = match reserve(
            &mut store,
            local(RACE_CAP),
            &session,
            WORKER_AMOUNT,
            &key,
            ExecutorAttribution::main_executor(Some(ChargeSource::TurnHold)),
        ) {
            MeteredReserveOutcome::Granted(r) => format!("GRANTED {}", r.amount),
            MeteredReserveOutcome::Insufficient { .. } => "REFUSED".to_string(),
            other => panic!("unexpected outcome {other:?}"),
        };
        println!("HORO2194_RESULT {tag}");
    }
}

/// Kills and reaps every child on drop.
struct Reaper(Vec<std::process::Child>);

impl Drop for Reaper {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn wait_all_exist(paths: &[PathBuf]) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !paths.iter().all(|p| p.exists()) {
        assert!(Instant::now() < deadline, "contenders never became ready");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn multi_process_race_against_local_configured_cap_never_exceeds_it() {
    const PROCS: usize = 8;
    // The stored pool capacity is deliberately far above the cap: if the
    // gate ever fell back to it, the race would over-grant.
    let (dir, store) = store_with_pool(10_000_000);
    drop(store);
    let db = dir.path().join("ledger.sqlite3");

    let gate = dir.path().join("go");
    let readies: Vec<PathBuf> = (0..PROCS)
        .map(|i| dir.path().join(format!("r{i}")))
        .collect();
    let exe = std::env::current_exe().unwrap();
    let mut reaper = Reaper(Vec::new());
    for (i, ready) in readies.iter().enumerate() {
        reaper.0.push(
            Command::new(&exe)
                .args([
                    "--exact",
                    "local_capacity_race_worker_entrypoint",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(
                    WORKER_ENV,
                    format!(
                        "{}|{}|{}|{i}",
                        db.display(),
                        ready.display(),
                        gate.display()
                    ),
                )
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        );
    }
    wait_all_exist(&readies);
    std::fs::write(&gate, b"").unwrap();

    let (mut granted, mut refused, mut granted_sum) = (0u64, 0u64, 0u64);
    while !reaper.0.is_empty() {
        let child = reaper.0.remove(0);
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "worker failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        for line in BufReader::new(&out.stdout[..]).lines() {
            let line = line.unwrap();
            let marker = "HORO2194_RESULT ";
            let Some(pos) = line.find(marker) else {
                continue;
            };
            let tag = &line[pos + marker.len()..];
            if let Some(a) = tag.strip_prefix("GRANTED ") {
                granted += 1;
                granted_sum += a.parse::<u64>().unwrap();
            } else {
                assert_eq!(tag, "REFUSED");
                refused += 1;
            }
        }
    }
    let attempts = (PROCS * WORKER_RESERVES) as u64;
    assert_eq!(granted + refused, attempts, "every attempt answered once");
    assert!(granted_sum <= RACE_CAP, "over-allocated: {granted_sum}");
    assert_eq!(
        granted,
        RACE_CAP / WORKER_AMOUNT,
        "exactly floor(cap/amount)"
    );

    let store = LedgerStore::open(&db).unwrap();
    let report = store.shared_pool_executor_usage(&pool()).unwrap();
    assert_eq!(report.total_reserved, granted_sum);
    let admission = store
        .pool_admission_for(&pool(), now(), local(RACE_CAP))
        .unwrap()
        .unwrap();
    assert_eq!(admission.remaining(), (RACE_CAP - granted_sum) as i64);
}

// ---------------------------------------------------------------------
// LocalConfigured semantics
// ---------------------------------------------------------------------

#[test]
fn local_configured_ignores_snapshot_and_stored_capacity() {
    let (_dir, mut store) = store_with_pool(100);

    // No snapshot at all: ProviderSnapshot fails closed, LocalConfigured
    // does not need one.
    assert!(matches!(
        reserve(
            &mut store,
            CapacitySource::ProviderSnapshot,
            "s",
            10,
            "p",
            Default::default()
        ),
        MeteredReserveOutcome::SnapshotUnusable(_)
    ));
    // The configured cap (1_000) governs, not the stored capacity (100).
    granted_id(reserve(
        &mut store,
        local(1_000),
        "s",
        900,
        "a",
        Default::default(),
    ));
    assert!(matches!(
        reserve(&mut store, local(1_000), "s", 101, "b", Default::default()),
        MeteredReserveOutcome::Insufficient { .. }
    ));
    granted_id(reserve(
        &mut store,
        local(1_000),
        "s",
        100,
        "c",
        Default::default(),
    ));
    assert!(matches!(
        reserve(&mut store, local(1_000), "s", 1, "d", Default::default()),
        MeteredReserveOutcome::Insufficient { .. }
    ));

    // An undisclosed provider snapshot (which blocks ProviderSnapshot)
    // does not affect a LocalConfigured admission.
    store
        .ingest_pool_provider_snapshot(
            &pool(),
            now(),
            None,
            &GaugeReading::Undisclosed,
            Confidence::High,
            now(),
        )
        .unwrap();
    granted_id(reserve(
        &mut store,
        local(2_000),
        "s",
        500,
        "e",
        Default::default(),
    ));
    assert!(matches!(
        reserve(
            &mut store,
            CapacitySource::ProviderSnapshot,
            "s",
            1,
            "f",
            Default::default()
        ),
        MeteredReserveOutcome::SnapshotUnusable(_)
    ));
}

#[test]
fn local_configured_cap_is_the_live_config_so_lowering_it_refuses_at_once() {
    let (_dir, mut store) = store_with_pool(10_000);
    granted_id(reserve(
        &mut store,
        local(1_000),
        "s",
        800,
        "a",
        Default::default(),
    ));
    // Operator lowers the cap below what is already held: nothing new is
    // admitted and the shortfall is visible, not clamped.
    assert!(matches!(
        reserve(&mut store, local(500), "s", 1, "b", Default::default()),
        MeteredReserveOutcome::Insufficient { admission, .. } if admission.remaining() == -300
    ));
}

// ---------------------------------------------------------------------
// (b) charge idempotency
// ---------------------------------------------------------------------

#[test]
fn charge_is_idempotent_on_its_key_and_scoped_like_reserve_keys() {
    let (_dir, mut store) = store_with_pool(0);
    let attr = ExecutorAttribution::agent("a1", Some(ChargeSource::SubagentTranscript));
    let first = match charge(&mut store, local(1_000), "s1", 300, "k", attr) {
        SharedPoolChargeOutcome::Charged { reservation, .. } => reservation,
        other => panic!("expected Charged, got {other:?}"),
    };
    let second = match charge(&mut store, local(1_000), "s1", 300, "k", attr) {
        SharedPoolChargeOutcome::AlreadyCharged(r) => r,
        other => panic!("expected AlreadyCharged, got {other:?}"),
    };
    assert_eq!(*first, *second, "same key => same row, same result");
    let report = store.shared_pool_executor_usage(&pool()).unwrap();
    assert_eq!(report.total_used, 300, "no second row, no double count");
    assert_eq!(report.executors.len(), 1);

    // Same key, different amount or session: a conflict, nothing written.
    assert!(matches!(
        charge(&mut store, local(1_000), "s1", 301, "k", attr),
        SharedPoolChargeOutcome::KeyConflict(_)
    ));
    assert!(matches!(
        charge(&mut store, local(1_000), "other", 300, "k", attr),
        SharedPoolChargeOutcome::KeyConflict(_)
    ));
    // A charge key that names an existing *hold* is a conflict too.
    granted_id(reserve(
        &mut store,
        local(1_000),
        "s1",
        50,
        "hold",
        Default::default(),
    ));
    assert!(matches!(
        charge(&mut store, local(1_000), "s1", 50, "hold", attr),
        SharedPoolChargeOutcome::KeyConflict(_)
    ));
    assert_eq!(
        store
            .shared_pool_executor_usage(&pool())
            .unwrap()
            .total_used,
        300
    );

    // Scope includes the principal: the same key under another principal
    // is its own charge.
    let other_principal = PrincipalId("someone-else".to_string());
    let outcome = store
        .charge_shared(SharedPoolChargeRequest {
            pool_id: &pool(),
            principal_id: &other_principal,
            session_id: "s1",
            amount: 300,
            unit: &QuotaUnit::UsdCents,
            idempotency_key: "k",
            now: now(),
            attribution: attr,
            capacity_source: local(1_000),
        })
        .unwrap();
    assert!(matches!(outcome, SharedPoolChargeOutcome::Charged { .. }));
    assert_eq!(
        store
            .shared_pool_executor_usage(&pool())
            .unwrap()
            .total_used,
        600
    );
}

#[test]
fn charge_on_a_missing_pool_or_wrong_unit_is_not_silently_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = LedgerStore::open(dir.path().join("l.sqlite3")).unwrap();
    assert!(matches!(
        charge(&mut store, local(10), "s", 1, "k", Default::default()),
        SharedPoolChargeOutcome::NoPool
    ));
    store
        .ensure_pool(&pool(), &QuotaUnit::UsdCents, 10, now())
        .unwrap();
    let err = store.charge_shared(SharedPoolChargeRequest {
        pool_id: &pool(),
        principal_id: &owner(),
        session_id: "s",
        amount: 1,
        unit: &QuotaUnit::Tokens,
        idempotency_key: "k",
        now: now(),
        attribution: Default::default(),
        capacity_source: local(10),
    });
    assert!(err.is_err(), "unit mismatch must be an error, not a row");
}

// ---------------------------------------------------------------------
// (c) charge beyond cap succeeds, shows as overrun, blocks reserves
// ---------------------------------------------------------------------

#[test]
fn charge_beyond_cap_succeeds_shows_overrun_and_blocks_later_reserves() {
    let (_dir, mut store) = store_with_pool(0);
    granted_id(reserve(
        &mut store,
        local(1_000),
        "s1",
        400,
        "h",
        Default::default(),
    ));

    let outcome = charge(
        &mut store,
        local(1_000),
        "s1",
        900,
        "big",
        ExecutorAttribution::agent("a1", Some(ChargeSource::Gateway)),
    );
    let SharedPoolChargeOutcome::Charged {
        reservation,
        admission,
        overrun,
        ..
    } = outcome
    else {
        panic!("a charge must never be refused for capacity: {outcome:?}");
    };
    assert_eq!(reservation.settled_amount, Some(900));
    // committed = 400 active + 900 settled = 1_300 against cap 1_000.
    assert_eq!(admission.remaining(), -300, "negative, never clamped");
    assert_eq!(overrun, 300);

    let status = store
        .pool_admission_for(&pool(), now(), local(1_000))
        .unwrap()
        .unwrap();
    assert_eq!(status.remaining(), -300);
    assert_eq!(status.overrun(), 300);

    assert!(matches!(
        reserve(&mut store, local(1_000), "s2", 1, "late", Default::default()),
        MeteredReserveOutcome::Insufficient { admission, .. } if admission.remaining() == -300
    ));

    // Even a further charge is accepted; the overrun just grows.
    let SharedPoolChargeOutcome::Charged { overrun, .. } = charge(
        &mut store,
        local(1_000),
        "s1",
        50,
        "more",
        Default::default(),
    ) else {
        panic!("charge refused");
    };
    assert_eq!(overrun, 350);
}

// ---------------------------------------------------------------------
// (d) charges reduce remaining
// ---------------------------------------------------------------------

#[test]
fn charges_count_toward_remaining_alongside_active_and_settled() {
    let (_dir, mut store) = store_with_pool(0);
    let cap = local(1_000);
    let hold = granted_id(reserve(&mut store, cap, "s1", 200, "h", Default::default()));
    let status = |store: &LedgerStore| {
        store
            .pool_admission_for(&pool(), now(), cap)
            .unwrap()
            .unwrap()
    };
    assert_eq!(status(&store).remaining(), 800);

    charge(&mut store, cap, "s1", 300, "c1", Default::default());
    assert_eq!(status(&store).remaining(), 500, "cap - settled - active");

    match store.settle_shared(hold, Some(150), now()).unwrap() {
        SharedPoolSettleOutcome::Settled { .. } => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(status(&store).remaining(), 550);

    // Exactly the remaining amount is admitted; one more is refused.
    assert!(matches!(
        reserve(&mut store, cap, "s2", 551, "x", Default::default()),
        MeteredReserveOutcome::Insufficient { .. }
    ));
    granted_id(reserve(&mut store, cap, "s2", 550, "y", Default::default()));
    assert_eq!(status(&store).remaining(), 0);
}

// ---------------------------------------------------------------------
// (e) executor usage reconciles with pool totals
// ---------------------------------------------------------------------

#[test]
fn executor_usage_sums_reconcile_with_pool_totals_including_unattributed() {
    let (_dir, mut store) = store_with_pool(0);
    let cap = local(1_000_000);

    // Same session id "s1": main executor vs two distinct agents.
    let main_hold = granted_id(reserve(
        &mut store,
        cap,
        "s1",
        100,
        "m",
        ExecutorAttribution::main_executor(Some(ChargeSource::TurnHold)),
    ));
    store.settle_shared(main_hold, Some(80), now()).unwrap();
    granted_id(reserve(
        &mut store,
        cap,
        "s1",
        40,
        "m2",
        ExecutorAttribution::main_executor(Some(ChargeSource::TurnHold)),
    ));
    granted_id(reserve(
        &mut store,
        cap,
        "s1",
        25,
        "a1h",
        ExecutorAttribution::agent("a1", Some(ChargeSource::TurnHold)),
    ));
    charge(
        &mut store,
        cap,
        "s1",
        500,
        "a1c",
        ExecutorAttribution::agent("a1", Some(ChargeSource::SubagentTranscript)),
    );
    charge(
        &mut store,
        cap,
        "s1",
        7,
        "a2c",
        ExecutorAttribution::agent("a2", Some(ChargeSource::Gateway)),
    );
    // Session known, agent unknown (explicit), and a row written with no
    // attribution at all (legacy shape): both are unattributed.
    charge(
        &mut store,
        cap,
        "s1",
        11,
        "u1",
        ExecutorAttribution::unattributed(Some(ChargeSource::Gateway)),
    );
    charge(&mut store, cap, "s1", 13, "legacy", Default::default());
    granted_id(reserve(
        &mut store,
        cap,
        "s1",
        3,
        "legacy-hold",
        Default::default(),
    ));
    // A different session with its own main executor.
    charge(
        &mut store,
        cap,
        "s2",
        1_000,
        "s2m",
        ExecutorAttribution::main_executor(None),
    );
    // Released / expired holds contribute to neither side.
    let released = granted_id(reserve(
        &mut store,
        cap,
        "s2",
        999,
        "rel",
        Default::default(),
    ));
    store.release_shared(released, now()).unwrap();

    let report = store.shared_pool_executor_usage(&pool()).unwrap();
    let used: u64 = report.executors.iter().map(|e| e.used).sum();
    let reserved: u64 = report.executors.iter().map(|e| e.reserved).sum();
    assert_eq!(used, report.total_used, "{report:#?}");
    assert_eq!(reserved, report.total_reserved, "{report:#?}");
    assert_eq!(report.total_used, 80 + 500 + 7 + 11 + 13 + 1_000);
    assert_eq!(report.total_reserved, 40 + 25 + 3);

    // Totals agree with the independent admission computation.
    let admission = store
        .pool_admission_for(&pool(), now(), cap)
        .unwrap()
        .unwrap();
    assert_eq!(
        (admission.settled, admission.active),
        (report.total_used, report.total_reserved)
    );

    let find = |session: &str, agent: Option<&str>, kind: ExecutorKind| {
        report
            .executors
            .iter()
            .find(|e| {
                e.session_id == session && e.agent_id.as_deref() == agent && e.executor_kind == kind
            })
            .map(|e| (e.used, e.reserved))
    };
    assert_eq!(find("s1", None, ExecutorKind::AgentAbsent), Some((80, 40)));
    assert_eq!(find("s1", Some("a1"), ExecutorKind::Agent), Some((500, 25)));
    assert_eq!(find("s1", Some("a2"), ExecutorKind::Agent), Some((7, 0)));
    assert_eq!(
        find("s1", None, ExecutorKind::Unattributed),
        Some((24, 3)),
        "explicit unattributed and NULL-kind rows share one key"
    );
    assert_eq!(
        find("s2", None, ExecutorKind::AgentAbsent),
        Some((1_000, 0))
    );
    assert_eq!(report.executors.len(), 5, "{report:#?}");
}

#[test]
fn agent_absent_is_never_conflated_with_a_root_or_named_agent() {
    let (_dir, mut store) = store_with_pool(0);
    let cap = local(1_000);
    let SharedPoolChargeOutcome::Charged { reservation, .. } = charge(
        &mut store,
        cap,
        "s",
        5,
        "k",
        ExecutorAttribution::main_executor(Some(ChargeSource::TurnHold)),
    ) else {
        panic!()
    };
    assert_eq!(reservation.agent_id, None);
    assert_eq!(reservation.executor_kind, Some(ExecutorKind::AgentAbsent));
    assert_eq!(reservation.charge_source, Some(ChargeSource::TurnHold));
    // And it round-trips through a fresh read of the row.
    let read = store
        .get_shared_reservation(reservation.id)
        .unwrap()
        .unwrap();
    assert_eq!(read, *reservation);
}

#[test]
fn inconsistent_attribution_is_rejected_before_anything_is_written() {
    let (_dir, mut store) = store_with_pool(0);
    let bad = [
        ExecutorAttribution {
            agent_id: None,
            executor_kind: Some(ExecutorKind::Agent),
            charge_source: None,
        },
        ExecutorAttribution {
            agent_id: Some("a"),
            executor_kind: Some(ExecutorKind::AgentAbsent),
            charge_source: None,
        },
        ExecutorAttribution {
            agent_id: Some("a"),
            executor_kind: Some(ExecutorKind::Unattributed),
            charge_source: None,
        },
        ExecutorAttribution {
            agent_id: Some(""),
            executor_kind: None,
            charge_source: None,
        },
    ];
    for (i, attribution) in bad.into_iter().enumerate() {
        let r = store.charge_shared(SharedPoolChargeRequest {
            pool_id: &pool(),
            principal_id: &owner(),
            session_id: "s",
            amount: 1,
            unit: &QuotaUnit::UsdCents,
            idempotency_key: &format!("bad-{i}"),
            now: now(),
            attribution,
            capacity_source: local(10),
        });
        assert!(r.is_err(), "case {i} must be rejected");
        let r = store.reserve_shared_metered(MeteredReserveRequest {
            base: SharedPoolReserveRequest {
                pool_id: &pool(),
                principal_id: &owner(),
                session_id: "s",
                amount: 1,
                unit: &QuotaUnit::UsdCents,
                idempotency_key: &format!("badh-{i}"),
                now: now(),
                ttl_secs: 60,
                attribution,
            },
            session_cap: None,
            capacity_source: local(10),
        });
        assert!(r.is_err(), "hold case {i} must be rejected");
    }
    let report = store.shared_pool_executor_usage(&pool()).unwrap();
    assert_eq!((report.total_used, report.total_reserved), (0, 0));
    // A bare agent_id with no kind is read as Agent.
    let SharedPoolChargeOutcome::Charged { reservation, .. } = charge(
        &mut store,
        local(10),
        "s",
        1,
        "bare",
        ExecutorAttribution {
            agent_id: Some("a9"),
            executor_kind: None,
            charge_source: None,
        },
    ) else {
        panic!()
    };
    assert_eq!(reservation.executor_kind, Some(ExecutorKind::Agent));
}

#[test]
fn settle_attribution_fills_gaps_but_never_rewrites_hold_time_attribution() {
    let (_dir, mut store) = store_with_pool(0);
    let cap = local(1_000);

    // Unattributed hold: settle attributes it.
    let h1 = granted_id(reserve(&mut store, cap, "s", 10, "h1", Default::default()));
    let SharedPoolSettleOutcome::Settled { reservation, .. } = store
        .settle_shared_attributed(
            h1,
            Some(10),
            now(),
            ExecutorAttribution::agent("late", Some(ChargeSource::SubagentTranscript)),
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(reservation.agent_id.as_deref(), Some("late"));
    assert_eq!(reservation.executor_kind, Some(ExecutorKind::Agent));
    assert_eq!(
        reservation.charge_source,
        Some(ChargeSource::SubagentTranscript)
    );

    // Attributed hold: a conflicting settle-time attribution is ignored.
    let h2 = granted_id(reserve(
        &mut store,
        cap,
        "s",
        10,
        "h2",
        ExecutorAttribution::main_executor(Some(ChargeSource::TurnHold)),
    ));
    let SharedPoolSettleOutcome::Settled { reservation, .. } = store
        .settle_shared_attributed(
            h2,
            None,
            now(),
            ExecutorAttribution::agent("intruder", Some(ChargeSource::Gateway)),
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(reservation.agent_id, None);
    assert_eq!(reservation.executor_kind, Some(ExecutorKind::AgentAbsent));
    assert_eq!(reservation.charge_source, Some(ChargeSource::TurnHold));

    // The plain settle path is unchanged for legacy callers.
    let h3 = granted_id(reserve(&mut store, cap, "s", 10, "h3", Default::default()));
    let SharedPoolSettleOutcome::Settled { reservation, .. } =
        store.settle_shared(h3, Some(4), now()).unwrap()
    else {
        panic!()
    };
    assert_eq!(
        (
            reservation.agent_id,
            reservation.executor_kind,
            reservation.charge_source
        ),
        (None, None, None)
    );
}

// ---------------------------------------------------------------------
// Review cycle 1 regressions (HORO-2194 M1-M4 + LOW)
// ---------------------------------------------------------------------

fn ingest_disclosed(store: &mut LedgerStore, used: u64, limit: u64) {
    store
        .ingest_pool_provider_snapshot(
            &pool(),
            now(),
            Some(now() + time::Duration::seconds(3600)),
            &GaugeReading::Used {
                used: QuotaAmount::new(QuotaUnit::UsdCents, used),
                limit: Some(limit),
            },
            Confidence::High,
            now(),
        )
        .unwrap();
}

#[test]
fn local_configured_ignores_provider_snapshot_overhang_provider_snapshot_unchanged() {
    let (_dir, mut store) = store_with_pool(1_000);
    // Disclosed snapshot: 800 used externally, no matching Libra rows.
    ingest_disclosed(&mut store, 800, 1_000);

    let local_status = store
        .pool_admission_for(&pool(), now(), local(1_000))
        .unwrap()
        .unwrap();
    assert_eq!(local_status.admission.external_overhang, 0);
    assert_eq!(local_status.admission.remaining(), 1_000);

    let provider_status = store
        .pool_admission_for(&pool(), now(), CapacitySource::ProviderSnapshot)
        .unwrap()
        .unwrap();
    assert_eq!(provider_status.admission.external_overhang, 800);
    assert_eq!(provider_status.admission.remaining(), 200);

    // ProviderSnapshot admission still refuses what the overhang forbids.
    assert!(matches!(
        reserve(
            &mut store,
            CapacitySource::ProviderSnapshot,
            "s",
            201,
            "p",
            Default::default()
        ),
        MeteredReserveOutcome::Insufficient { .. }
    ));
    // LocalConfigured admits the full cap, and a charge reports no overhang.
    granted_id(reserve(
        &mut store,
        local(1_000),
        "s",
        1_000,
        "l",
        Default::default(),
    ));
    let SharedPoolChargeOutcome::Charged { admission, .. } =
        charge(&mut store, local(1_000), "s", 10, "c", Default::default())
    else {
        panic!()
    };
    assert_eq!(admission.external_overhang, 0);
}

#[test]
fn charge_replay_never_swallows_a_settled_hold_or_differently_attributed_charge() {
    let (_dir, mut store) = store_with_pool(0);
    let cap = local(1_000);
    let hold = granted_id(reserve(
        &mut store,
        cap,
        "s",
        10,
        "k",
        ExecutorAttribution::main_executor(Some(ChargeSource::TurnHold)),
    ));
    store.settle_shared(hold, Some(10), now()).unwrap();

    // Same key, amount and session as the settled turn hold: spend must
    // not be silently dropped as a "replay".
    for attribution in [
        ExecutorAttribution::agent("a1", Some(ChargeSource::SubagentTranscript)),
        ExecutorAttribution::main_executor(Some(ChargeSource::TurnHold)),
    ] {
        assert!(matches!(
            charge(&mut store, cap, "s", 10, "k", attribution),
            SharedPoolChargeOutcome::KeyConflict(_)
        ));
    }
    assert_eq!(
        store
            .shared_pool_executor_usage(&pool())
            .unwrap()
            .total_used,
        10
    );

    // A true replay is still AlreadyCharged; any attribution difference
    // (NULL is its own value) is a conflict.
    let attr = ExecutorAttribution::agent("a1", Some(ChargeSource::SubagentTranscript));
    assert!(matches!(
        charge(&mut store, cap, "s", 5, "t", attr),
        SharedPoolChargeOutcome::Charged { .. }
    ));
    assert!(matches!(
        charge(&mut store, cap, "s", 5, "t", attr),
        SharedPoolChargeOutcome::AlreadyCharged(_)
    ));
    for other in [
        ExecutorAttribution::agent("a2", Some(ChargeSource::SubagentTranscript)),
        ExecutorAttribution::agent("a1", Some(ChargeSource::Gateway)),
        ExecutorAttribution::agent("a1", None),
        ExecutorAttribution::unattributed(Some(ChargeSource::SubagentTranscript)),
        Default::default(),
    ] {
        assert!(matches!(
            charge(&mut store, cap, "s", 5, "t", other),
            SharedPoolChargeOutcome::KeyConflict(_)
        ));
    }
    assert_eq!(
        store
            .shared_pool_executor_usage(&pool())
            .unwrap()
            .total_used,
        15
    );
}

#[test]
fn reserve_with_a_charges_key_is_a_conflict_not_a_granted_replay() {
    let (_dir, mut store) = store_with_pool(0);
    let cap = local(1_000);
    charge(&mut store, cap, "s", 10, "c", Default::default());
    let r = store.reserve_shared_metered(MeteredReserveRequest {
        base: SharedPoolReserveRequest {
            pool_id: &pool(),
            principal_id: &owner(),
            session_id: "s",
            amount: 10,
            unit: &QuotaUnit::UsdCents,
            idempotency_key: "c",
            now: now(),
            ttl_secs: 60,
            attribution: Default::default(),
        },
        session_cap: None,
        capacity_source: cap,
    });
    assert!(matches!(
        r,
        Err(libra_governor_ledger::LedgerError::SharedPoolKeyConflict)
    ));
    // The plain launch-slot entry point refuses it the same way.
    let r = store.reserve_shared(SharedPoolReserveRequest {
        pool_id: &pool(),
        principal_id: &owner(),
        session_id: "s",
        amount: 10,
        unit: &QuotaUnit::UsdCents,
        idempotency_key: "c",
        now: now(),
        ttl_secs: 60,
        attribution: Default::default(),
    });
    assert!(matches!(
        r,
        Err(libra_governor_ledger::LedgerError::SharedPoolKeyConflict)
    ));

    // Replaying a genuine (even settled) hold key is unchanged.
    let hold = granted_id(reserve(&mut store, cap, "s", 5, "h", Default::default()));
    store.settle_shared(hold, Some(5), now()).unwrap();
    assert!(matches!(
        reserve(&mut store, cap, "s", 5, "h", Default::default()),
        MeteredReserveOutcome::AlreadyGranted(_)
    ));
}

#[test]
fn status_flags_capacity_unknown_when_the_provider_snapshot_is_unusable() {
    let (_dir, mut store) = store_with_pool(1_000);
    // No snapshot: ProviderSnapshot capacity is unknown, LocalConfigured known.
    let unknown = store
        .pool_admission_for(&pool(), now(), CapacitySource::ProviderSnapshot)
        .unwrap()
        .unwrap();
    assert!(!unknown.capacity_known);
    assert!(
        store
            .pool_admission_for(&pool(), now(), local(500))
            .unwrap()
            .unwrap()
            .capacity_known
    );
    let SharedPoolChargeOutcome::Charged { capacity_known, .. } = charge(
        &mut store,
        CapacitySource::ProviderSnapshot,
        "s",
        5,
        "c1",
        Default::default(),
    ) else {
        panic!()
    };
    assert!(
        !capacity_known,
        "stored capacity must not read as effective"
    );
    let SharedPoolChargeOutcome::Charged { capacity_known, .. } =
        charge(&mut store, local(500), "s", 5, "c2", Default::default())
    else {
        panic!()
    };
    assert!(capacity_known);

    // A usable snapshot makes it known again; admission behaviour is the
    // same as before (still fail-closed above).
    ingest_disclosed(&mut store, 0, 1_000);
    assert!(
        store
            .pool_admission_for(&pool(), now(), CapacitySource::ProviderSnapshot)
            .unwrap()
            .unwrap()
            .capacity_known
    );
}

#[test]
fn invalid_attribution_is_rejected_even_when_the_pool_is_full() {
    let (_dir, mut store) = store_with_pool(0);
    let cap = local(10);
    charge(&mut store, cap, "s", 10, "fill", Default::default());
    let bad = ExecutorAttribution {
        agent_id: None,
        executor_kind: Some(ExecutorKind::Agent),
        charge_source: None,
    };
    let r = store.reserve_shared_metered(MeteredReserveRequest {
        base: SharedPoolReserveRequest {
            pool_id: &pool(),
            principal_id: &owner(),
            session_id: "s",
            amount: 5,
            unit: &QuotaUnit::UsdCents,
            idempotency_key: "bad",
            now: now(),
            ttl_secs: 60,
            attribution: bad,
        },
        session_cap: None,
        capacity_source: cap,
    });
    assert!(r.is_err(), "must be an error, not Insufficient: {r:?}");
}
