//! End-to-end HORO-1141 checks: drives real `Preflight`/`ToolInvoked`/
//! `Finalize` requests over a real socket against the real dispatch logic
//! ([`libra_governor_daemon::handle_connection`], the same function
//! `daemon run`'s accept loop calls — see `preflight_integration.rs` and
//! `replan_integration.rs` for the established pattern this file
//! follows), asserting the Completion Reserve and reservation ledger are
//! actually wired into admission, replan, and finalization — not just
//! exercised at the ledger-crate level.

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use libra_governor_daemon::{recon::ReconBudget, DaemonConfig};
use libra_governor_domain::{
    Admission, AutonomyBoundary, CompletionContract, CompletionCriterion, Confidence,
    ConstraintMode, ExecutionOutcome, ExecutionPlan, ExecutionReceipt, Policy, PolicyPresetInputs,
    ReplanHysteresisConfig, ReservationClass, ResourceAmount, ResourceBound, TaskIdentity,
    TimeBound,
};
use libra_governor_ledger::{LedgerStore, ReserveOutcome, ReserveRequest};
use libra_governor_protocol::{
    wire, FinalizeOutcome, ReplanState, Request, RequestEnvelope, Response, ResponseEnvelope,
    PROTOCOL_VERSION,
};

fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample_repo")
}

/// Drives one full request/response round trip against a live, already
/// listening daemon — identical helper to the one in
/// `replan_integration.rs`.
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

fn base_config(dir: &Path, policy: Policy) -> DaemonConfig {
    DaemonConfig {
        socket_path: dir.join("d.sock"),
        ledger_path: dir.join("ledger.sqlite3"),
        log_path: dir.join("daemon.log"),
        recon_budget: ReconBudget::default(),
        replan_hysteresis: ReplanHysteresisConfig::default(),
        policy,
        reservation_ttl_secs: 900,
        gateway: None,
        gateway_stats: std::sync::Arc::new(Default::default()),
        gateway_session_header: libra_governor_gateway::proxy::DEFAULT_SESSION_HEADER.to_string(),
    }
}

/// A generous [`Policy`] a normal task's projected requirement should
/// always be admitted under. Uses `min_confidence: Confidence::Low`
/// rather than `Policy::balanced`'s `Medium` deliberately: a cold-start
/// estimate (the honest, normal case for a fresh task with no local
/// receipt history — see `Estimate::cold_start` docs) always carries
/// `Confidence::Low`, so a `Medium` floor would Deny on confidence alone
/// and never actually exercise the resource/time admission path these
/// tests are for.
fn generous_policy() -> Policy {
    Policy::validated(
        "generous-test",
        ResourceBound {
            mode: ConstraintMode::Elastic,
            target: ResourceAmount::Tokens(100_000),
            elastic_ceiling: Some(ResourceAmount::Tokens(125_000)),
            hard_ceiling: ResourceAmount::Tokens(150_000),
        },
        TimeBound {
            mode: ConstraintMode::Elastic,
            target_secs: 3600,
            elastic_ceiling_secs: Some(4500),
            hard_ceiling_secs: Some(5400),
            deadline: None,
        },
        CompletionContract::first(vec![CompletionCriterion::required(
            "required verification (tests/build/lint) passes",
        )]),
        Confidence::Low,
        AutonomyBoundary::AskOnApproval,
    )
    .unwrap()
}

// --- (a) admission happy path -----------------------------------------

#[test]
fn admission_happy_path_initializes_a_budget_and_reserves_the_work_envelope() {
    let dir = tempfile::tempdir().unwrap();
    let config = base_config(dir.path(), generous_policy());
    let mut ledger = LedgerStore::open(&config.ledger_path).unwrap();
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
            session_id: "happy-path-session".to_string(),
        },
    ) {
        Response::Preflight(result) => *result,
        other => panic!("expected a Preflight response, got {other:?}"),
    };

    assert_eq!(
        preflight.admission.as_ref().map(|d| &d.admission),
        Some(&Admission::Admit),
        "a generous policy against a fresh task must admit cleanly"
    );
    let completion_reserve = preflight
        .completion_reserve
        .expect("completion_reserve must be populated on a normal preflight");
    assert!(
        completion_reserve.as_f64() > 0.0,
        "required completion work must have an explicit protected reserve"
    );

    let budget = ledger
        .task_budget(preflight.task_id)
        .unwrap()
        .expect("task_budgets row must exist after a preflight");
    assert_eq!(budget.completion_reserve, completion_reserve);
    assert!(budget.initial_completion_reserve.as_f64() > 0.0);

    let reservations = ledger.reservations_for_task(preflight.task_id).unwrap();
    let active_for_plan: Vec<_> = reservations
        .iter()
        .filter(|r| {
            r.plan_id == Some(preflight.plan_id)
                && r.state == libra_governor_domain::ReservationState::Active
        })
        .collect();
    assert_eq!(
        active_for_plan.len(),
        1,
        "exactly one active RequiredWork reservation must back the admitted plan"
    );
    assert_eq!(active_for_plan[0].class, ReservationClass::RequiredWork);
}

// --- (b) Deny path: confidence floor -----------------------------------

#[test]
fn admission_deny_writes_no_reservation_and_leaves_the_reserve_untouched() {
    let dir = tempfile::tempdir().unwrap();
    // strict_budget requires Confidence::High; a cold-start estimate is
    // always Confidence::Low, so this Denies deterministically on the
    // very first preflight without any manual ledger setup.
    let policy = Policy::strict_budget(PolicyPresetInputs {
        resource_target: ResourceAmount::Tokens(1000),
        time_target_secs: 600,
        quality_floor: CompletionContract::first(vec![CompletionCriterion::required(
            "required verification (tests/build/lint) passes",
        )]),
    })
    .unwrap();
    let config = base_config(dir.path(), policy);
    let mut ledger = LedgerStore::open(&config.ledger_path).unwrap();
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
            session_id: "deny-session".to_string(),
        },
    ) {
        Response::Preflight(result) => *result,
        other => panic!("expected a Preflight response, got {other:?}"),
    };

    assert!(
        matches!(
            preflight.admission.as_ref().map(|d| &d.admission),
            Some(Admission::Deny(_))
        ),
        "a cold-start estimate under a High-confidence-floor policy must Deny, got {:?}",
        preflight.admission
    );

    let reservations = ledger.reservations_for_task(preflight.task_id).unwrap();
    assert!(
        reservations.is_empty(),
        "a Deny decision must not write any reservation — protected capacity stays untouched"
    );
    let budget = ledger.task_budget(preflight.task_id).unwrap().unwrap();
    assert_eq!(
        budget.completion_reserve, budget.initial_completion_reserve,
        "nothing was reserved, so the completion reserve must be exactly its initial value"
    );
}

// --- (b) ApprovalRequired path: optional headroom already committed ----

#[test]
fn admission_approval_required_writes_no_reservation() {
    let dir = tempfile::tempdir().unwrap();
    // Approval mode with a small target and a generous hard ceiling: the
    // very first preflight's projected requirement always equals the
    // target exactly (no resource_p80 exists yet — cold start), which
    // Policy::evaluate treats as `Admit`. A SECOND, unrelated committed
    // reservation (simulating capacity already spoken for by other
    // in-flight work) pushes the next preflight's projection past the
    // target into the approval-required band, without exceeding the
    // hard ceiling.
    let policy = Policy::validated(
        "approval-test",
        ResourceBound {
            mode: ConstraintMode::Approval,
            target: ResourceAmount::Tokens(100),
            elastic_ceiling: None,
            hard_ceiling: ResourceAmount::Tokens(1_000_000),
        },
        TimeBound {
            mode: ConstraintMode::Approval,
            target_secs: 600,
            elastic_ceiling_secs: None,
            hard_ceiling_secs: None,
            deadline: None,
        },
        CompletionContract::first(vec![CompletionCriterion::required(
            "required verification (tests/build/lint) passes",
        )]),
        Confidence::Low,
        AutonomyBoundary::AskOnApproval,
    )
    .unwrap();
    let config = base_config(dir.path(), policy);
    let mut ledger = LedgerStore::open(&config.ledger_path).unwrap();
    let mut current_task = None;
    let listener = libra_governor_daemon::bind_or_detect_running(&config.socket_path).unwrap();
    let socket_path = dir.path().join("d.sock");

    let first = match send(
        &listener,
        &socket_path,
        &mut ledger,
        &mut current_task,
        &config,
        Request::Preflight {
            task_hint: "fix the login bug".to_string(),
            cwd: fixture_repo(),
            session_id: "approval-session".to_string(),
        },
    ) {
        Response::Preflight(result) => *result,
        other => panic!("expected a Preflight response, got {other:?}"),
    };
    assert_eq!(
        first.admission.as_ref().map(|d| &d.admission),
        Some(&Admission::Admit)
    );

    // An unrelated (no plan_id) committed reservation — not tied to
    // `first.plan_id`, so the next preflight's release-outgoing-plan step
    // will not touch it, and it stays committed to simulate other
    // in-flight work consuming this task's headroom.
    let now = time::OffsetDateTime::now_utc();
    let ReserveOutcome::Granted(_) = ledger
        .reserve(ReserveRequest {
            task_id: first.task_id,
            session_id: "approval-session",
            plan_id: None,
            class: ReservationClass::OptionalWork,
            amount: ResourceAmount::Tokens(150),
            idempotency_key: "unrelated-committed-work",
            now,
            ttl_secs: 900,
        })
        .unwrap()
    else {
        panic!("test setup: the unrelated reservation must be grantable");
    };

    let second = match send(
        &listener,
        &socket_path,
        &mut ledger,
        &mut current_task,
        &config,
        Request::Preflight {
            task_hint: "fix the login bug, part two".to_string(),
            cwd: fixture_repo(),
            session_id: "approval-session".to_string(),
        },
    ) {
        Response::Preflight(result) => *result,
        other => panic!("expected a Preflight response, got {other:?}"),
    };

    assert!(
        matches!(
            second.admission.as_ref().map(|d| &d.admission),
            Some(Admission::ApprovalRequired(_))
        ),
        "committed capacity pushing the projection past target (but under the hard ceiling) \
         must require approval, got {:?}",
        second.admission
    );

    let reservations_for_second_plan: Vec<_> = ledger
        .reservations_for_task(second.task_id)
        .unwrap()
        .into_iter()
        .filter(|r| r.plan_id == Some(second.plan_id))
        .collect();
    assert!(
        reservations_for_second_plan.is_empty(),
        "optional work must be denied/approval-gated before it can consume protected completion \
         resources — no reservation may be written for an ApprovalRequired plan"
    );
}

// --- (c) replan recomputes and re-reserves ------------------------------

/// Seeds `MIN_CLASS_SAMPLES` (5) same-repo receipts so a `Preflight`
/// against the fixture repo lands on a real, non-cold-start estimate —
/// same fixture-seeding shape as `replan_integration.rs`.
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

#[test]
fn a_replan_releases_the_old_plans_reservation_and_reserves_the_new_one() {
    let dir = tempfile::tempdir().unwrap();
    let config = base_config(dir.path(), generous_policy());
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
            session_id: "replan-reserve-session".to_string(),
        },
    ) {
        Response::Preflight(result) => *result,
        other => panic!("expected a Preflight response, got {other:?}"),
    };
    let original_plan_id = preflight.plan_id;

    // Seeded history gives a bucket-typical tool-call count of 3, so the
    // material-deviation threshold is `> 2 * 3 = 6`.
    send_distinct_tool_calls(
        &listener,
        &socket_path,
        &mut ledger,
        &mut current_task,
        &config,
        "replan-reserve-session",
        7,
    );

    let summary = current_task
        .clone()
        .expect("a replan must still leave a current task");
    assert_eq!(summary.replan_state, ReplanState::Replanned { count: 1 });
    assert_ne!(summary.plan_id, original_plan_id);

    let reservations = ledger.reservations_for_task(summary.task_id).unwrap();
    let old_plan_reservations: Vec<_> = reservations
        .iter()
        .filter(|r| r.plan_id == Some(original_plan_id))
        .collect();
    assert!(
        !old_plan_reservations.is_empty(),
        "test setup: the original plan must have been reserved against"
    );
    assert!(
        old_plan_reservations
            .iter()
            .all(|r| r.state == libra_governor_domain::ReservationState::Released),
        "every reservation tied to the superseded plan must be released, not left active"
    );

    let new_plan_active: Vec<_> = reservations
        .iter()
        .filter(|r| {
            r.plan_id == Some(summary.plan_id)
                && r.state == libra_governor_domain::ReservationState::Active
        })
        .collect();
    assert_eq!(
        new_plan_active.len(),
        1,
        "the replanned plan must have exactly one active reservation backing it"
    );
}

// --- (d) finalize settles and records receipt evidence ------------------

#[test]
fn finalize_settles_active_reservations_and_records_receipt_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let config = base_config(dir.path(), generous_policy());
    let mut ledger = LedgerStore::open(&config.ledger_path).unwrap();
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
            session_id: "finalize-session".to_string(),
        },
    ) {
        Response::Preflight(result) => *result,
        other => panic!("expected a Preflight response, got {other:?}"),
    };
    assert_eq!(
        preflight.admission.as_ref().map(|d| &d.admission),
        Some(&Admission::Admit)
    );

    let outcome = send(
        &listener,
        &socket_path,
        &mut ledger,
        &mut current_task,
        &config,
        Request::Finalize {
            session_id: "finalize-session".to_string(),
            model: None,
        },
    );
    let Response::Finalize(FinalizeOutcome::Finalized(result)) = outcome else {
        panic!("expected a Finalized outcome, got {outcome:?}");
    };

    let evidence = result
        .receipt
        .reservations
        .expect("a preflighted-and-finalized task must carry reservation evidence");
    assert_eq!(evidence.reservation_count, 1);
    assert_eq!(
        evidence.usage_known_count, 0,
        "no real usage figure is ever reported today"
    );
    assert_eq!(
        evidence.settled_total, evidence.reserved_total,
        "with no reported usage, settlement falls back to the reserved amount exactly"
    );
    assert_eq!(
        evidence.completion_reserve_remaining, evidence.completion_reserve_initial,
        "a fully-settled-at-reserved-amount reservation must not have drawn on the reserve at all"
    );

    let reservations = ledger.reservations_for_task(preflight.task_id).unwrap();
    assert!(
        reservations
            .iter()
            .all(|r| r.state == libra_governor_domain::ReservationState::Settled),
        "finalize must settle every reservation tied to the finalized plan"
    );
}

// --- (e) determinism without MCP/LLM behavior ---------------------------

#[test]
fn identical_preflights_produce_identical_admission_and_reserve_decisions() {
    let policy = generous_policy();

    let run = |session_id: &str| -> (Admission, ResourceAmount) {
        let dir = tempfile::tempdir().unwrap();
        let config = base_config(dir.path(), policy.clone());
        let mut ledger = LedgerStore::open(&config.ledger_path).unwrap();
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
                session_id: session_id.to_string(),
            },
        ) {
            Response::Preflight(result) => *result,
            other => panic!("expected a Preflight response, got {other:?}"),
        };
        (
            preflight
                .admission
                .expect("admission must be present")
                .admission,
            preflight
                .completion_reserve
                .expect("completion_reserve must be present"),
        )
    };

    let (admission_a, reserve_a) = run("determinism-session-a");
    let (admission_b, reserve_b) = run("determinism-session-b");

    assert_eq!(
        admission_a, admission_b,
        "identical fresh-task inputs against the same policy must produce the same admission \
         decision — no MCP/LLM call influences this path"
    );
    assert_eq!(
        reserve_a, reserve_b,
        "the Completion Reserve computation is a pure function of contract/estimate/policy"
    );
}

// --- (f) HORO-1146 gate finding #2: replan must not reserve for a task
//         whose original admission was Denied ---------------------------

#[test]
fn a_material_event_replan_does_not_reserve_capacity_for_a_task_denied_at_admission() {
    // Reproduces the exact HORO-1146 gate scenario 5/9 finding: the
    // initial preflight is Denied (a real Deny, not confidence-based —
    // `strict_budget` requires Confidence::High and a cold-start estimate
    // is always Confidence::Low, so this Denies deterministically on the
    // very first preflight, the same mechanism
    // `admission_deny_writes_no_reservation_and_leaves_the_reserve_untouched`
    // above uses), then a `PossibleToolLoop` material-event replan fires
    // for that same task. Before the HORO-1146 fix, the replan path
    // wrote a real, active `ReservationClass::RequiredWork` reservation
    // for the denied task regardless.
    let dir = tempfile::tempdir().unwrap();
    let policy = Policy::strict_budget(PolicyPresetInputs {
        resource_target: ResourceAmount::Tokens(1000),
        time_target_secs: 600,
        quality_floor: CompletionContract::first(vec![CompletionCriterion::required(
            "required verification (tests/build/lint) passes",
        )]),
    })
    .unwrap();
    let config = base_config(dir.path(), policy);
    let mut ledger = LedgerStore::open(&config.ledger_path).unwrap();
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
            session_id: "denied-replan-session".to_string(),
        },
    ) {
        Response::Preflight(result) => *result,
        other => panic!("expected a Preflight response, got {other:?}"),
    };
    assert!(
        matches!(
            preflight.admission.as_ref().map(|d| &d.admission),
            Some(Admission::Deny(_))
        ),
        "a cold-start estimate under strict_budget must Deny, got {:?}",
        preflight.admission
    );
    assert!(
        ledger
            .reservations_for_task(preflight.task_id)
            .unwrap()
            .is_empty(),
        "no reservation should exist yet — the original preflight was Denied"
    );

    // 4 consecutive invocations of the SAME tool crosses the
    // `PossibleToolLoop` streak threshold and triggers a real replan —
    // same mechanism as
    // `possible_tool_loop_streak_triggers_a_replan_before_the_count_threshold`
    // in `replan_integration.rs`.
    for _ in 0..4 {
        let response = send(
            &listener,
            &socket_path,
            &mut ledger,
            &mut current_task,
            &config,
            Request::ToolInvoked {
                session_id: "denied-replan-session".to_string(),
                tool_name: "Bash".to_string(),
            },
        );
        assert_eq!(response, Response::Ack);
    }

    let status = match send(
        &listener,
        &socket_path,
        &mut ledger,
        &mut current_task,
        &config,
        Request::Status,
    ) {
        Response::Status(status) => status,
        other => panic!("expected a Status response, got {other:?}"),
    };
    let summary = status
        .current_task
        .expect("a task was preflighted; Status must reflect it");
    assert_eq!(
        summary.replan_state,
        ReplanState::Replanned { count: 1 },
        "the material-event replan must still fire for a denied task — hooks are \
         advisory-only (ADR 0001), so work still proceeds after a Deny"
    );

    let reservations = ledger.reservations_for_task(preflight.task_id).unwrap();
    let required_work: Vec<_> = reservations
        .iter()
        .filter(|r| r.class == ReservationClass::RequiredWork)
        .collect();
    assert!(
        required_work.is_empty(),
        "the replan must not reserve capacity for a task whose original admission was \
         Denied, but found: {required_work:?}"
    );
}
