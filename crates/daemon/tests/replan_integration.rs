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
///
/// Each receipt also carries a small, deliberately chosen
/// `tool_call_count` (2, 2, 3, 3, 4) so `typical_tool_call_count_bucketed`
/// resolves to a real bucket-specific median of `3` rather than falling
/// back to the absolute fallback (20) — keeping the material-deviation
/// threshold (`2x typical` = 6) small enough that these tests can drive a
/// handful of `ToolInvoked` calls instead of dozens.
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

/// Sends `count` `ToolInvoked` notifications for `session_id`, cycling
/// through four distinct tool names so no same-tool streak ever reaches
/// the loop threshold (4) — isolates the tool-call-count material-
/// deviation signal from the loop signal.
fn send_distinct_tool_calls(
    listener: &std::os::unix::net::UnixListener,
    socket_path: &Path,
    ledger: &mut LedgerStore,
    current_task: &mut Option<libra_governor_protocol::TaskSummary>,
    config: &DaemonConfig,
    session_id: &str,
    count: u64,
) {
    let tools = ["Bash", "Read", "Grep", "Write"];
    for i in 0..count {
        let response = send(
            listener,
            socket_path,
            ledger,
            current_task,
            config,
            Request::ToolInvoked {
                session_id: session_id.to_string(),
                tool_name: tools[(i as usize) % tools.len()].to_string(),
            },
        );
        assert_eq!(response, Response::Ack);
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
        policy: libra_governor_daemon::default_admission_policy(),
        reservation_ttl_secs: 900,
        gateway: None,
        gateway_stats: std::sync::Arc::new(Default::default()),
        gateway_session_header: libra_governor_gateway::proxy::DEFAULT_SESSION_HEADER.to_string(),
        extensions: None,
        extension_runtime: std::sync::OnceLock::new(),
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
    let contract_before = ledger
        .latest_contract(preflight.task_id)
        .unwrap()
        .expect("preflight always records a contract");

    // Seeded history gives a bucket-typical tool-call count of 3, so the
    // material-deviation threshold is `> 2 * 3 = 6`. 7 distinct-tool
    // calls crosses it.
    send_distinct_tool_calls(
        &listener,
        &socket_path,
        &mut ledger,
        &mut current_task,
        &config,
        "replan-session",
        7,
    );

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
        "crossing the bucket-typical tool-call-count threshold must trigger exactly one replan"
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

    // Required-criteria protection (HORO-1139 acceptance criterion,
    // mirroring HORO-1137's policy invariant test style): a replan must
    // never alter or drop the task's Completion Contract criteria. The
    // new plan's `contract_revision` must still resolve to the exact
    // same criteria set the original preflight recorded.
    let contract_after = ledger
        .latest_contract(summary.task_id)
        .unwrap()
        .expect("contract still present after replan");
    assert_eq!(
        contract_before.criteria, contract_after.criteria,
        "a replan must never alter or drop required Completion Contract criteria"
    );
    let new_plan = ledger
        .get_plan(summary.plan_id)
        .unwrap()
        .expect("the replanned plan must be persisted");
    assert_eq!(
        new_plan.contract_revision, contract_before.revision,
        "the replanned plan must still reference the same contract revision"
    );

    // A further material event -- a genuine new deviation, not stale
    // evidence: material-event detection is re-baselined at the last
    // replan's tool-call count (see `ReplanHysteresisState` docs), so
    // this sends 7 MORE calls, crossing the threshold again relative to
    // the new baseline. Still within the default 300s cooldown, so this
    // must NOT trigger a second replan.
    send_distinct_tool_calls(
        &listener,
        &socket_path,
        &mut ledger,
        &mut current_task,
        &config,
        "replan-session",
        7,
    );
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
        "a second, genuinely material event inside the cooldown window must not thrash another replan"
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
        policy: libra_governor_daemon::default_admission_policy(),
        reservation_ttl_secs: 900,
        gateway: None,
        gateway_stats: std::sync::Arc::new(Default::default()),
        gateway_session_header: libra_governor_gateway::proxy::DEFAULT_SESSION_HEADER.to_string(),
        extensions: None,
        extension_runtime: std::sync::OnceLock::new(),
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
        policy: libra_governor_daemon::default_admission_policy(),
        reservation_ttl_secs: 900,
        gateway: None,
        gateway_stats: std::sync::Arc::new(Default::default()),
        gateway_session_header: libra_governor_gateway::proxy::DEFAULT_SESSION_HEADER.to_string(),
        extensions: None,
        extension_runtime: std::sync::OnceLock::new(),
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

    // Seeded history gives a bucket-typical tool-call count of 3
    // (threshold `> 6`). 7 distinct-tool calls crosses it and consumes
    // the one-replan budget.
    send_distinct_tool_calls(
        &listener,
        &socket_path,
        &mut ledger,
        &mut current_task,
        &config,
        "escalate-session",
        7,
    );
    let after_first_replan = status_task_summary(
        &listener,
        &socket_path,
        &mut ledger,
        &mut current_task,
        &config,
    );
    // The budget is exactly `1`, and this replan IS the one that
    // exhausts it -- `replan_state` must render as escalated immediately,
    // consistent with `evaluate_hysteresis`'s own `auto_replan_count >=
    // max_auto_replans` check, not wait for the next material event to
    // notice.
    assert_eq!(
        after_first_replan.replan_state,
        ReplanState::EscalatedAwaitingApproval
    );

    // A genuinely new material deviation relative to the re-baselined
    // threshold (7 more calls, crossing the threshold again since the
    // last replan) -- not stale evidence from the same event. The
    // auto-replan budget is exhausted, so this must escalate instead of
    // silently replanning again.
    send_distinct_tool_calls(
        &listener,
        &socket_path,
        &mut ledger,
        &mut current_task,
        &config,
        "escalate-session",
        7,
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
