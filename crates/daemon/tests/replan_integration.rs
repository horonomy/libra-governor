//! End-to-end HORO-1139 checks: drives real `Preflight`/`ToolInvoked`/
//! `Status` requests over a real socket against the real dispatch logic
//! ([`libra_governor_daemon::handle_connection`], the same function
//! `daemon run`'s accept loop calls — see `preflight_integration.rs` for
//! the established pattern this file follows), asserting that material
//! runtime evidence produces a real, persisted, hysteresis-gated replan
//! and that the resulting statusline-visible `TaskSummary` reflects it.

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use libra_governor_daemon::{recon::ReconBudget, DaemonConfig};
use libra_governor_domain::{
    CompletionContract, ExecutionOutcome, ExecutionPlan, ExecutionReceipt, ReplanHysteresisConfig,
    ReplanTriggerKind, TaskIdentity,
};
use libra_governor_ledger::LedgerStore;
use libra_governor_protocol::{
    wire, ReplanState, Request, RequestEnvelope, Response, ResponseEnvelope, PROTOCOL_VERSION,
};

fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample_repo")
}

/// Seeds `MIN_CLASS_SAMPLES` (5) same-repo receipts against the real
/// fixture repo's derived `TaskFeatures`, so a `Preflight` against that
/// same repo lands on the `Repo` bucket tier with a real (non-cold-start)
/// estimate — needed so a later replan's widening is observable against
/// real P90 numbers, not cold-start `None`s.
fn seed_same_repo_history(ledger: &mut LedgerStore) {
    use libra_governor_daemon::{features::derive_task_features, recon::run_recon};

    let now = time::OffsetDateTime::now_utc();
    let recon = run_recon(
        &fixture_repo(),
        "fix the login bug",
        &ReconBudget::default(),
    );
    let features = derive_task_features(&recon, "fix the login bug", &fixture_repo(), None);

    for n in 1..=5u64 {
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
        .with_task_features(Some(features.clone()));
        ledger.insert_receipt(&receipt).unwrap();
    }
}

/// Drives one full request/response round trip against a live, already
/// listening daemon: spawns a client thread that writes `request` and
/// reads back a response, while the current thread runs the one real
/// server-side `handle_connection` call it will produce.
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

fn status_task_summary(
    listener: &std::os::unix::net::UnixListener,
    socket_path: &Path,
    ledger: &mut LedgerStore,
    current_task: &mut Option<libra_governor_protocol::TaskSummary>,
    config: &DaemonConfig,
) -> libra_governor_protocol::TaskSummary {
    match send(
        listener,
        socket_path,
        ledger,
        current_task,
        config,
        Request::Status,
    ) {
        Response::Status(status) => status
            .current_task
            .expect("a task was preflighted; Status must reflect it"),
        other => panic!("expected a Status response, got {other:?}"),
    }
}

#[test]
fn tool_call_count_material_deviation_triggers_a_replan_visible_in_status() {
    let dir = tempfile::tempdir().unwrap();
    let config = DaemonConfig {
        socket_path: dir.path().join("d.sock"),
        ledger_path: dir.path().join("ledger.sqlite3"),
        log_path: dir.path().join("daemon.log"),
        recon_budget: ReconBudget::default(),
        replan_hysteresis: ReplanHysteresisConfig::default(),
    };

    let mut ledger = LedgerStore::open(&config.ledger_path).unwrap();
    seed_same_repo_history(&mut ledger);

    let mut current_task = None;
    let listener = libra_governor_daemon::bind_or_detect_running(&config.socket_path).unwrap();
    let socket_path = dir.path().join("d.sock");

    let preflight = match send(
        &listener,
        &socket_path,
        &mut ledger,
        &mut current_task,
        &config,
        Request::Preflight {
            task_hint: "fix the login bug".to_string(),
            cwd: fixture_repo(),
            session_id: "replan-session".to_string(),
        },
    ) {
        Response::Preflight(result) => *result,
        other => panic!("expected a Preflight response, got {other:?}"),
    };
    let original_estimate = preflight
        .estimate
        .clone()
        .expect("Repo-tier history was seeded; estimate must be real, not cold-start");
    assert!(!original_estimate.cold_start);
    let original_plan_id = preflight.plan_id;

    // 21 tool calls, cycling through four distinct tool names so no
    // same-tool streak ever reaches the loop threshold (4) -- this
    // isolates the tool-call-count material-deviation signal.
    let tools = ["Bash", "Read", "Grep", "Write"];
    for i in 0..21u64 {
        let response = send(
            &listener,
            &socket_path,
            &mut ledger,
            &mut current_task,
            &config,
            Request::ToolInvoked {
                session_id: "replan-session".to_string(),
                tool_name: tools[(i as usize) % tools.len()].to_string(),
            },
        );
        assert_eq!(response, Response::Ack);
    }

    let summary = status_task_summary(
        &listener,
        &socket_path,
        &mut ledger,
        &mut current_task,
        &config,
    );
    assert_eq!(
        summary.replan_state,
        ReplanState::Replanned { count: 1 },
        "crossing the absolute tool-call-count fallback threshold must trigger exactly one replan"
    );
    assert_ne!(
        summary.plan_id, original_plan_id,
        "a replan must produce a new, different plan id"
    );
    assert!(
        summary.remaining_estimate.duration_p90_secs.unwrap()
            > original_estimate.duration_p90_secs.unwrap(),
        "the deterministic tier must widen the remaining estimate's P90 above the original"
    );

    // A further material event, still within the default 300s cooldown,
    // must NOT trigger a second replan.
    let response = send(
        &listener,
        &socket_path,
        &mut ledger,
        &mut current_task,
        &config,
        Request::ToolInvoked {
            session_id: "replan-session".to_string(),
            tool_name: "Bash".to_string(),
        },
    );
    assert_eq!(response, Response::Ack);
    let summary_after_cooldown_suppression = status_task_summary(
        &listener,
        &socket_path,
        &mut ledger,
        &mut current_task,
        &config,
    );
    assert_eq!(
        summary_after_cooldown_suppression.replan_state,
        ReplanState::Replanned { count: 1 },
        "a second material event inside the cooldown window must not thrash another replan"
    );

    // Persisted, queryable replan history (HORO-1139 acceptance
    // criterion): exactly one linked record.
    let history = ledger.replan_history_for_task(summary.task_id).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].prior_plan_id, original_plan_id);
    assert_eq!(history[0].new_plan_id, summary.plan_id);
    assert_eq!(
        history[0].reason.trigger,
        ReplanTriggerKind::ToolCallCountExceeded
    );
}

#[test]
fn possible_tool_loop_streak_triggers_a_replan_before_the_count_threshold() {
    let dir = tempfile::tempdir().unwrap();
    let config = DaemonConfig {
        socket_path: dir.path().join("d.sock"),
        ledger_path: dir.path().join("ledger.sqlite3"),
        log_path: dir.path().join("daemon.log"),
        recon_budget: ReconBudget::default(),
        replan_hysteresis: ReplanHysteresisConfig::default(),
    };

    let mut ledger = LedgerStore::open(&config.ledger_path).unwrap();
    seed_same_repo_history(&mut ledger);

    let mut current_task = None;
    let listener = libra_governor_daemon::bind_or_detect_running(&config.socket_path).unwrap();
    let socket_path = dir.path().join("d.sock");

    send(
        &listener,
        &socket_path,
        &mut ledger,
        &mut current_task,
        &config,
        Request::Preflight {
            task_hint: "fix the login bug".to_string(),
            cwd: fixture_repo(),
            session_id: "loop-session".to_string(),
        },
    );

    // 4 consecutive invocations of the SAME tool -- well under the
    // absolute tool-call-count fallback (20), but at the loop-streak
    // threshold.
    for _ in 0..4 {
        let response = send(
            &listener,
            &socket_path,
            &mut ledger,
            &mut current_task,
            &config,
            Request::ToolInvoked {
                session_id: "loop-session".to_string(),
                tool_name: "Bash".to_string(),
            },
        );
        assert_eq!(response, Response::Ack);
    }

    let summary = status_task_summary(
        &listener,
        &socket_path,
        &mut ledger,
        &mut current_task,
        &config,
    );
    assert_eq!(summary.replan_state, ReplanState::Replanned { count: 1 });

    let history = ledger.replan_history_for_task(summary.task_id).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(
        history[0].reason.trigger,
        ReplanTriggerKind::PossibleToolLoop
    );
}

#[test]
fn exhausting_the_auto_replan_budget_escalates_instead_of_replanning_again() {
    let dir = tempfile::tempdir().unwrap();
    let config = DaemonConfig {
        socket_path: dir.path().join("d.sock"),
        ledger_path: dir.path().join("ledger.sqlite3"),
        log_path: dir.path().join("daemon.log"),
        recon_budget: ReconBudget::default(),
        // Zero cooldown, budget of exactly one automatic replan -- so
        // the second material event in this test deterministically
        // escalates rather than being suppressed by the cooldown.
        replan_hysteresis: ReplanHysteresisConfig {
            cooldown_secs: 0,
            max_auto_replans: 1,
        },
    };

    let mut ledger = LedgerStore::open(&config.ledger_path).unwrap();
    seed_same_repo_history(&mut ledger);

    let mut current_task = None;
    let listener = libra_governor_daemon::bind_or_detect_running(&config.socket_path).unwrap();
    let socket_path = dir.path().join("d.sock");

    send(
        &listener,
        &socket_path,
        &mut ledger,
        &mut current_task,
        &config,
        Request::Preflight {
            task_hint: "fix the login bug".to_string(),
            cwd: fixture_repo(),
            session_id: "escalate-session".to_string(),
        },
    );

    let tools = ["Bash", "Read", "Grep", "Write"];
    // 21 calls crosses the absolute fallback threshold and consumes the
    // one-replan budget.
    for i in 0..21u64 {
        send(
            &listener,
            &socket_path,
            &mut ledger,
            &mut current_task,
            &config,
            Request::ToolInvoked {
                session_id: "escalate-session".to_string(),
                tool_name: tools[(i as usize) % tools.len()].to_string(),
            },
        );
    }
    let after_first_replan = status_task_summary(
        &listener,
        &socket_path,
        &mut ledger,
        &mut current_task,
        &config,
    );
    assert_eq!(
        after_first_replan.replan_state,
        ReplanState::Replanned { count: 1 }
    );

    // One more material tool call: the count is still (in fact further)
    // past the threshold, but the auto-replan budget is exhausted.
    send(
        &listener,
        &socket_path,
        &mut ledger,
        &mut current_task,
        &config,
        Request::ToolInvoked {
            session_id: "escalate-session".to_string(),
            tool_name: "Bash".to_string(),
        },
    );
    let after_escalation = status_task_summary(
        &listener,
        &socket_path,
        &mut ledger,
        &mut current_task,
        &config,
    );
    assert_eq!(
        after_escalation.replan_state,
        ReplanState::EscalatedAwaitingApproval,
        "exhausting the auto-replan budget must escalate, not silently replan again"
    );

    // No second replan was persisted.
    let history = ledger
        .replan_history_for_task(after_first_replan.task_id)
        .unwrap();
    assert_eq!(history.len(), 1);
}
