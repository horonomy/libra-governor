//! The gateway's integration with the REAL ledger and the REAL admission
//! policy (HORO-1144).
//!
//! `crates/gateway`'s own suites drive the proxy against a stub
//! `SpendAuthority`, because that crate cannot depend on the ledger — ADR
//! 0003 §2's whole point. This file closes the other half: it exercises
//! `LedgerSpendAuthority` against a real SQLite ledger and a real
//! `Policy`, and it checks the three integration properties that only
//! exist once both halves are present:
//!
//! 1. Gateway reservations are `OptionalWork` with no plan id, so they
//!    can never draw the protected Completion Reserve and a replan cannot
//!    release one out from under a live request.
//! 2. With the gateway enabled, `Preflight` and the replan path reserve
//!    **no** plan-level envelope — per-request reservations replace it
//!    rather than stacking with it.
//! 3. A hard budget refusal actually happens at the ledger, not merely at
//!    the policy projection.

use std::io::BufReader;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use libra_governor_daemon::gateway_authority::{
    open_gateway_ledger, LedgerRequestRecorder, LedgerSpendAuthority,
};
use libra_governor_daemon::{recon::ReconBudget, DaemonConfig};
use libra_governor_domain::{
    AutonomyBoundary, CompletionContract, CompletionCriterion, Confidence, ConstraintMode, Policy,
    ReplanHysteresisConfig, ReservationClass, ReservationState, ResourceAmount, ResourceBound,
    TimeBound,
};
use libra_governor_gateway::authority::{SpendAuthority, SpendDecision, SpendDenial, SpendRequest};
use libra_governor_gateway::config::{GatewayConfig, GatewayCredentialMode};
use libra_governor_gateway::credential::CredentialCommand;
use libra_governor_gateway::proxy::{GatewayRequestRecord, RequestRecorder};
use libra_governor_ledger::LedgerStore;
use libra_governor_protocol::{
    wire, Request, RequestEnvelope, Response, ResponseEnvelope, PROTOCOL_VERSION,
};

fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample_repo")
}

fn send(
    listener: &UnixListener,
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

/// A gateway configuration that validates but is never actually started
/// by these tests — they drive `LedgerSpendAuthority` directly. Its only
/// job is to make `config.gateway.is_some()` true so the mutual-exclusion
/// guard in the daemon's reserve sites is exercised.
fn gateway_config(dir: &Path) -> GatewayConfig {
    GatewayConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        dir.join("gateway.token"),
        GatewayCredentialMode::GovernorHeld {
            credential: CredentialCommand::new(
                "/bin/sh",
                vec!["-c".to_string(), "printf 'sk-fake-test-key'".to_string()],
            ),
        },
    )
}

fn config(dir: &Path, policy: Policy, with_gateway: bool) -> DaemonConfig {
    DaemonConfig {
        socket_path: dir.join("d.sock"),
        ledger_path: dir.join("ledger.sqlite3"),
        log_path: dir.join("daemon.log"),
        recon_budget: ReconBudget::default(),
        replan_hysteresis: ReplanHysteresisConfig::default(),
        policy,
        reservation_ttl_secs: 900,
        gateway: with_gateway.then(|| gateway_config(dir)),
        gateway_stats: Arc::new(Default::default()),
        gateway_session_header: libra_governor_gateway::proxy::DEFAULT_SESSION_HEADER.to_string(),
        extensions: None,
        extension_runtime: std::sync::OnceLock::new(),
    }
}

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

/// A tiny hard budget, so one modest gateway request exhausts it.
fn tiny_policy() -> Policy {
    Policy::validated(
        "tiny-test",
        ResourceBound {
            mode: ConstraintMode::Hard,
            target: ResourceAmount::Tokens(1_000),
            elastic_ceiling: None,
            hard_ceiling: ResourceAmount::Tokens(1_000),
        },
        TimeBound {
            mode: ConstraintMode::Elastic,
            target_secs: 3600,
            elastic_ceiling_secs: Some(4500),
            hard_ceiling_secs: Some(5400),
            deadline: None,
        },
        CompletionContract::first(vec![CompletionCriterion::required("verification passes")]),
        Confidence::Low,
        AutonomyBoundary::AskOnApproval,
    )
    .unwrap()
}

/// Runs one `Preflight` so the session has a task and an initialized
/// budget, then returns a ledger-backed authority over the same file.
struct Fixture {
    _dir: tempfile::TempDir,
    authority: LedgerSpendAuthority,
    gateway_ledger: Arc<Mutex<LedgerStore>>,
    session_id: String,
}

fn preflighted(policy: Policy, with_gateway: bool) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let daemon_config = config(dir.path(), policy, with_gateway);
    let listener = UnixListener::bind(&daemon_config.socket_path).unwrap();
    let mut ledger = LedgerStore::open(&daemon_config.ledger_path).unwrap();
    let mut current_task = None;
    let session_id = "gw-session-1".to_string();

    send(
        &listener,
        &daemon_config.socket_path,
        &mut ledger,
        &mut current_task,
        &daemon_config,
        Request::Preflight {
            task_hint: "add a login endpoint".to_string(),
            cwd: fixture_repo(),
            session_id: session_id.clone(),
        },
    );
    drop(ledger);

    let gateway_ledger = open_gateway_ledger(&daemon_config.ledger_path).unwrap();
    Fixture {
        authority: LedgerSpendAuthority::new(Arc::clone(&gateway_ledger)),
        gateway_ledger,
        _dir: dir,
        session_id,
    }
}

#[test]
fn a_gateway_reservation_is_optional_work_with_no_plan_id() {
    let fixture = preflighted(generous_policy(), true);
    let context = fixture
        .authority
        .budget_context(&fixture.session_id)
        .unwrap()
        .expect("the preflighted session resolves to a task with a budget");

    let decision = fixture
        .authority
        .authorize(SpendRequest {
            task_id: context.task_id,
            session_id: &fixture.session_id,
            amount: ResourceAmount::Tokens(5_000),
            idempotency_key: "gw:test-1",
            ttl_secs: 600,
        })
        .unwrap();
    assert!(matches!(decision, SpendDecision::Granted { .. }));

    let ledger = fixture.gateway_ledger.lock().unwrap();
    let reservations = ledger.reservations_for_task(context.task_id).unwrap();
    let gateway_reservation = reservations
        .iter()
        .find(|r| r.idempotency_key == "gw:test-1")
        .expect("the gateway's reservation row");

    assert_eq!(
        gateway_reservation.class,
        ReservationClass::OptionalWork,
        "the gateway cannot tell required from optional work from an HTTP request, so it must \
         take the weaker claim and never draw the Completion Reserve"
    );
    assert_eq!(
        gateway_reservation.plan_id, None,
        "a plan id would let a replan's release_active_for_plan free this reservation out from \
         under a live in-flight request"
    );
    assert_eq!(
        gateway_reservation.drawn_from_reserve,
        ResourceAmount::Tokens(0),
        "optional work can never draw against the protected reserve"
    );
}

#[test]
fn a_replan_never_releases_an_in_flight_gateway_reservation() {
    let fixture = preflighted(generous_policy(), true);
    let context = fixture
        .authority
        .budget_context(&fixture.session_id)
        .unwrap()
        .unwrap();
    let SpendDecision::Granted { reservation_id, .. } = fixture
        .authority
        .authorize(SpendRequest {
            task_id: context.task_id,
            session_id: &fixture.session_id,
            amount: ResourceAmount::Tokens(5_000),
            idempotency_key: "gw:in-flight",
            ttl_secs: 600,
        })
        .unwrap()
    else {
        panic!("the reservation must be granted");
    };

    // Release every reservation the in-flight plan owns — exactly what
    // `handle_tool_invoked`'s replan path does.
    {
        let mut ledger = fixture.gateway_ledger.lock().unwrap();
        let plan_ids: Vec<_> = ledger
            .reservations_for_task(context.task_id)
            .unwrap()
            .into_iter()
            .filter_map(|r| r.plan_id)
            .collect();
        for plan_id in plan_ids {
            ledger
                .release_active_for_plan(context.task_id, plan_id, time::OffsetDateTime::now_utc())
                .unwrap();
        }
    }

    let ledger = fixture.gateway_ledger.lock().unwrap();
    let still_active = ledger
        .reservations_for_task(context.task_id)
        .unwrap()
        .into_iter()
        .find(|r| r.id == reservation_id)
        .unwrap();
    assert_eq!(
        still_active.state,
        ReservationState::Active,
        "a replan firing mid-request must not release the live request's reservation — the \
         later settle would hit AlreadyFinal and the spend would vanish silently"
    );
}

#[test]
fn enabling_the_gateway_suppresses_the_plan_level_work_envelope() {
    let with_gateway = preflighted(generous_policy(), true);
    let context = with_gateway
        .authority
        .budget_context(&with_gateway.session_id)
        .unwrap()
        .unwrap();
    let plan_reservations = with_gateway
        .gateway_ledger
        .lock()
        .unwrap()
        .reservations_for_task(context.task_id)
        .unwrap();
    assert!(
        plan_reservations.is_empty(),
        "with the gateway on, per-request reservations REPLACE the plan envelope; reserving \
         both would double-count the same spend against one hard_limit"
    );

    // And without it, the HORO-1141 behaviour is unchanged.
    let without_gateway = preflighted(generous_policy(), false);
    let context = without_gateway
        .authority
        .budget_context(&without_gateway.session_id)
        .unwrap()
        .unwrap();
    let plan_reservations = without_gateway
        .gateway_ledger
        .lock()
        .unwrap()
        .reservations_for_task(context.task_id)
        .unwrap();
    assert_eq!(
        plan_reservations.len(),
        1,
        "with no gateway, the plan-level envelope is still the instrument that holds capacity"
    );
    assert_eq!(plan_reservations[0].class, ReservationClass::RequiredWork);
}

#[test]
fn a_request_beyond_the_hard_budget_is_refused_by_the_real_ledger() {
    let fixture = preflighted(tiny_policy(), true);
    let context = fixture
        .authority
        .budget_context(&fixture.session_id)
        .unwrap()
        .unwrap();

    let decision = fixture
        .authority
        .authorize(SpendRequest {
            task_id: context.task_id,
            session_id: &fixture.session_id,
            amount: ResourceAmount::Tokens(5_000),
            idempotency_key: "gw:too-big",
            ttl_secs: 600,
        })
        .unwrap();

    match decision {
        SpendDecision::Denied(SpendDenial::PolicyDenied { detail }) => {
            assert!(detail.contains("ResourceExceedsHardCeiling"), "{detail}");
        }
        SpendDecision::Denied(SpendDenial::Insufficient { .. }) => {}
        other => panic!("a request past the hard ceiling must be refused, got {other:?}"),
    }

    let ledger = fixture.gateway_ledger.lock().unwrap();
    assert!(
        ledger
            .reservations_for_task(context.task_id)
            .unwrap()
            .iter()
            .all(|r| r.idempotency_key != "gw:too-big"),
        "a refused request must leave no reservation row holding capacity"
    );
}

#[test]
fn the_completion_reserve_is_never_drawn_by_gateway_traffic() {
    let fixture = preflighted(tiny_policy(), true);
    let context = fixture
        .authority
        .budget_context(&fixture.session_id)
        .unwrap()
        .unwrap();
    let reserve_before = fixture
        .gateway_ledger
        .lock()
        .unwrap()
        .task_budget(context.task_id)
        .unwrap()
        .unwrap()
        .completion_reserve;
    assert!(reserve_before.as_f64() > 0.0);

    // Keep asking until the budget refuses. Whatever happens, the
    // protected reserve must be untouched at the end.
    for i in 0..20 {
        let _ = fixture.authority.authorize(SpendRequest {
            task_id: context.task_id,
            session_id: &fixture.session_id,
            amount: ResourceAmount::Tokens(100),
            idempotency_key: &format!("gw:drain-{i}"),
            ttl_secs: 600,
        });
    }

    let reserve_after = fixture
        .gateway_ledger
        .lock()
        .unwrap()
        .task_budget(context.task_id)
        .unwrap()
        .unwrap()
        .completion_reserve;
    assert_eq!(
        reserve_after, reserve_before,
        "gateway traffic exhausting the budget must leave the Completion Reserve intact — that \
         is the whole point of classifying it OptionalWork"
    );
}

#[test]
fn settlement_refunds_the_unspent_remainder_of_a_gateway_reservation() {
    let fixture = preflighted(generous_policy(), true);
    let context = fixture
        .authority
        .budget_context(&fixture.session_id)
        .unwrap()
        .unwrap();
    let SpendDecision::Granted { reservation_id, .. } = fixture
        .authority
        .authorize(SpendRequest {
            task_id: context.task_id,
            session_id: &fixture.session_id,
            amount: ResourceAmount::Tokens(5_000),
            idempotency_key: "gw:settle-me",
            ttl_secs: 600,
        })
        .unwrap()
    else {
        panic!("granted");
    };

    fixture
        .authority
        .settle(reservation_id, Some(ResourceAmount::Tokens(800)))
        .unwrap();

    let ledger = fixture.gateway_ledger.lock().unwrap();
    let settled = ledger
        .reservations_for_task(context.task_id)
        .unwrap()
        .into_iter()
        .find(|r| r.id == reservation_id)
        .unwrap();
    assert_eq!(settled.state, ReservationState::Settled);
    assert_eq!(settled.settled_amount, Some(ResourceAmount::Tokens(800)));
    assert_eq!(settled.usage_known, Some(true));
    assert_eq!(settled.refunded(), Some(ResourceAmount::Tokens(4_200)));
}

#[test]
fn settling_without_a_usage_figure_charges_the_full_reserved_amount() {
    let fixture = preflighted(generous_policy(), true);
    let context = fixture
        .authority
        .budget_context(&fixture.session_id)
        .unwrap()
        .unwrap();
    let SpendDecision::Granted { reservation_id, .. } = fixture
        .authority
        .authorize(SpendRequest {
            task_id: context.task_id,
            session_id: &fixture.session_id,
            amount: ResourceAmount::Tokens(5_000),
            idempotency_key: "gw:no-usage",
            ttl_secs: 600,
        })
        .unwrap()
    else {
        panic!("granted");
    };

    fixture.authority.settle(reservation_id, None).unwrap();

    let ledger = fixture.gateway_ledger.lock().unwrap();
    let settled = ledger
        .reservations_for_task(context.task_id)
        .unwrap()
        .into_iter()
        .find(|r| r.id == reservation_id)
        .unwrap();
    assert_eq!(settled.settled_amount, Some(ResourceAmount::Tokens(5_000)));
    assert_eq!(
        settled.usage_known,
        Some(false),
        "nothing may be refunded that cannot be proven unspent"
    );
}

#[test]
fn settling_the_same_reservation_twice_does_not_double_charge() {
    let fixture = preflighted(generous_policy(), true);
    let context = fixture
        .authority
        .budget_context(&fixture.session_id)
        .unwrap()
        .unwrap();
    let SpendDecision::Granted { reservation_id, .. } = fixture
        .authority
        .authorize(SpendRequest {
            task_id: context.task_id,
            session_id: &fixture.session_id,
            amount: ResourceAmount::Tokens(5_000),
            idempotency_key: "gw:double-settle",
            ttl_secs: 600,
        })
        .unwrap()
    else {
        panic!("granted");
    };

    fixture
        .authority
        .settle(reservation_id, Some(ResourceAmount::Tokens(800)))
        .unwrap();
    fixture
        .authority
        .settle(reservation_id, Some(ResourceAmount::Tokens(800)))
        .unwrap();

    let ledger = fixture.gateway_ledger.lock().unwrap();
    let rows: Vec<_> = ledger
        .reservations_for_task(context.task_id)
        .unwrap()
        .into_iter()
        .filter(|r| r.id == reservation_id)
        .collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].settled_amount, Some(ResourceAmount::Tokens(800)));
}

#[test]
fn a_gateway_request_row_is_written_and_carries_no_body_or_credential() {
    let fixture = preflighted(generous_policy(), true);
    let context = fixture
        .authority
        .budget_context(&fixture.session_id)
        .unwrap()
        .unwrap();
    let log_path = std::env::temp_dir().join("libra-gateway-recorder-test.log");
    let recorder =
        LedgerRequestRecorder::new(Arc::clone(&fixture.gateway_ledger), log_path.clone());

    let mut record = GatewayRequestRecord {
        id: "req-integration-1".to_string(),
        task_id: Some(context.task_id),
        session_id: Some(fixture.session_id.clone()),
        route: Some("/v1/messages"),
        model: Some("claude-sonnet-4-5".to_string()),
        tier: "gateway_metered",
        decision: "allowed",
        decision_detail: None,
        reservation_id: None,
        reserved_amount: Some(ResourceAmount::Tokens(2_000)),
        settled_amount: Some(ResourceAmount::Tokens(150)),
        resource_kind: Some(libra_governor_domain::ResourceKind::Tokens),
        usage_known: true,
        input_tokens: Some(120),
        cache_creation_input_tokens: Some(0),
        cache_read_input_tokens: Some(0),
        output_tokens: Some(30),
        max_tokens: Some(1_000),
        bound_violated: false,
        pricing_version: libra_governor_gateway::pricing::PRICING_VERSION,
        upstream_status: Some(200),
        terminal_state: "completed_cleanly",
    };
    recorder.record(record.clone());

    // A second, refused request with no task at all — the row an auditor
    // most wants, and the one a NOT NULL / FK'd task_id would make
    // un-insertable.
    record.id = "req-integration-2".to_string();
    record.task_id = None;
    record.session_id = None;
    record.decision = "task_unbound";
    record.settled_amount = None;
    record.output_tokens = None;
    record.terminal_state = "rejected_before_upstream";
    recorder.record(record);

    let ledger = fixture.gateway_ledger.lock().unwrap();
    assert_eq!(
        ledger
            .gateway_request_decision("req-integration-1")
            .unwrap(),
        Some("allowed".to_string())
    );
    assert_eq!(
        ledger
            .gateway_request_decision("req-integration-2")
            .unwrap(),
        Some("task_unbound".to_string()),
        "a taskless refusal must still produce a provenance row"
    );
    let (count, total) = ledger.gateway_spend_for_task(context.task_id).unwrap();
    assert_eq!(count, 1);
    assert_eq!(total, Some(ResourceAmount::Tokens(150)));
}
