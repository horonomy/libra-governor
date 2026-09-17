//! Spins up the daemon on a real Unix socket in a tempdir and drives one
//! full `Preflight` request against the synthetic fixture repo in
//! `tests/fixtures/sample_repo`, asserting a sane [`PreflightResult`]
//! comes back within the reconnaissance time budget.
//!
//! Uses only `libra-governor-daemon`'s public API
//! ([`libra_governor_daemon::handle_connection`] is the same dispatch
//! function `daemon run`'s accept loop calls), so this exercises the
//! real server code path end to end over a real socket.

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use libra_governor_daemon::{recon::ReconBudget, DaemonConfig};
use libra_governor_ledger::LedgerStore;
use libra_governor_protocol::{
    wire, Confidence, Request, RequestEnvelope, Response, ResponseEnvelope, PROTOCOL_VERSION,
};

fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample_repo")
}

#[test]
fn preflight_returns_sane_result_within_recon_budget() {
    let dir = tempfile::tempdir().unwrap();
    let recon_budget = ReconBudget {
        max_duration: Duration::from_secs(3),
        ..ReconBudget::default()
    };
    let config = DaemonConfig {
        socket_path: dir.path().join("d.sock"),
        ledger_path: dir.path().join("ledger.sqlite3"),
        log_path: dir.path().join("daemon.log"),
        recon_budget,
    };

    let listener = libra_governor_daemon::bind_or_detect_running(&config.socket_path).unwrap();
    let server_thread = std::thread::spawn(move || {
        // Serve exactly one connection with the real dispatch logic,
        // then return — this test only needs one round trip.
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
        request: Request::Preflight {
            task_hint: "fix the login bug".to_string(),
            cwd: fixture_repo(),
            session_id: "integration-test-session".to_string(),
        },
    };

    let start = Instant::now();
    wire::write_message(&client, &request).unwrap();
    let response: ResponseEnvelope =
        wire::read_message(BufReader::new(client.try_clone().unwrap())).unwrap();
    let elapsed = start.elapsed();

    server_thread.join().unwrap();

    assert!(
        elapsed < Duration::from_secs(5),
        "preflight took {elapsed:?}, expected well within the recon budget"
    );
    assert_eq!(response.protocol_version, PROTOCOL_VERSION);

    match response.response {
        Response::Preflight(result) => {
            assert_eq!(result.contract_draft.revision, 1);
            assert!(
                !result.contract_draft.criteria.is_empty(),
                "contract draft must always carry at least the baseline criterion"
            );
            assert!(
                result.contract_draft.criteria[0].required,
                "the baseline 'matches user request' criterion must be required"
            );
            assert!(
                result
                    .recon_summary
                    .detected_test_commands
                    .contains(&"cargo test".to_string()),
                "fixture repo has a Cargo.toml; recon must detect cargo test"
            );
            assert!(
                result
                    .recon_summary
                    .likely_affected_paths
                    .iter()
                    .any(|p| p.contains("login")),
                "prompt mentions 'login'; recon must match src/login.rs"
            );
            assert_eq!(result.confidence, Confidence::High);
            assert!(result.recon_cost_seconds >= 0.0);
            assert!(result.recon_cost_seconds < 5.0);
            assert!(
                result.estimate.is_none(),
                "HORO-1126 estimator math is out of scope here"
            );
        }
        other => panic!("expected a Preflight response, got {other:?}"),
    }
}

#[test]
fn second_preflight_for_same_session_supersedes_the_first() {
    let dir = tempfile::tempdir().unwrap();
    let config = DaemonConfig {
        socket_path: dir.path().join("d.sock"),
        ledger_path: dir.path().join("ledger.sqlite3"),
        log_path: dir.path().join("daemon.log"),
        recon_budget: ReconBudget::default(),
    };

    let mut ledger = LedgerStore::open(&config.ledger_path).unwrap();
    let mut current_task = None;

    let listener = libra_governor_daemon::bind_or_detect_running(&config.socket_path).unwrap();
    let socket_path = dir.path().join("d.sock");

    let mut send = |cwd: PathBuf| -> Response {
        let client_thread_socket = socket_path.clone();
        let client = std::thread::spawn(move || {
            let client = UnixStream::connect(&client_thread_socket).unwrap();
            let request = RequestEnvelope {
                protocol_version: PROTOCOL_VERSION,
                request: Request::Preflight {
                    task_hint: "fix the login bug".to_string(),
                    cwd,
                    session_id: "repeat-session".to_string(),
                },
            };
            wire::write_message(&client, &request).unwrap();
            let response: ResponseEnvelope =
                wire::read_message(BufReader::new(client.try_clone().unwrap())).unwrap();
            response.response
        });
        let (stream, _) = listener.accept().unwrap();
        libra_governor_daemon::handle_connection(stream, &mut ledger, &mut current_task, &config)
            .unwrap();
        client.join().unwrap()
    };

    let first = send(fixture_repo());
    let second = send(fixture_repo());

    let (first_task, second_task) = match (first, second) {
        (Response::Preflight(a), Response::Preflight(b)) => (a, b),
        other => panic!("expected two Preflight responses, got {other:?}"),
    };

    assert_eq!(
        first_task.task_id, second_task.task_id,
        "same session must resolve to the same task"
    );
    assert_eq!(
        second_task.contract_draft.revision, 2,
        "second preflight for the same session must produce contract revision 2"
    );
}
