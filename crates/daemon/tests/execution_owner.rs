//! Real Unix IPC and SQLite effects; fixture identities are not native-host evidence.
use libra_governor_daemon::{default_admission_policy, recon::ReconBudget, DaemonConfig};
use libra_governor_domain::{
    AssociationUnavailable as Gap, ExecutionIdentity, ExecutionIdentityBuilder, ExecutionPosition,
    ExecutionTarget, ReservationState, EXECUTION_ASSOCIATION_VERSION,
};
use libra_governor_ledger::LedgerStore;
use libra_governor_protocol::{
    wire, ExecutionOperation as Op, ExecutionOwnerOutcome as Outcome, ExecutionOwnerRequest,
    NativeExecutionContext, Request, RequestEnvelope, Response, ResponseEnvelope, TaskSummary,
    PROTOCOL_VERSION,
};
use std::{
    io::BufReader,
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
    sync::{Arc, OnceLock},
};

fn identity(provider: &str, agent: &str, turn: &str) -> ExecutionIdentity {
    ExecutionIdentityBuilder::new("fixture-host", provider)
        .provider_session_id("same-native-session")
        .agent_id(agent)
        .turn_id(turn)
        .build_at(time::OffsetDateTime::now_utc())
        .unwrap()
}
fn owner(identity: ExecutionIdentity, operation: Op) -> Request {
    Request::ExecutionOwner {
        event: Box::new(ExecutionOwnerRequest {
            association_version: EXECUTION_ASSOCIATION_VERSION,
            native_context: NativeExecutionContext {
                identity: identity.clone(),
                operation,
            },
            identity,
        }),
    }
}
fn prompt(i: ExecutionIdentity) -> Request {
    owner(
        i,
        Op::Prompt {
            task_hint: String::new(),
            cwd: Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample_repo"),
            supersedes_turn: None,
        },
    )
}
fn tool(i: ExecutionIdentity, call: &str) -> Request {
    owner(
        i,
        Op::Tool {
            native_call_id: call.into(),
            tool_name: "Read".into(),
        },
    )
}
fn stop(i: ExecutionIdentity) -> Request {
    owner(
        i,
        Op::Stop {
            model: None,
            transcript_path: None,
        },
    )
}
fn config(dir: &Path, name: &str) -> DaemonConfig {
    DaemonConfig {
        socket_path: dir.join(format!("{name}.sock")),
        ledger_path: dir.join("ledger.sqlite3"),
        log_path: dir.join("daemon.log"),
        recon_budget: ReconBudget::default(),
        replan_hysteresis: Default::default(),
        policy: default_admission_policy(),
        reservation_ttl_secs: 900,
        gateway: None,
        gateway_stats: Arc::new(Default::default()),
        gateway_session_header: "x-libra-session".into(),
        extensions: None,
        extension_runtime: OnceLock::new(),
        progressive_interval_secs: 60,
    }
}
fn send(
    listener: &UnixListener,
    ledger: &mut LedgerStore,
    cfg: &DaemonConfig,
    request: Request,
    version: u32,
) -> Response {
    let socket = cfg.socket_path.clone();
    let client = std::thread::spawn(move || {
        let stream = UnixStream::connect(socket).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        wire::write_message(
            &stream,
            &RequestEnvelope {
                protocol_version: version,
                request,
            },
        )
        .unwrap();
        let envelope: ResponseEnvelope = wire::read_message(BufReader::new(stream)).unwrap();
        envelope.response
    });
    let (stream, _) = listener.accept().unwrap();
    let mut host_summary: Option<TaskSummary> = None;
    libra_governor_daemon::handle_connection(stream, ledger, &mut host_summary, cfg).unwrap();
    client.join().unwrap()
}
struct Fixture {
    dir: tempfile::TempDir,
    cfg: DaemonConfig,
    listener: UnixListener,
    ledger: LedgerStore,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(dir.path(), "owner");
        let listener = UnixListener::bind(&cfg.socket_path).unwrap();
        let ledger = LedgerStore::open(&cfg.ledger_path).unwrap();
        Self {
            dir,
            cfg,
            listener,
            ledger,
        }
    }
    fn request(&mut self, r: Request) -> Response {
        send(
            &self.listener,
            &mut self.ledger,
            &self.cfg,
            r,
            PROTOCOL_VERSION,
        )
    }
    fn outcome(&mut self, r: Request) -> Outcome {
        match self.request(r) {
            Response::ExecutionOwner(outcome) => *outcome,
            other => panic!("unexpected response: {other:?}"),
        }
    }
    fn count(&self, table: &str) -> u64 {
        rusqlite::Connection::open(&self.cfg.ledger_path)
            .unwrap()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }
    fn reopen(&mut self) {
        self.ledger = LedgerStore::open(&self.cfg.ledger_path).unwrap();
    }
}
fn applied(outcome: Outcome) -> ExecutionTarget {
    match outcome {
        Outcome::Applied { target, .. } => target,
        other => panic!("not applied: {other:?}"),
    }
}
fn gap(outcome: Outcome, reason: Gap) {
    assert_eq!(outcome, Outcome::Unavailable { reason });
}

#[test]
fn same_session_agents_and_providers_never_share_tasks_or_finalize_siblings() {
    let mut f = Fixture::new();
    let a = identity("codex", "agent-a", "turn-1");
    let b = identity("codex", "agent-b", "turn-1");
    let c = identity("claude-code", "agent-a", "turn-1");
    let ta = applied(f.outcome(prompt(a.clone())));
    let tb = applied(f.outcome(prompt(b.clone())));
    let tc = applied(f.outcome(prompt(c.clone())));
    assert_ne!(ta.task_id, tb.task_id);
    assert_ne!(ta.task_id, tc.task_id);
    assert_ne!(tb.task_id, tc.task_id);
    applied(f.outcome(tool(a.clone(), "call-a")));
    applied(f.outcome(tool(b.clone(), "call-b")));
    applied(f.outcome(stop(a)));
    assert_eq!(
        f.ledger.task_trajectory(ta.task_id).unwrap().receipts.len(),
        1
    );
    assert_eq!(
        f.ledger.task_trajectory(tb.task_id).unwrap().receipts.len(),
        0
    );
    assert_eq!(
        f.ledger.task_trajectory(tc.task_id).unwrap().receipts.len(),
        0
    );
    for (i, target) in [(b, tb), (c, tc)] {
        match f.outcome(owner(i, Op::Query {})) {
            Outcome::Resolved {
                target: actual,
                budget,
            } => {
                assert_eq!(actual, target);
                assert!(budget.is_some());
            }
            other => panic!("sibling unavailable: {other:?}"),
        }
    }
}

#[test]
fn superseding_prompt_makes_older_tool_stop_and_prompt_stale_without_effects() {
    let mut f = Fixture::new();
    let old = identity("codex", "agent", "turn-old");
    let new = identity("codex", "agent", "turn-new");
    let first = applied(f.outcome(prompt(old.clone())));
    applied(f.outcome(tool(old.clone(), "older-applied")));
    let mut next = prompt(new.clone());
    if let Request::ExecutionOwner { event } = &mut next {
        if let Op::Prompt {
            supersedes_turn, ..
        } = &mut event.native_context.operation
        {
            *supersedes_turn = Some("turn-old".into());
        }
    }
    let second = applied(f.outcome(next));
    assert_eq!(first.task_id, second.task_id);
    assert_ne!(first.plan_id, second.plan_id);
    let before = f.count("execution_replays");
    for r in [
        tool(old.clone(), "late"),
        tool(old.clone(), "older-applied"),
        stop(old.clone()),
        prompt(old),
    ] {
        gap(f.outcome(r), Gap::Stale);
    }
    assert_eq!(f.count("execution_replays"), before);
    assert_eq!(f.count("receipts"), 0);
    applied(f.outcome(tool(new.clone(), "current")));
    applied(f.outcome(stop(new)));
    let t = f.ledger.task_trajectory(second.task_id).unwrap();
    assert_eq!(t.receipts.len(), 1);
    assert_eq!(t.receipts[0].plan_id, second.plan_id);
}

#[test]
fn native_replay_keys_survive_reopen_and_fresh_transport_event_ids() {
    let mut f = Fixture::new();
    let i = identity("codex", "agent", "turn");
    let target = applied(f.outcome(prompt(i.clone())));
    f.reopen();
    assert!(
        matches!(f.outcome(prompt(i.clone())), Outcome::Duplicate { target: t } if t == target)
    );
    applied(f.outcome(tool(i.clone(), "native-call")));
    f.reopen();
    let mut wire = serde_json::to_value(&i).unwrap();
    wire["event_id"] = serde_json::json!(uuid::Uuid::new_v4().to_string());
    let fresh = serde_json::from_value(wire).unwrap();
    assert!(matches!(
        f.outcome(tool(fresh, "native-call")),
        Outcome::Duplicate { .. }
    ));
    applied(f.outcome(tool(i.clone(), "distinct-call")));
    let position = ExecutionPosition::from_identity(&i).unwrap();
    assert_eq!(
        f.ledger
            .tool_call_count_for_session(&LedgerStore::execution_lane_session(&position))
            .unwrap(),
        2
    );
    applied(f.outcome(stop(i.clone())));
    let settled = f.ledger.budget_snapshot(target.task_id).unwrap().unwrap();
    f.reopen();
    assert!(matches!(f.outcome(stop(i)), Outcome::Duplicate { .. }));
    assert_eq!(f.count("tasks"), 1);
    assert_eq!(f.count("plans"), 1);
    assert_eq!(f.count("receipts"), 1);
    assert_eq!(
        f.ledger.budget_snapshot(target.task_id).unwrap().unwrap(),
        settled
    );
    assert!(f
        .ledger
        .reservations_for_task(target.task_id)
        .unwrap()
        .iter()
        .all(|r| r.state == ReservationState::Settled));
}

#[test]
fn missing_richer_dimensions_mismatched_context_and_legacy_rows_never_guess() {
    let mut f = Fixture::new();
    let i = identity("codex", "agent", "turn");
    gap(f.outcome(owner(i.clone(), Op::Query {})), Gap::Missing);
    let legacy = f.request(Request::Preflight {
        task_hint: String::new(),
        cwd: f.dir.path().into(),
        session_id: "same-native-session".into(),
    });
    assert!(matches!(legacy, Response::Preflight(_)));
    gap(f.outcome(owner(i.clone(), Op::Query {})), Gap::Missing);
    let task = applied(f.outcome(prompt(i.clone())));
    for field in ["provider_session_id", "agent_id", "turn_id"] {
        let mut wire = serde_json::to_value(&i).unwrap();
        wire.as_object_mut().unwrap().remove(field);
        gap(
            f.outcome(owner(serde_json::from_value(wire).unwrap(), Op::Query {})),
            Gap::Unsupported,
        );
    }
    let mut richer = serde_json::to_value(&i).unwrap();
    richer["repo_id"] = serde_json::json!("a-real-repo-id");
    gap(
        f.outcome(owner(serde_json::from_value(richer).unwrap(), Op::Query {})),
        Gap::Ambiguous,
    );
    let mut mismatch = owner(
        i.clone(),
        Op::Stop {
            model: None,
            transcript_path: None,
        },
    );
    if let Request::ExecutionOwner { event } = &mut mismatch {
        event.native_context.identity = identity("codex", "sibling", "turn");
    }
    gap(f.outcome(mismatch), Gap::Ambiguous);
    assert_eq!(f.count("receipts"), 0);
    assert_eq!(
        task.lineage_status,
        libra_governor_domain::LineageStatus::Unknown
    );
    let reserved =
        LedgerStore::execution_lane_session(&ExecutionPosition::from_identity(&i).unwrap());
    assert!(matches!(
        f.request(Request::ToolInvoked {
            session_id: reserved,
            tool_name: "Read".into()
        }),
        Response::Error { .. }
    ));
}

#[test]
fn receipt_failure_rolls_back_settlement_close_and_replay_then_retry_applies_once() {
    let mut f = Fixture::new();
    let i = identity("codex", "agent", "turn");
    let target = applied(f.outcome(prompt(i.clone())));
    let conn = rusqlite::Connection::open(&f.cfg.ledger_path).unwrap();
    conn.execute_batch("CREATE TRIGGER reject_receipt BEFORE INSERT ON receipts BEGIN SELECT RAISE(ABORT,'fixture failure'); END;").unwrap();
    assert!(matches!(f.request(stop(i.clone())), Response::Error { .. }));
    assert_eq!(f.count("receipts"), 0);
    assert_eq!(f.count("execution_replays"), 1);
    assert!(f
        .ledger
        .reservations_for_task(target.task_id)
        .unwrap()
        .iter()
        .any(|r| r.state == ReservationState::Active));
    assert!(matches!(
        f.outcome(owner(i.clone(), Op::Query {})),
        Outcome::Resolved { .. }
    ));
    conn.execute_batch("DROP TRIGGER reject_receipt;").unwrap();
    f.reopen();
    applied(f.outcome(stop(i)));
    assert_eq!(f.count("receipts"), 1);
    assert_eq!(f.count("execution_replays"), 2);
}

#[test]
fn protocol_or_owner_version_skew_and_session_transcript_cannot_create_effects() {
    let mut f = Fixture::new();
    let i = identity("codex", "agent", "turn");
    let r = send(
        &f.listener,
        &mut f.ledger,
        &f.cfg,
        prompt(i.clone()),
        PROTOCOL_VERSION - 1,
    );
    assert!(matches!(r, Response::Error { .. }));
    assert_eq!(f.count("tasks"), 0);
    let mut skew = prompt(i.clone());
    if let Request::ExecutionOwner { event } = &mut skew {
        event.association_version += 1;
    }
    gap(f.outcome(skew), Gap::Unsupported);
    assert_eq!(f.count("tasks"), 0);
    applied(f.outcome(prompt(i.clone())));
    gap(
        f.outcome(owner(
            i,
            Op::Stop {
                model: None,
                transcript_path: Some("/fixture/shared-session-transcript".into()),
            },
        )),
        Gap::Unsupported,
    );
    assert_eq!(f.count("receipts"), 0);
}

#[test]
fn concurrent_connections_commit_one_prompt_and_preserve_all_other_lanes() {
    let dir = tempfile::tempdir().unwrap();
    let seed = LedgerStore::open(dir.path().join("ledger.sqlite3")).unwrap();
    drop(seed);
    let gate = Arc::new(std::sync::Barrier::new(6));
    let mut handles = vec![];
    for index in 0..6 {
        let cfg = config(dir.path(), &format!("lane{index}"));
        let listener = UnixListener::bind(&cfg.socket_path).unwrap();
        let gate = gate.clone();
        handles.push(std::thread::spawn(move || {
            let mut ledger = LedgerStore::open(&cfg.ledger_path).unwrap();
            gate.wait();
            let agent = if index < 3 {
                "duplicate-agent".into()
            } else {
                format!("agent{index}")
            };
            let mut i = identity("codex", &agent, "turn");
            if index >= 4 {
                let mut wire = serde_json::to_value(&i).unwrap();
                wire["provider_session_id"] = serde_json::json!(format!("session{index}"));
                // Reuse an agent ID across sessions; session scope must isolate it.
                wire["agent_id"] = serde_json::json!("duplicate-agent");
                i = serde_json::from_value(wire).unwrap();
            }
            let response = send(
                &listener,
                &mut ledger,
                &cfg,
                prompt(i.clone()),
                PROTOCOL_VERSION,
            );
            let Response::ExecutionOwner(outcome) = response else {
                panic!("owner error")
            };
            let target = match &*outcome {
                Outcome::Applied { target, .. } | Outcome::Duplicate { target } => target.clone(),
                other => panic!("unexpected concurrent outcome: {other:?}"),
            };
            match send(
                &listener,
                &mut ledger,
                &cfg,
                owner(i, Op::Query {}),
                PROTOCOL_VERSION,
            ) {
                Response::ExecutionOwner(query) => match *query {
                    Outcome::Resolved {
                        target: selected,
                        budget,
                    } => {
                        assert_eq!(selected, target);
                        assert!(budget.is_some());
                    }
                    other => panic!("cross-session query: {other:?}"),
                },
                other => panic!("query error: {other:?}"),
            }
            *outcome
        }));
    }
    let outcomes: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| matches!(o, Outcome::Applied { .. }))
            .count(),
        4
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| matches!(o, Outcome::Duplicate { .. }))
            .count(),
        2
    );
    let conn = rusqlite::Connection::open(dir.path().join("ledger.sqlite3")).unwrap();
    let plans: u64 = conn
        .query_row("SELECT COUNT(*) FROM plans", [], |r| r.get(0))
        .unwrap();
    assert_eq!(plans, 4);
}

fn superseding(i: ExecutionIdentity, prior: &str) -> Request {
    let mut r = prompt(i);
    if let Request::ExecutionOwner { event } = &mut r {
        if let Op::Prompt {
            supersedes_turn, ..
        } = &mut event.native_context.operation
        {
            *supersedes_turn = Some(prior.into());
        }
    }
    r
}

#[test]
fn delayed_unseen_prompt_requires_exact_native_predecessor() {
    let mut f = Fixture::new();
    let a = identity("codex", "agent", "a");
    let b = identity("codex", "agent", "b");
    let c = identity("codex", "agent", "delayed-c");
    applied(f.outcome(prompt(a)));
    let target = applied(f.outcome(superseding(b.clone(), "a")));
    gap(f.outcome(superseding(c.clone(), "a")), Gap::Stale);
    gap(f.outcome(prompt(c)), Gap::Ambiguous);
    assert_eq!(f.count("plans"), 2);
    match f.outcome(owner(b, Op::Query {})) {
        Outcome::Resolved { target: actual, .. } => assert_eq!(actual, target),
        other => panic!("current turn replaced: {other:?}"),
    }
}

#[test]
fn plan_replacement_is_resolved_and_duplicate_tool_never_replans_again() {
    let mut f = Fixture::new();
    let i = identity("codex", "agent", "turn");
    let first = applied(f.outcome(prompt(i.clone())));
    // Loop-signal gate exercised via real existing policy; no hand-written plan update.
    let mut last = first.clone();
    for index in 0..8 {
        last = applied(f.outcome(tool(i.clone(), &format!("call{index}"))));
    }
    assert_ne!(
        first.plan_id, last.plan_id,
        "fixture must actually trigger the material replan gate"
    );
    assert_eq!(last.initial_plan_id, first.initial_plan_id);
    let plans = f.count("plans");
    assert!(matches!(
        f.outcome(tool(i.clone(), "call7")),
        Outcome::Duplicate { .. }
    ));
    assert_eq!(f.count("plans"), plans);
    let stop_target = applied(f.outcome(stop(i)));
    assert_eq!(stop_target.plan_id, last.plan_id);
    let trajectory = f.ledger.task_trajectory(first.task_id).unwrap();
    assert_eq!(trajectory.receipts.len(), 1);
    assert_eq!(trajectory.receipts[0].plan_id, last.plan_id);
}

#[test]
fn native_tool_reference_cannot_be_reused_on_a_new_turn() {
    let mut f = Fixture::new();
    let a = identity("codex", "agent", "a");
    let b = identity("codex", "agent", "b");
    applied(f.outcome(prompt(a.clone())));
    applied(f.outcome(tool(a, "same-native-call")));
    applied(f.outcome(superseding(b.clone(), "a")));
    gap(f.outcome(tool(b, "same-native-call")), Gap::ReplayConflict);
}

#[test]
fn replay_insert_failure_rolls_back_the_receipt_and_settlement_too() {
    let mut f = Fixture::new();
    let i = identity("codex", "agent", "turn");
    let target = applied(f.outcome(prompt(i.clone())));
    let conn = rusqlite::Connection::open(&f.cfg.ledger_path).unwrap();
    conn.execute_batch("CREATE TRIGGER reject_replay BEFORE INSERT ON execution_replays WHEN NEW.operation='stop' BEGIN SELECT RAISE(ABORT,'fixture failure'); END;").unwrap();
    assert!(matches!(f.request(stop(i.clone())), Response::Error { .. }));
    assert_eq!(f.count("receipts"), 0);
    assert_eq!(f.count("execution_replays"), 1);
    assert!(f
        .ledger
        .reservations_for_task(target.task_id)
        .unwrap()
        .iter()
        .any(|r| r.state == ReservationState::Active));
    conn.execute_batch("DROP TRIGGER reject_replay;").unwrap();
    f.reopen();
    applied(f.outcome(stop(i)));
    assert_eq!(f.count("receipts"), 1);
}

#[test]
fn owner_rows_do_not_persist_recon_input_and_queries_use_indexes() {
    let mut f = Fixture::new();
    let marker = "NON_SENSITIVE_FIXTURE_RECON_INPUT";
    let i = identity("codex", "agent", "turn");
    let mut r = prompt(i);
    if let Request::ExecutionOwner { event } = &mut r {
        if let Op::Prompt { task_hint, .. } = &mut event.native_context.operation {
            *task_hint = marker.into();
        }
    }
    applied(f.outcome(r));
    let conn = rusqlite::Connection::open(&f.cfg.ledger_path).unwrap();
    for table in ["execution_lanes", "execution_turns", "execution_replays"] {
        let mut statement = conn.prepare(&format!("SELECT * FROM {table}")).unwrap();
        let columns = statement.column_count();
        let rows: Vec<Vec<String>> = statement
            .query_map([], |row| {
                Ok((0..columns)
                    .filter_map(|index| row.get::<_, String>(index).ok())
                    .collect())
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(!rows.iter().flatten().any(|value| value.contains(marker)));
    }
    let detail:String=conn.query_row("EXPLAIN QUERY PLAN SELECT target_json FROM execution_replays WHERE lane=?1 AND operation=?2 AND native_ref=?3",["lane","tool","call"],|r|r.get(3)).unwrap();
    assert!(
        detail.contains("INDEX"),
        "indexed exact lookup required: {detail}"
    );
}

#[test]
fn richer_context_drift_cannot_create_a_second_lane_or_effect() {
    let mut f = Fixture::new();
    let original = identity("codex", "agent", "turn-a");
    let target = applied(f.outcome(prompt(original.clone())));
    let baseline_replays = f.count("execution_replays");
    for (field, value) in [
        ("lineage_status", serde_json::json!("root")),
        ("tool_instance_id", serde_json::json!("instance")),
        ("session_lineage_id", serde_json::json!("session-parent")),
        ("repo_id", serde_json::json!("repo")),
        ("worktree_id", serde_json::json!("worktree")),
    ] {
        let mut wire = serde_json::to_value(&original).unwrap();
        wire[field] = value;
        let changed: ExecutionIdentity = serde_json::from_value(wire.clone()).unwrap();
        gap(f.outcome(prompt(changed.clone())), Gap::Ambiguous);
        gap(f.outcome(owner(changed, Op::Query {})), Gap::Ambiguous);
        wire["turn_id"] = serde_json::json!("turn-b");
        let changed_turn: ExecutionIdentity = serde_json::from_value(wire).unwrap();
        gap(
            f.outcome(superseding(changed_turn, "turn-a")),
            Gap::Ambiguous,
        );
    }
    let child = ExecutionIdentityBuilder::new("fixture-host", "codex")
        .provider_session_id("same-native-session")
        .agent_id("agent")
        .turn_id("turn-a")
        .child_lineage("parent")
        .build_at(time::OffsetDateTime::now_utc())
        .unwrap();
    gap(f.outcome(prompt(child)), Gap::Ambiguous);
    assert_eq!(f.count("tasks"), 1);
    assert_eq!(f.count("plans"), 1);
    assert_eq!(f.count("execution_lanes"), 1);
    assert_eq!(f.count("execution_replays"), baseline_replays);
    let successor = applied(f.outcome(superseding(identity("codex", "agent", "turn-b"), "turn-a")));
    assert_eq!(successor.task_id, target.task_id);
    assert_ne!(successor.plan_id, target.plan_id);
}

#[test]
fn configured_unsupported_effects_do_not_disable_exact_queries() {
    let mut f = Fixture::new();
    let i = identity("codex", "agent", "turn");
    let target = applied(f.outcome(prompt(i.clone())));
    f.cfg.gateway = Some(libra_governor_gateway::config::GatewayConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        f.dir.path().join("unused-token"),
        libra_governor_gateway::config::GatewayCredentialMode::PassThroughSubscription,
    ));
    gap(f.outcome(tool(i.clone(), "call")), Gap::Unsupported);
    gap(f.outcome(stop(i.clone())), Gap::Unsupported);
    match f.outcome(owner(i, Op::Query {})) {
        Outcome::Resolved {
            target: selected,
            budget,
        } => {
            assert_eq!(selected, target);
            assert!(budget.is_some());
        }
        other => panic!("exact query became unavailable: {other:?}"),
    }
    assert_eq!(f.count("execution_replays"), 1);
    assert_eq!(f.count("receipts"), 0);
    assert!(!f.dir.path().join("unused-token").exists());
}

#[test]
fn a_successor_invalidates_retries_of_an_already_finalized_turn() {
    let mut f = Fixture::new();
    let old = identity("codex", "agent", "old");
    applied(f.outcome(prompt(old.clone())));
    applied(f.outcome(stop(old.clone())));
    assert!(matches!(
        f.outcome(stop(old.clone())),
        Outcome::Duplicate { .. }
    ));
    let current = applied(f.outcome(superseding(identity("codex", "agent", "new"), "old")));
    gap(f.outcome(stop(old.clone())), Gap::Stale);
    gap(f.outcome(prompt(old)), Gap::Stale);
    assert_eq!(f.count("receipts"), 1);
    match f.outcome(owner(identity("codex", "agent", "new"), Op::Query {})) {
        Outcome::Resolved { target, .. } => assert_eq!(target, current),
        other => panic!("successor changed: {other:?}"),
    }
}

#[test]
fn explicit_child_lineage_survives_selection_without_finalizing_its_parent() {
    let mut f = Fixture::new();
    let parent = ExecutionIdentityBuilder::new("fixture-host", "codex")
        .provider_session_id("same-native-session")
        .agent_id("parent")
        .turn_id("parent-turn")
        .root_lineage()
        .build_at(time::OffsetDateTime::now_utc())
        .unwrap();
    let child = ExecutionIdentityBuilder::new("fixture-host", "codex")
        .provider_session_id("same-native-session")
        .agent_id("child")
        .turn_id("child-turn")
        .child_lineage("parent")
        .session_lineage_id("native-lineage")
        .build_at(time::OffsetDateTime::now_utc())
        .unwrap();
    let parent_target = applied(f.outcome(prompt(parent.clone())));
    let child_target = applied(f.outcome(prompt(child.clone())));
    assert_ne!(parent_target.task_id, child_target.task_id);
    assert_eq!(
        child_target.lineage_status,
        libra_governor_domain::LineageStatus::Child
    );
    assert_eq!(child_target.parent_agent_id.as_deref(), Some("parent"));
    assert_eq!(child_target.initial_plan_id, child_target.plan_id);
    match f.outcome(owner(child.clone(), Op::Query {})) {
        Outcome::Resolved { target, .. } => assert_eq!(target, child_target),
        other => panic!("child selection unavailable: {other:?}"),
    }
    applied(f.outcome(stop(child)));
    match f.outcome(owner(parent, Op::Query {})) {
        Outcome::Resolved { target, .. } => assert_eq!(target, parent_target),
        other => panic!("parent was finalized by child: {other:?}"),
    }
    assert!(f
        .ledger
        .task_trajectory(parent_target.task_id)
        .unwrap()
        .receipts
        .is_empty());
    assert_eq!(
        f.ledger
            .task_trajectory(child_target.task_id)
            .unwrap()
            .receipts
            .len(),
        1
    );
}

#[test]
fn inconsistent_persisted_identity_refuses_selection_and_effects() {
    let mut f = Fixture::new();
    let i = identity("codex", "agent", "turn");
    applied(f.outcome(prompt(i.clone())));
    let position = ExecutionPosition::from_identity(&i).unwrap();
    let conn = rusqlite::Connection::open(&f.cfg.ledger_path).unwrap();
    let mut stored = serde_json::to_value(&i).unwrap();
    stored["lineage_status"] = serde_json::json!("root");
    conn.execute(
        "UPDATE execution_turns SET identity_json=?1 WHERE lane=?2 AND turn=?3",
        rusqlite::params![stored.to_string(), position.lane, position.turn],
    )
    .unwrap();
    assert_refused_without_effects(&mut f, i);
}

#[test]
fn another_tasks_initial_plan_refuses_selection_and_effects() {
    let mut f = Fixture::new();
    let i = identity("codex", "agent", "turn");
    let first = applied(f.outcome(prompt(i.clone())));
    let other = applied(f.outcome(prompt(identity("codex", "other", "turn"))));
    assert_ne!(first.task_id, other.task_id);
    let position = ExecutionPosition::from_identity(&i).unwrap();
    let conn = rusqlite::Connection::open(&f.cfg.ledger_path).unwrap();
    conn.execute(
        "UPDATE execution_turns SET initial_plan_id=?1 WHERE lane=?2 AND turn=?3",
        rusqlite::params![
            other.initial_plan_id.0.to_string(),
            position.lane,
            position.turn
        ],
    )
    .unwrap();
    assert_refused_without_effects(&mut f, i);
}

fn assert_refused_without_effects(f: &mut Fixture, i: ExecutionIdentity) {
    let tables = ["execution_replays", "plans", "receipts", "reservations"];
    let before: Vec<_> = tables.iter().map(|table| f.count(table)).collect();
    gap(f.outcome(owner(i.clone(), Op::Query {})), Gap::Ambiguous);
    gap(f.outcome(tool(i.clone(), "unapplied-call")), Gap::Ambiguous);
    gap(f.outcome(stop(i)), Gap::Ambiguous);
    assert_eq!(
        before,
        tables
            .iter()
            .map(|table| f.count(table))
            .collect::<Vec<_>>()
    );
}
