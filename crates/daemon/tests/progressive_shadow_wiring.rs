//! HORO-1669: the progressive-estimate cadence gate and shadow-decision
//! recording must be purely additive. This file's single test is the
//! most important correctness check in the whole wiring — see
//! `handle_tool_invoked`'s module comment in `crates/daemon/src/server.rs`
//! and `docs/adr/0011-progressive-remaining-estimate-and-shadow-decisions.md`.

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use libra_governor_daemon::{recon::ReconBudget, DaemonConfig};
use libra_governor_domain::{
    CompletionContract, ExecutionOutcome, ExecutionPlan, ExecutionReceipt, PersistedPins, Policy,
    ReplanHysteresisConfig, RuntimeDecision, TaskIdentity, REPLAY_PINS_SCHEMA_VERSION,
};
use libra_governor_ledger::LedgerStore;
use libra_governor_protocol::{
    wire, Request, RequestEnvelope, Response, ResponseEnvelope, PROTOCOL_VERSION,
};

fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample_repo")
}

/// Mirrors `replan_integration.rs`'s `seed_same_repo_history` exactly —
/// five same-repo receipts with a bucket-typical tool-call count of 3, so
/// the material-deviation threshold (`> 2*3 = 6`) is comfortably above
/// the single tool call this test actually sends.
fn seed_same_repo_history(ledger: &mut LedgerStore) {
    use libra_governor_daemon::{features::derive_task_features, recon::run_recon};

    let now = time::OffsetDateTime::now_utc();
    let recon = run_recon(
        &fixture_repo(),
        "fix the login bug",
        &ReconBudget::default(),
    );
    let features = derive_task_features(&recon, "fix the login bug", &fixture_repo(), None);
    let seeded_tool_call_counts = [2u64, 2, 3, 3, 4];

    for (i, n) in (1..=5u64).enumerate() {
        let identity = TaskIdentity::new(None);
        ledger.insert_task(&identity, now).unwrap();
        ledger
            .insert_contract(identity.id, &CompletionContract::first(vec![]), now)
            .unwrap();
        let plan = ExecutionPlan::new(identity.id, 1, None, now)
            .with_task_features(Some(features.clone()));
        ledger.insert_plan(&plan).unwrap();
        let receipt = ExecutionReceipt::new(
            identity.id,
            1,
            plan.id,
            n * 60,
            vec![],
            ExecutionOutcome::Unknown,
            now,
        )
        .with_task_features(Some(features.clone()))
        .with_tool_call_count(seeded_tool_call_counts[i]);
        ledger.insert_receipt(&receipt).unwrap();
    }
}

fn send(
    listener: &std::os::unix::net::UnixListener,
    socket_path: &Path,
    ledger: &mut LedgerStore,
    current_task: &mut Option<libra_governor_protocol::TaskSummary>,
    config: &DaemonConfig,
    request: Request,
) -> Response {
    let socket_path = socket_path.to_path_buf();
    let client = std::thread::spawn(move || {
        let client = UnixStream::connect(&socket_path).unwrap();
        let envelope = RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request,
        };
        wire::write_message(&client, &envelope).unwrap();
        let response: ResponseEnvelope =
            wire::read_message(BufReader::new(client.try_clone().unwrap())).unwrap();
        response.response
    });
    let (stream, _) = listener.accept().unwrap();
    libra_governor_daemon::handle_connection(stream, ledger, current_task, config).unwrap();
    client.join().unwrap()
}

/// A canonical, order-independent dump of every row in `table` as a
/// sorted vector of pipe-joined column strings — good enough to detect
/// any row added, removed, or changed, without depending on column order
/// matching between two separate connections' schema introspection.
fn dump_table(conn: &rusqlite::Connection, table: &str) -> Vec<String> {
    let mut stmt = conn.prepare(&format!("SELECT * FROM {table}")).unwrap();
    let column_count = stmt.column_count();
    let mut rows = stmt
        .query_map([], |row| {
            let mut cells = Vec::with_capacity(column_count);
            for i in 0..column_count {
                let value: rusqlite::types::Value = row.get(i)?;
                cells.push(format!("{value:?}"));
            }
            Ok(cells.join("|"))
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect::<Vec<_>>();
    rows.sort();
    rows
}

fn snapshot(ledger_path: &Path) -> (Vec<String>, Vec<String>, Vec<String>) {
    let conn = rusqlite::Connection::open(ledger_path).unwrap();
    (
        dump_table(&conn, "task_budgets"),
        dump_table(&conn, "reservations"),
        dump_table(&conn, "resource_accounts"),
    )
}

fn shadow_decision_count(ledger_path: &Path) -> i64 {
    let conn = rusqlite::Connection::open(ledger_path).unwrap();
    conn.query_row("SELECT COUNT(*) FROM shadow_runtime_decisions", [], |r| {
        r.get(0)
    })
    .unwrap()
}

/// `(policy_json, pins_json, decision_json)` of the most recently
/// recorded shadow decision (HORO-1670) — `policy_json`/`pins_json` must
/// be present (non-NULL) once the daemon's `record_shadow_decision` call
/// site passes them.
fn shadow_decision_policy_and_pins(ledger_path: &Path) -> (Option<String>, Option<String>, String) {
    let conn = rusqlite::Connection::open(ledger_path).unwrap();
    conn.query_row(
        "SELECT policy_json, pins_json, decision_json FROM shadow_runtime_decisions \
         ORDER BY decided_at DESC LIMIT 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .unwrap()
}

/// The cadence gate firing alone (no loop/count material-event signal)
/// must record exactly one `shadow_runtime_decisions` row and leave
/// every mutating ledger table — `task_budgets`, `reservations`,
/// `resource_accounts` — byte-identical. This is the structural proof
/// that the shadow computation is purely additive: it answers "what
/// would Libra have recommended," it never acts.
#[test]
fn cadence_only_shadow_decision_never_mutates_existing_ledger_state() {
    let dir = tempfile::tempdir().unwrap();
    let config = DaemonConfig {
        socket_path: dir.path().join("d.sock"),
        ledger_path: dir.path().join("ledger.sqlite3"),
        log_path: dir.path().join("daemon.log"),
        recon_budget: ReconBudget::default(),
        replan_hysteresis: ReplanHysteresisConfig::default(),
        policy: libra_governor_daemon::default_admission_policy(),
        reservation_ttl_secs: 900,
        gateway: None,
        gateway_stats: std::sync::Arc::new(Default::default()),
        gateway_session_header: libra_governor_gateway::proxy::DEFAULT_SESSION_HEADER.to_string(),
        extensions: None,
        extension_runtime: std::sync::OnceLock::new(),
        // Always due: the sole purpose of this test is to isolate the
        // cadence gate from the material-event gate, so it must fire on
        // the very first ToolInvoked call.
        progressive_interval_secs: 0,
    };

    let mut ledger = LedgerStore::open(&config.ledger_path).unwrap();
    seed_same_repo_history(&mut ledger);

    let mut current_task = None;
    let listener = libra_governor_daemon::bind_or_detect_running(&config.socket_path).unwrap();
    let socket_path = dir.path().join("d.sock");

    match send(
        &listener,
        &socket_path,
        &mut ledger,
        &mut current_task,
        &config,
        Request::Preflight {
            task_hint: "fix the login bug".to_string(),
            cwd: fixture_repo(),
            session_id: "shadow-session".to_string(),
        },
    ) {
        Response::Preflight(_) => {}
        other => panic!("expected a Preflight response, got {other:?}"),
    }

    assert_eq!(
        shadow_decision_count(&config.ledger_path),
        0,
        "no ToolInvoked has been sent yet"
    );
    let before = snapshot(&config.ledger_path);

    // A single tool call: `same_tool_streak` is 1 (far below the loop
    // threshold of 4), and `tool_calls_since_last_replan` is 1 (far below
    // the material-deviation threshold of 6). Neither material-event
    // gate can fire. Only the cadence gate (interval 0) can.
    match send(
        &listener,
        &socket_path,
        &mut ledger,
        &mut current_task,
        &config,
        Request::ToolInvoked {
            session_id: "shadow-session".to_string(),
            tool_name: "Read".to_string(),
        },
    ) {
        Response::Ack => {}
        other => panic!("expected Ack, got {other:?}"),
    }

    let after = snapshot(&config.ledger_path);
    assert_eq!(
        before, after,
        "task_budgets/reservations/resource_accounts must be byte-identical: \
         a shadow decision must never mutate real ledger state"
    );
    assert_eq!(
        shadow_decision_count(&config.ledger_path),
        1,
        "the cadence gate must have recorded exactly one shadow decision"
    );

    let (policy_json, pins_json, decision_json) =
        shadow_decision_policy_and_pins(&config.ledger_path);
    let policy_json = policy_json
        .expect("HORO-1670: the recorded policy must be persisted alongside the shadow decision");
    let pins_json = pins_json.expect(
        "HORO-1670: the replay version pins must be persisted alongside the shadow decision",
    );

    // End-to-end proof, not just presence: a real daemon-written row must
    // decode cleanly through the exact path the HORO-1670 replay harness
    // uses (`crates/daemon/examples/v003_replay.rs`) — `decision_json`
    // straight into `RuntimeDecision` (relying on `Shadow<T>`'s
    // `#[serde(transparent)]` serialization, never on giving `Shadow`
    // itself a `Deserialize` impl), `policy_json` into `Policy`, and
    // `pins_json` into `PersistedPins`.
    let _: RuntimeDecision =
        serde_json::from_str(&decision_json).expect("decision_json must decode as RuntimeDecision");
    let _: Policy = serde_json::from_str(&policy_json).expect("policy_json must decode as Policy");
    let pins: PersistedPins =
        serde_json::from_str(&pins_json).expect("pins_json must decode as PersistedPins");
    assert_eq!(pins.schema_version, REPLAY_PINS_SCHEMA_VERSION);
}
