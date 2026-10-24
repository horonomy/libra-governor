//! HORO-1727 S4: the configured lifetime scope reaches the persisted task
//! budget through a real `Preflight`.

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use libra_governor_daemon::{recon::ReconBudget, DaemonConfig};
use libra_governor_domain::LifetimeScope;
use libra_governor_domain::ReservationClass;
use libra_governor_ledger::{LedgerStore, ReserveOutcome, ReserveRequest};
use libra_governor_protocol::{wire, Request, RequestEnvelope, ResponseEnvelope, PROTOCOL_VERSION};

fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample_repo")
}

/// Runs one `Preflight` against a daemon configured with `scope` and
/// returns the `lifetime_bounded` value persisted for the new task.
fn stored_lifetime_bounded_after_one_preflight(scope: LifetimeScope) -> i64 {
    let dir = tempfile::tempdir().unwrap();
    let config = DaemonConfig {
        socket_path: dir.path().join("d.sock"),
        ledger_path: dir.path().join("ledger.sqlite3"),
        log_path: dir.path().join("daemon.log"),
        recon_budget: ReconBudget {
            max_duration: Duration::from_secs(3),
            ..ReconBudget::default()
        },
        replan_hysteresis: libra_governor_domain::ReplanHysteresisConfig::default(),
        policy: libra_governor_daemon::default_admission_policy(),
        lifetime_scope: scope,
        reservation_ttl_secs: 900,
        gateway: None,
        gateway_stats: std::sync::Arc::new(Default::default()),
        gateway_session_header: libra_governor_gateway::proxy::DEFAULT_SESSION_HEADER.to_string(),
        extensions: None,
        extension_runtime: std::sync::OnceLock::new(),
        outcome_authority: None,
        launch_gate: None,
        shared_budget: Default::default(),
        progressive_interval_secs: libra_governor_daemon::DEFAULT_PROGRESSIVE_INTERVAL_SECS,
    };

    let listener = libra_governor_daemon::bind_or_detect_running(&config.socket_path).unwrap();
    let ledger_path = config.ledger_path.clone();
    let socket_path = config.socket_path.clone();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut ledger = LedgerStore::open(&config.ledger_path).unwrap();
        let mut current_task = None;
        libra_governor_daemon::handle_connection(stream, &mut ledger, &mut current_task, &config)
            .unwrap();
    });

    let client = UnixStream::connect(&socket_path).unwrap();
    let request = RequestEnvelope {
        protocol_version: PROTOCOL_VERSION,
        request: Request::Preflight {
            task_hint: "fix the login bug".to_string(),
            cwd: fixture_repo(),
            session_id: "lifetime-scope-wiring".to_string(),
        },
    };
    wire::write_message(&client, &request).unwrap();
    let _: ResponseEnvelope =
        wire::read_message(BufReader::new(client.try_clone().unwrap())).unwrap();
    server.join().unwrap();

    let conn = rusqlite::Connection::open(ledger_path).unwrap();
    conn.query_row("SELECT lifetime_bounded FROM task_budgets", [], |row| {
        row.get(0)
    })
    .unwrap()
}

#[test]
fn a_lifetime_bounded_daemon_persists_a_bounded_budget() {
    assert_eq!(
        stored_lifetime_bounded_after_one_preflight(LifetimeScope::LifetimeBounded),
        1
    );
}

/// Runs two `Preflight`s of the same session against a daemon configured
/// with `scope`, settling `settled_chunks` x 60,000 tokens of spend in the
/// ledger between them, and returns the second preflight's admission.
fn second_preflight_admission(
    scope: LifetimeScope,
    settled_chunks: u32,
) -> libra_governor_domain::Admission {
    let dir = tempfile::tempdir().unwrap();
    let config = DaemonConfig {
        socket_path: dir.path().join("d.sock"),
        ledger_path: dir.path().join("ledger.sqlite3"),
        log_path: dir.path().join("daemon.log"),
        recon_budget: ReconBudget {
            max_duration: Duration::from_secs(3),
            ..ReconBudget::default()
        },
        replan_hysteresis: libra_governor_domain::ReplanHysteresisConfig::default(),
        policy: libra_governor_daemon::default_admission_policy(),
        lifetime_scope: scope,
        reservation_ttl_secs: 900,
        gateway: None,
        gateway_stats: std::sync::Arc::new(Default::default()),
        gateway_session_header: libra_governor_gateway::proxy::DEFAULT_SESSION_HEADER.to_string(),
        extensions: None,
        extension_runtime: std::sync::OnceLock::new(),
        outcome_authority: None,
        launch_gate: None,
        shared_budget: Default::default(),
        progressive_interval_secs: libra_governor_daemon::DEFAULT_PROGRESSIVE_INTERVAL_SECS,
    };
    let listener = libra_governor_daemon::bind_or_detect_running(&config.socket_path).unwrap();
    let ledger_path = config.ledger_path.clone();
    let socket_path = config.socket_path.clone();
    let server = std::thread::spawn(move || {
        for _ in 0..2 {
            let (stream, _) = listener.accept().unwrap();
            let mut ledger = LedgerStore::open(&config.ledger_path).unwrap();
            let mut current_task = None;
            libra_governor_daemon::handle_connection(
                stream,
                &mut ledger,
                &mut current_task,
                &config,
            )
            .unwrap();
        }
    });

    let preflight = || {
        let client = UnixStream::connect(&socket_path).unwrap();
        let request = RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request: Request::Preflight {
                task_hint: "fix the login bug".to_string(),
                cwd: fixture_repo(),
                session_id: "lifetime-scope-spend".to_string(),
            },
        };
        wire::write_message(&client, &request).unwrap();
        let response: ResponseEnvelope =
            wire::read_message(BufReader::new(client.try_clone().unwrap())).unwrap();
        response
    };

    let _ = preflight();

    let task_id: String = rusqlite::Connection::open(&ledger_path)
        .unwrap()
        .query_row("SELECT task_id FROM task_budgets", [], |row| row.get(0))
        .unwrap();
    let task_id = libra_governor_domain::TaskId(uuid::Uuid::parse_str(&task_id).unwrap());
    let mut ledger = LedgerStore::open(&ledger_path).unwrap();
    let now = time::OffsetDateTime::now_utc();
    for chunk in 0..settled_chunks {
        let key = format!("seed-{chunk}");
        let outcome = ledger
            .reserve(ReserveRequest {
                task_id,
                session_id: "lifetime-scope-spend",
                plan_id: None,
                class: ReservationClass::RequiredWork,
                amount: libra_governor_domain::ResourceAmount::Tokens(60_000),
                idempotency_key: &key,
                now,
                ttl_secs: 900,
            })
            .unwrap();
        match outcome {
            ReserveOutcome::Granted(reservation) => {
                ledger
                    .settle(
                        reservation.id,
                        Some(libra_governor_domain::ResourceAmount::Tokens(60_000)),
                        now,
                    )
                    .unwrap();
            }
            // Only a lifetime-bounded budget may refuse a later chunk.
            other => assert!(
                chunk > 0 && scope == LifetimeScope::LifetimeBounded,
                "seeding chunk {chunk} was not granted: {other:?}"
            ),
        }
    }
    let seeded = ledger
        .budget_snapshot(task_id)
        .unwrap()
        .unwrap()
        .used()
        .as_f64();
    assert!(
        seeded >= 60_000.0,
        "the seeded spend must be settled: {seeded}"
    );
    drop(ledger);

    let second = preflight();
    server.join().unwrap();
    match second.response {
        libra_governor_protocol::Response::Preflight(result) => {
            result.admission.expect("a budget was resolved").admission
        }
        other => panic!("expected a Preflight response, got {other:?}"),
    }
}

#[test]
fn a_turn_scoped_preflight_admits_past_the_lifetime_ceiling_a_bounded_one_denies() {
    use libra_governor_domain::Admission;
    assert_eq!(
        second_preflight_admission(LifetimeScope::TurnScoped, 2),
        Admission::Admit
    );
    // Negative control: the same spend under a lifetime-bounded daemon is
    // refused or escalated for approval (the projection lands past the
    // lifetime target), never a plain admit.
    assert_ne!(
        second_preflight_admission(LifetimeScope::LifetimeBounded, 2),
        Admission::Admit
    );
}
