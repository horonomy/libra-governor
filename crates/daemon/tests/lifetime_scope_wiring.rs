//! HORO-1727 S4: the configured lifetime scope reaches the persisted task
//! budget through a real `Preflight`.

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use libra_governor_daemon::{recon::ReconBudget, DaemonConfig};
use libra_governor_domain::LifetimeScope;
use libra_governor_ledger::LedgerStore;
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
