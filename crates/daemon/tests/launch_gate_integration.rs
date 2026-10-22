//! Integration tests for the launch-gate admission primitive (HORO-1766).
//!
//! Most cases drive `libra_governor_daemon::launch_gate`'s handler
//! functions directly against a real on-disk `LedgerStore` — the same
//! "real socket for the parts that need it, direct calls for the parts
//! that don't" split `crates/ledger/src/shared_pool.rs`'s own
//! `concurrent_reservations_never_collectively_exceed_capacity_at_exact_limit`
//! test uses, because going through the daemon's serial accept loop
//! would prove nothing extra about the ledger's own transaction
//! atomicity (see that test's docs). One test below exercises the real
//! `dispatch`/`Request::Launch` wire path end to end over a Unix socket,
//! following `crates/daemon/tests/preflight_integration.rs`'s pattern,
//! to prove the IPC shape itself is wired correctly.

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::Path;

use libra_governor_daemon::launch_gate::{
    handle_admit, handle_release, handle_renew, handle_status, LaunchGateConfig, LaunchPoolConfig,
};
use libra_governor_ledger::LedgerStore;
use libra_governor_protocol::{
    wire, LaunchOutcome, LaunchRequest, Request, RequestEnvelope, ReservationState, Response,
    ResponseEnvelope, UnavailableReason, WaitReason, PROTOCOL_VERSION,
};
use time::OffsetDateTime;
use uuid::Uuid;

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()
}

fn one_pool_config(alias: &str, capacity: u64) -> LaunchGateConfig {
    LaunchGateConfig {
        pools: vec![LaunchPoolConfig {
            alias: alias.to_string(),
            capacity,
            enabled: true,
        }],
        hold_ttl_secs: 300,
    }
}

fn open_store(dir: &Path) -> LedgerStore {
    LedgerStore::open(dir.join("ledger.sqlite3")).unwrap()
}

#[test]
fn gate_disabled_reports_not_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open_store(dir.path());
    let outcome = handle_admit(&mut store, None, "agents", Uuid::new_v4(), now()).unwrap();
    assert_eq!(
        outcome,
        LaunchOutcome::Unavailable {
            reason: UnavailableReason::NotEnabled
        }
    );
}

#[test]
fn unknown_pool_alias_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open_store(dir.path());
    let config = one_pool_config("agents", 2);
    let outcome = handle_admit(
        &mut store,
        Some(&config),
        "not-a-real-alias",
        Uuid::new_v4(),
        now(),
    )
    .unwrap();
    assert_eq!(
        outcome,
        LaunchOutcome::Unavailable {
            reason: UnavailableReason::UnknownPool
        }
    );
}

#[test]
fn admin_disabled_pool_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open_store(dir.path());
    let config = LaunchGateConfig {
        pools: vec![LaunchPoolConfig {
            alias: "agents".to_string(),
            capacity: 2,
            enabled: false,
        }],
        hold_ttl_secs: 300,
    };
    let outcome = handle_admit(&mut store, Some(&config), "agents", Uuid::new_v4(), now()).unwrap();
    assert_eq!(
        outcome,
        LaunchOutcome::Unavailable {
            reason: UnavailableReason::AdminDisabled
        }
    );
}

/// Capacity 1: A admits -> Start; B admits -> Wait(Throttled); release A
/// -> B admits -> Start.
#[test]
fn capacity_one_pool_throttles_then_admits_after_release() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open_store(dir.path());
    let config = one_pool_config("agents", 1);

    let a = Uuid::new_v4();
    let a_outcome = handle_admit(&mut store, Some(&config), "agents", a, now()).unwrap();
    let a_reservation_id = match a_outcome {
        LaunchOutcome::Start { reservation_id, .. } => reservation_id,
        other => panic!("expected Start for A, got {other:?}"),
    };

    let b = Uuid::new_v4();
    let b_outcome = handle_admit(&mut store, Some(&config), "agents", b, now()).unwrap();
    match b_outcome {
        LaunchOutcome::Wait {
            reason: WaitReason::Throttled { active, capacity },
            ..
        } => {
            assert_eq!(active, 1);
            assert_eq!(capacity, 1);
        }
        other => panic!("expected Wait(Throttled) for B, got {other:?}"),
    }

    let release_outcome = handle_release(&mut store, a_reservation_id, a, now()).unwrap();
    assert_eq!(
        release_outcome,
        LaunchOutcome::Lost {
            state: ReservationState::Released
        }
    );

    let b_retry = handle_admit(&mut store, Some(&config), "agents", b, now()).unwrap();
    assert!(
        matches!(b_retry, LaunchOutcome::Start { .. }),
        "expected Start for B after A released, got {b_retry:?}"
    );
}

/// A retries `Admit` with the same `launch_id` after already being
/// granted -> returns the same reservation (idempotent), not a double
/// grant.
#[test]
fn retrying_admit_with_the_same_launch_id_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open_store(dir.path());
    let config = one_pool_config("agents", 1);
    let launch_id = Uuid::new_v4();

    let first = handle_admit(&mut store, Some(&config), "agents", launch_id, now()).unwrap();
    let first_id = match first {
        LaunchOutcome::Start { reservation_id, .. } => reservation_id,
        other => panic!("expected Start, got {other:?}"),
    };

    let second = handle_admit(&mut store, Some(&config), "agents", launch_id, now()).unwrap();
    match second {
        LaunchOutcome::Start { reservation_id, .. } => {
            assert_eq!(
                reservation_id, first_id,
                "replay must return the SAME reservation"
            )
        }
        other => panic!("expected Start (idempotent replay), got {other:?}"),
    }

    // And capacity was never double-consumed by the replay: a second,
    // genuinely distinct launch_id must still be throttled against this
    // capacity-1 pool.
    let other_launch = Uuid::new_v4();
    let other = handle_admit(&mut store, Some(&config), "agents", other_launch, now()).unwrap();
    assert!(
        matches!(other, LaunchOutcome::Wait { .. }),
        "replaying the first launch_id must not have freed or doubled capacity"
    );
}

/// THE critical trap this ticket exists to close: an `AlreadyGranted`
/// result whose underlying row is no longer active (expired) must map to
/// `Lost`, never `Start` — otherwise a retried lost-response after the
/// hold already expired produces an unguarded start that silently
/// oversubscribes the pool.
#[test]
fn retrying_admit_after_the_original_hold_expired_never_resolves_to_start() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open_store(dir.path());
    let config = LaunchGateConfig {
        pools: vec![LaunchPoolConfig {
            alias: "agents".to_string(),
            capacity: 1,
            enabled: true,
        }],
        hold_ttl_secs: 60,
    };
    let launch_id = Uuid::new_v4();

    let first = handle_admit(&mut store, Some(&config), "agents", launch_id, now()).unwrap();
    assert!(matches!(first, LaunchOutcome::Start { .. }));

    // Time passes well beyond the 60s hold TTL, then the caller retries
    // the exact same launch_id (as if the original Start response was
    // lost in transit and the caller is safely re-discovering what
    // happened).
    let much_later = now() + time::Duration::seconds(3600);
    let retry = handle_admit(&mut store, Some(&config), "agents", launch_id, much_later).unwrap();
    match retry {
        LaunchOutcome::Lost { state } => {
            assert_eq!(state, ReservationState::Expired);
        }
        other => panic!(
            "CRITICAL: expected Lost (never Start) for a replay of an expired hold, got {other:?}"
        ),
    }

    // And the now-reclaimed capacity is genuinely available to a FRESH
    // launch_id — proving the expiry sweep actually ran, not just that
    // the stale row was reported honestly.
    let fresh_launch = Uuid::new_v4();
    let fresh = handle_admit(
        &mut store,
        Some(&config),
        "agents",
        fresh_launch,
        much_later,
    )
    .unwrap();
    assert!(
        matches!(fresh, LaunchOutcome::Start { .. }),
        "expected a fresh launch_id to be admitted once the stale hold's capacity is reclaimed, \
         got {fresh:?}"
    );
}

/// The same trap, but via an explicit `Release` instead of a time-based
/// expiry: replaying `launch_id` after the original hold was released
/// must also never resolve to `Start`.
#[test]
fn retrying_admit_after_the_original_hold_was_released_never_resolves_to_start() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open_store(dir.path());
    let config = one_pool_config("agents", 1);
    let launch_id = Uuid::new_v4();

    let first = handle_admit(&mut store, Some(&config), "agents", launch_id, now()).unwrap();
    let reservation_id = match first {
        LaunchOutcome::Start { reservation_id, .. } => reservation_id,
        other => panic!("expected Start, got {other:?}"),
    };
    handle_release(&mut store, reservation_id, launch_id, now()).unwrap();

    let retry = handle_admit(&mut store, Some(&config), "agents", launch_id, now()).unwrap();
    match retry {
        LaunchOutcome::Lost { state } => assert_eq!(state, ReservationState::Released),
        other => panic!(
            "CRITICAL: expected Lost (never Start) for a replay of a released hold, got {other:?}"
        ),
    }
}

/// A `Release` with a mismatched `launch_id`/session is refused; the
/// slot stays held.
#[test]
fn release_with_mismatched_launch_id_is_refused_and_the_slot_stays_held() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open_store(dir.path());
    let config = one_pool_config("agents", 1);
    let real_launch_id = Uuid::new_v4();

    let outcome = handle_admit(&mut store, Some(&config), "agents", real_launch_id, now()).unwrap();
    let reservation_id = match outcome {
        LaunchOutcome::Start { reservation_id, .. } => reservation_id,
        other => panic!("expected Start, got {other:?}"),
    };

    let impostor_launch_id = Uuid::new_v4();
    let release_outcome =
        handle_release(&mut store, reservation_id, impostor_launch_id, now()).unwrap();
    assert_eq!(
        release_outcome,
        LaunchOutcome::Unavailable {
            reason: UnavailableReason::ReleaseRefused
        }
    );

    // The slot really does stay held: a second launch cannot admit
    // against this capacity-1 pool.
    let other = handle_admit(&mut store, Some(&config), "agents", Uuid::new_v4(), now()).unwrap();
    assert!(
        matches!(other, LaunchOutcome::Wait { .. }),
        "the mismatched release must not have freed the slot, got {other:?}"
    );
}

#[test]
fn renew_extends_the_hold_and_a_mismatched_renew_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open_store(dir.path());
    let config = one_pool_config("agents", 1);
    let launch_id = Uuid::new_v4();

    let outcome = handle_admit(&mut store, Some(&config), "agents", launch_id, now()).unwrap();
    let (reservation_id, original_expiry) = match outcome {
        LaunchOutcome::Start {
            reservation_id,
            expires_at,
            ..
        } => (reservation_id, expires_at),
        other => panic!("expected Start, got {other:?}"),
    };

    let later = now() + time::Duration::seconds(60);
    let renewed =
        handle_renew(&mut store, Some(&config), reservation_id, launch_id, later).unwrap();
    match renewed {
        LaunchOutcome::Renewed { expires_at } => {
            assert_ne!(expires_at, original_expiry);
        }
        other => panic!("expected Renewed, got {other:?}"),
    }

    let impostor = Uuid::new_v4();
    let refused = handle_renew(&mut store, Some(&config), reservation_id, impostor, later).unwrap();
    assert_eq!(
        refused,
        LaunchOutcome::Unavailable {
            reason: UnavailableReason::RenewFailed
        }
    );
}

/// N concurrent threads admitting against capacity k -> granted count
/// never exceeds k, and at every sampled point active <= capacity.
/// Real OS threads, each opening its own `LedgerStore` against the same
/// on-disk file — the ledger's own documented concurrency model
/// (`BEGIN IMMEDIATE` serializing writers at the SQLite layer), not
/// anything this module adds.
#[test]
fn concurrent_admits_never_collectively_exceed_capacity() {
    let dir = tempfile::tempdir().unwrap();
    let ledger_path = dir.path().join("ledger.sqlite3");
    let capacity = 3u64;
    let config = one_pool_config("agents", capacity);

    // Applies migrations (creates the on-disk schema) single-threaded
    // first — mirroring `shared_pool.rs`'s own
    // `concurrent_reservations_never_collectively_exceed_capacity_at_exact_limit`
    // test precedent. Migration application is not itself safe against
    // many independent connections all racing to apply it for the first
    // time against a brand-new file; every concurrency test in this repo
    // pre-applies it (by opening the store once) before spawning threads,
    // rather than that being special to this test.
    drop(LedgerStore::open(&ledger_path).unwrap());

    let thread_count = 15usize;
    let mut handles = Vec::with_capacity(thread_count);
    for _ in 0..thread_count {
        let ledger_path = ledger_path.clone();
        let config = config.clone();
        handles.push(std::thread::spawn(move || {
            let mut store = LedgerStore::open(&ledger_path).unwrap();
            let launch_id = Uuid::new_v4();
            let outcome =
                handle_admit(&mut store, Some(&config), "agents", launch_id, now()).unwrap();
            matches!(outcome, LaunchOutcome::Start { .. })
        }));
    }

    let granted_count = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .filter(|granted| *granted)
        .count();
    assert_eq!(
        granted_count as u64, capacity,
        "exactly `capacity` of the concurrent admits must be granted"
    );

    let store = LedgerStore::open(&ledger_path).unwrap();
    let status = handle_status(&store, Some(&config), now()).unwrap();
    match status {
        LaunchOutcome::StatusReport { pools } => {
            assert_eq!(pools.len(), 1);
            assert!(pools[0].active <= pools[0].effective_capacity);
            assert_eq!(pools[0].active, capacity);
        }
        other => panic!("expected StatusReport, got {other:?}"),
    }
}

#[test]
fn status_report_shows_configured_vs_effective_capacity_divergence() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open_store(dir.path());
    let original = one_pool_config("agents", 2);
    // First Admit creates the pool at capacity 2.
    handle_admit(&mut store, Some(&original), "agents", Uuid::new_v4(), now()).unwrap();

    // Config is edited to claim capacity 9, but `ensure_pool` never
    // overwrites an existing pool's capacity — `effective_capacity` must
    // keep reporting the real, original 2.
    let edited = one_pool_config("agents", 9);
    let status = handle_status(&store, Some(&edited), now()).unwrap();
    match status {
        LaunchOutcome::StatusReport { pools } => {
            assert_eq!(pools[0].configured_capacity, 9);
            assert_eq!(pools[0].effective_capacity, 2);
        }
        other => panic!("expected StatusReport, got {other:?}"),
    }
}

#[test]
fn status_is_read_only_and_does_not_create_a_pool_that_was_never_admitted_against() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(dir.path());
    let config = one_pool_config("agents", 2);
    let status = handle_status(&store, Some(&config), now()).unwrap();
    match status {
        LaunchOutcome::StatusReport { pools } => {
            assert_eq!(pools[0].configured_capacity, 2);
            assert_eq!(pools[0].effective_capacity, 0);
            assert_eq!(pools[0].active, 0);
        }
        other => panic!("expected StatusReport, got {other:?}"),
    }
}

#[test]
fn status_report_carries_the_fixed_enforcement_scope() {
    let dir = tempfile::tempdir().unwrap();
    let store = open_store(dir.path());
    let config = one_pool_config("agents", 2);
    let status = handle_status(&store, Some(&config), now()).unwrap();
    match status {
        LaunchOutcome::StatusReport { pools } => {
            assert_eq!(
                pools[0].enforcement_scope,
                libra_governor_protocol::LAUNCH_GATE_ENFORCEMENT_SCOPE
            );
        }
        other => panic!("expected StatusReport, got {other:?}"),
    }
}

/// End-to-end over the real Unix socket and `dispatch` path, mirroring
/// `preflight_integration.rs`'s harness pattern — proves the `Request::Launch`/
/// `Response::Launch` wire shape round-trips through the real daemon
/// dispatch, not just through the handler functions called directly.
#[test]
fn launch_admit_round_trips_over_the_real_socket() {
    let dir = tempfile::tempdir().unwrap();
    let config = libra_governor_daemon::DaemonConfig {
        lifetime_scope: libra_governor_domain::LifetimeScope::LifetimeBounded,
        socket_path: dir.path().join("d.sock"),
        ledger_path: dir.path().join("ledger.sqlite3"),
        log_path: dir.path().join("daemon.log"),
        recon_budget: libra_governor_daemon::recon::ReconBudget::default(),
        replan_hysteresis: libra_governor_domain::ReplanHysteresisConfig::default(),
        policy: libra_governor_daemon::default_admission_policy(),
        reservation_ttl_secs: 900,
        gateway: None,
        gateway_stats: std::sync::Arc::new(Default::default()),
        gateway_session_header: libra_governor_gateway::proxy::DEFAULT_SESSION_HEADER.to_string(),
        extensions: None,
        extension_runtime: std::sync::OnceLock::new(),
        outcome_authority: None,
        progressive_interval_secs: libra_governor_daemon::DEFAULT_PROGRESSIVE_INTERVAL_SECS,
        launch_gate: Some(one_pool_config("agents", 1)),
    };

    let listener = libra_governor_daemon::bind_or_detect_running(&config.socket_path).unwrap();
    let server_thread = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut ledger = LedgerStore::open(&config.ledger_path).unwrap();
        let mut current_task = None;
        libra_governor_daemon::handle_connection(stream, &mut ledger, &mut current_task, &config)
            .unwrap();
    });

    let socket_path = dir.path().join("d.sock");
    let client = UnixStream::connect(&socket_path).unwrap();
    let request = RequestEnvelope {
        protocol_version: PROTOCOL_VERSION,
        request: Request::Launch {
            launch: LaunchRequest::Admit {
                pool_alias: "agents".to_string(),
                launch_id: Uuid::new_v4(),
            },
        },
    };
    wire::write_message(&client, &request).unwrap();
    let response: ResponseEnvelope =
        wire::read_message(BufReader::new(client.try_clone().unwrap())).unwrap();
    server_thread.join().unwrap();

    assert_eq!(response.protocol_version, PROTOCOL_VERSION);
    match response.response {
        Response::Launch(outcome) => {
            assert!(
                matches!(*outcome, LaunchOutcome::Start { .. }),
                "expected Start, got {outcome:?}"
            );
        }
        other => panic!("expected a Launch response, got {other:?}"),
    }
}

/// Confirms the zero-live-wiring static guard for `pacing` still passes
/// unmodified — this ticket must not reference `pacing` anywhere in
/// `daemon/src`, `gateway/src`, or `ledger/src` (HORO-1727 has not
/// settled the live-admission architecture question). This test just
/// documents the dependency; the guard itself lives in
/// `crates/domain/tests/pacing_not_wired_live.rs` and is run directly in
/// CI/validation as `cargo test -p libra-governor-domain --test
/// pacing_not_wired_live`.
#[test]
fn launch_gate_module_does_not_reference_pacing() {
    let root = repo_root();
    for crate_name in ["daemon", "gateway", "ledger"] {
        let src = root.join("crates").join(crate_name).join("src");
        let mut stack = vec![src];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                    let contents = std::fs::read_to_string(&path).unwrap_or_default();
                    assert!(
                        !contents.contains("pacing"),
                        "{crate_name}'s {path:?} must not reference `pacing` (HORO-1727 not yet \
                         settled)"
                    );
                }
            }
        }
    }
}

fn repo_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/daemon has a parent (crates/)")
        .parent()
        .expect("crates/ has a parent (the repo root)")
        .to_path_buf()
}
