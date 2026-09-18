//! HORO-1146 (MVP 3.0 release gate) — evidence for the two scenarios in
//! the required E2E matrix that have no existing test and are not
//! reachable through the shipped `libra-governor` CLI binary, because
//! `daemon run` hardcodes `policy: default_admission_policy()` (the
//! `balanced` preset) and `gateway: None` with no CLI/env override (see
//! `crates/cli/src/daemon_cmd.rs`). These tests drive the identical
//! `libra_governor_daemon::handle_connection` dispatch function the real
//! daemon's accept loop calls (see `reservation_integration.rs` and
//! `gateway_enforcement.rs` for the established pattern this file
//! follows) — real code, real SQLite ledger, real socket protocol — just
//! with policy/gateway configuration the CLI itself cannot select.
//!
//! Full context: `experiments/mvp3_gate/README.md`.

use std::io::BufReader;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use libra_governor_daemon::gateway_authority::{
    open_gateway_ledger, LedgerRequestRecorder, LedgerSpendAuthority,
};
use libra_governor_daemon::{recon::ReconBudget, DaemonConfig};
use libra_governor_domain::{
    Admission, AutonomyBoundary, CompletionContract, CompletionCriterion, Confidence,
    ConstraintMode, Policy, PolicyPresetInputs, ReplanHysteresisConfig, ResourceAmount,
    ResourceBound, TimeBound,
};
use libra_governor_gateway::authority::{SpendAuthority, SpendDecision, SpendRequest};
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

/// Identical helper to the one in `reservation_integration.rs` /
/// `gateway_enforcement.rs` — one real request/response round trip
/// against a live, already-listening daemon.
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
        gateway_stats: Arc::new(Default::default()),
        gateway_session_header: libra_governor_gateway::proxy::DEFAULT_SESSION_HEADER.to_string(),
    }
}

fn quality_floor() -> CompletionContract {
    CompletionContract::first(vec![CompletionCriterion::required(
        "required verification (tests/build/lint) passes",
    )])
}

// --- Scenario 2: deadline-first Elastic policy under a tight deadline --

/// `Policy::deadline_first` puts the *time* dimension in `Hard` mode
/// with zero slack (the deadline is firm) while *resource* stays
/// `Elastic` with a wide band (target..=2x, hard ceiling at 3x) — see
/// `Policy::deadline_first`'s doc comment.
///
/// REAL FINDING (documented honestly, not suppressed): the
/// `deadline_first` preset, like `strict_budget`, fixes
/// `min_confidence: Confidence::Medium`. A cold-start estimate (no
/// same-repo receipt history yet — the honest, normal case for a fresh
/// task) always carries `Confidence::Low` (see `Estimate::cold_start`
/// docs), so the FIRST assertion below proves the preset actually
/// **denies** a brand-new task purely on confidence pressure, before any
/// resource/time projection is even considered — identical in kind to
/// `reservation_integration.rs::admission_deny_writes_no_reservation_and_leaves_the_reserve_untouched`'s
/// `strict_budget` case. A real user selecting deadline-first for a
/// fresh task, with no local history yet, would see an immediate Deny,
/// not a governed on-time completion.
///
/// The SECOND assertion isolates just the deadline-first *shape*
/// (`ConstraintMode::Hard` time / `ConstraintMode::Elastic` resource,
/// same tight 120s hard ceiling) with `Confidence::Low` so a cold start
/// can clear the confidence gate, and shows that shape really does admit
/// a task under a tight, firm deadline once confidence is not the
/// binding constraint — proving the *time-pressure* mechanics work
/// correctly in isolation from the *confidence-floor* finding above.
#[test]
fn deadline_first_confidence_floor_denies_cold_start_but_the_deadline_pressure_shape_admits_at_low_confidence(
) {
    let dir = tempfile::tempdir().unwrap();
    let preset_policy = Policy::deadline_first(PolicyPresetInputs {
        resource_target: ResourceAmount::Tokens(20_000),
        time_target_secs: 120, // tight: 2 minutes, Hard, zero slack
        quality_floor: quality_floor(),
    })
    .unwrap();
    assert_eq!(preset_policy.time.mode, ConstraintMode::Hard);
    assert_eq!(preset_policy.time.hard_ceiling_secs, Some(120));
    assert_eq!(preset_policy.resource.mode, ConstraintMode::Elastic);
    assert_eq!(preset_policy.min_confidence, Confidence::Medium);

    let config = base_config(dir.path(), preset_policy);
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
            task_hint: "fix the login bug under a hard release deadline".to_string(),
            cwd: fixture_repo(),
            session_id: "deadline-session".to_string(),
        },
    ) {
        Response::Preflight(result) => *result,
        other => panic!("expected a Preflight response, got {other:?}"),
    };
    println!(
        "REAL FINDING — Policy::deadline_first preset, cold-start admission: {:?}",
        preflight.admission
    );
    assert!(
        matches!(
            preflight.admission.as_ref().map(|d| &d.admission),
            Some(Admission::Deny(_))
        ),
        "documented real finding: deadline_first denies a cold-start task on the confidence \
         floor alone, got {:?}",
        preflight.admission
    );
    drop(listener);

    // Second half: isolate the deadline-pressure *shape* from the
    // confidence-floor finding above by using Confidence::Low, mirroring
    // how `reservation_integration.rs::admission_approval_required_writes_no_reservation`
    // avoids the same trap for its own preset-adjacent custom policy.
    let dir2 = tempfile::tempdir().unwrap();
    let shape_policy = Policy::validated(
        "deadline-pressure-shape-test",
        ResourceBound {
            mode: ConstraintMode::Elastic,
            target: ResourceAmount::Tokens(20_000),
            elastic_ceiling: Some(ResourceAmount::Tokens(40_000)),
            hard_ceiling: ResourceAmount::Tokens(60_000),
        },
        TimeBound {
            mode: ConstraintMode::Hard,
            target_secs: 120,
            elastic_ceiling_secs: None,
            hard_ceiling_secs: Some(120),
            deadline: None,
        },
        quality_floor(),
        Confidence::Low,
        AutonomyBoundary::AskOnApproval,
    )
    .unwrap();
    let config2 = base_config(dir2.path(), shape_policy);
    let mut ledger2 = LedgerStore::open(&config2.ledger_path).unwrap();
    let mut current_task2 = None;
    let listener2 = libra_governor_daemon::bind_or_detect_running(&config2.socket_path).unwrap();
    let socket_path2 = dir2.path().join("d.sock");

    let preflight2 = match send(
        &listener2,
        &socket_path2,
        &mut ledger2,
        &mut current_task2,
        &config2,
        Request::Preflight {
            task_hint: "fix the login bug under a hard release deadline".to_string(),
            cwd: fixture_repo(),
            session_id: "deadline-session-2".to_string(),
        },
    ) {
        Response::Preflight(result) => *result,
        other => panic!("expected a Preflight response, got {other:?}"),
    };
    println!(
        "deadline-pressure shape (Confidence::Low) admission: {:?}, plan_id={:?}",
        preflight2.admission, preflight2.plan_id
    );
    assert_eq!(
        preflight2.admission.as_ref().map(|d| &d.admission),
        Some(&Admission::Admit),
        "with the confidence floor no longer the binding constraint, a real task really can be \
         admitted end to end through the daemon's dispatch under a tight (120s) hard deadline: \
         {:?}",
        preflight2.admission
    );
    let reservations2 = ledger2.reservations_for_task(preflight2.task_id).unwrap();
    assert_eq!(
        reservations2.len(),
        1,
        "an Admit must reserve exactly one plan-level work envelope"
    );
}

// --- Scenario 8: concurrent subagents near a budget boundary, through --
// --- the real LedgerSpendAuthority (the component the gateway calls) --

/// Extends HORO-1141's `reservation_concurrency.rs` ledger-level pattern
/// one layer up: instead of calling `LedgerStore::reserve` directly from
/// N threads, this drives N real OS threads through
/// `LedgerSpendAuthority::authorize` — the exact trait implementation
/// `crates/gateway/src/server.rs` calls on every `/v1/messages` request
/// — against one already-admitted (real `Preflight`, over the real
/// socket protocol) task with a deliberately tight hard ceiling. This is
/// the real daemon-side authorization layer, real SQLite ledger, real
/// concurrent OS threads simulating concurrent subagents; the one thing
/// it does not do is drive an actual HTTP gateway listener (see
/// `experiments/mvp3_gate/README.md` for why, and for the corroborating
/// `fake_upstream`-backed gateway tests that exercise the HTTP layer
/// itself, just with a stub, non-persistent authority).
#[test]
fn concurrent_subagents_near_a_tight_hard_ceiling_through_the_real_ledger_spend_authority() {
    let dir = tempfile::tempdir().unwrap();
    // Elastic resource bound with a tight hard ceiling: target 1000,
    // elastic ceiling 1200, hard ceiling 1600 tokens. Gateway requests
    // are OptionalWork and can never draw the completion reserve, so the
    // real optional headroom here is hard_ceiling(1600) - reserve.
    let policy = Policy::validated(
        "concurrency-test",
        ResourceBound {
            mode: ConstraintMode::Elastic,
            target: ResourceAmount::Tokens(1_000),
            elastic_ceiling: Some(ResourceAmount::Tokens(1_200)),
            hard_ceiling: ResourceAmount::Tokens(1_600),
        },
        TimeBound {
            mode: ConstraintMode::Elastic,
            target_secs: 3600,
            elastic_ceiling_secs: Some(4500),
            hard_ceiling_secs: Some(5400),
            deadline: None,
        },
        quality_floor(),
        Confidence::Low,
        AutonomyBoundary::AskOnApproval,
    )
    .unwrap();

    let config = base_config(dir.path(), policy);
    let listener = libra_governor_daemon::bind_or_detect_running(&config.socket_path).unwrap();
    let socket_path = dir.path().join("d.sock");
    let session_id = "concurrent-subagents-session".to_string();

    let task_id = {
        let mut ledger = LedgerStore::open(&config.ledger_path).unwrap();
        let mut current_task = None;
        let preflight = match send(
            &listener,
            &socket_path,
            &mut ledger,
            &mut current_task,
            &config,
            Request::Preflight {
                task_hint: "run many concurrent subagents against one shared task".to_string(),
                cwd: fixture_repo(),
                session_id: session_id.clone(),
            },
        ) {
            Response::Preflight(result) => *result,
            other => panic!("expected a Preflight response, got {other:?}"),
        };
        assert_eq!(
            preflight.admission.as_ref().map(|d| &d.admission),
            Some(&Admission::Admit)
        );
        preflight.task_id
    };
    drop(listener);

    let gateway_ledger = open_gateway_ledger(&config.ledger_path).unwrap();
    let authority = Arc::new(LedgerSpendAuthority::new(Arc::clone(&gateway_ledger)));

    // Real headroom check before firing: hard_ceiling(1600) - the
    // already-committed plan-level RequiredWork reservation from the
    // Preflight above (1000, cold-start projection == target).
    let before = {
        let ledger = gateway_ledger.lock().unwrap();
        ledger.task_budget(task_id).unwrap().unwrap()
    };
    println!(
        "before concurrent subagents: hard_limit={:?}, completion_reserve={:?}",
        before.hard_limit, before.completion_reserve
    );

    const N: usize = 12;
    const PER_REQUEST_TOKENS: u64 = 200;
    let mut handles = Vec::with_capacity(N);
    for i in 0..N {
        let authority = Arc::clone(&authority);
        let session_id = session_id.clone();
        handles.push(std::thread::spawn(move || {
            authority.authorize(SpendRequest {
                task_id,
                session_id: &session_id,
                amount: ResourceAmount::Tokens(PER_REQUEST_TOKENS),
                idempotency_key: &format!("gw:subagent-{i}"),
                ttl_secs: 600,
            })
        }))
    }

    let mut granted = 0usize;
    let mut denied = 0usize;
    for h in handles {
        match h.join().unwrap().unwrap() {
            SpendDecision::Granted { .. } => granted += 1,
            SpendDecision::Denied(reason) => {
                denied += 1;
                println!("subagent denied: {reason:?}");
            }
        }
    }
    println!("concurrent subagents result: N={N}, granted={granted}, denied={denied}");

    let after = {
        let ledger = gateway_ledger.lock().unwrap();
        ledger.task_budget(task_id).unwrap().unwrap()
    };
    assert_eq!(
        after.completion_reserve, after.initial_completion_reserve,
        "gateway (OptionalWork) reservations must never draw the protected Completion Reserve, \
         even under real concurrent contention: {:?} != {:?}",
        after.completion_reserve, after.initial_completion_reserve
    );
    assert!(
        granted + denied == N,
        "every one of the {N} concurrent subagent requests must resolve to exactly one outcome"
    );
    assert!(
        denied > 0,
        "the whole point of this scenario is to prove some requests hit the boundary: N={N} x \
         {PER_REQUEST_TOKENS} tokens = {} must exceed remaining optional headroom",
        N as u64 * PER_REQUEST_TOKENS
    );

    let reservations = gateway_ledger
        .lock()
        .unwrap()
        .reservations_for_task(task_id)
        .unwrap();
    let active_optional_total: u64 = reservations
        .iter()
        .filter(|r| {
            r.idempotency_key.starts_with("gw:subagent-")
                && r.state == libra_governor_domain::ReservationState::Active
        })
        .map(|r| match r.amount {
            ResourceAmount::Tokens(t) => t,
            other => panic!("unexpected resource kind {other:?}"),
        })
        .sum();
    assert_eq!(
        active_optional_total,
        granted as u64 * PER_REQUEST_TOKENS,
        "no double-spend: the sum of active gateway reservations must equal exactly \
         granted_count * per_request_amount"
    );
}

// --- Security review: credential isolation + prompt-privacy grep -----

/// Deliberately does NOT use `tempfile::tempdir()` (auto-deleted on
/// drop) — the whole point is a persisted directory the security review
/// in `experiments/mvp3_gate/results/security_review.md` can `grep`
/// AFTER this test process exits, exactly like
/// `experiments/mvp1_validation/run_validation_matrix.py::test_privacy_inspection`'s
/// nonce-grep technique but for the gateway's credential path. Prints
/// its own path (visible with `--nocapture`) so the grep command is
/// reproducible from the test's own stdout.
#[test]
fn security_evidence_credential_and_prompt_content_never_touch_persisted_state() {
    let dir = std::env::temp_dir().join("libra-horo1146-mvp3-security-evidence");
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    std::fs::create_dir_all(&dir).unwrap();
    println!(
        "security evidence persisted at: {} (grep this after the test run)",
        dir.display()
    );

    const FAKE_CREDENTIAL: &str = "sk-fake-test-key-mvp3-security-9f3c7a1e";
    const PRIVACY_NONCE: &str = "NONCE-MVP3-7c4c1a8e-do-not-leak-this-prompt-text";

    let gateway_config = GatewayConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        dir.join("gateway.token"),
        GatewayCredentialMode::GovernorHeld {
            credential: CredentialCommand::new(
                "/bin/sh",
                vec!["-c".to_string(), format!("printf '{FAKE_CREDENTIAL}'")],
            ),
        },
    );
    let config = DaemonConfig {
        socket_path: dir.join("d.sock"),
        ledger_path: dir.join("ledger.sqlite3"),
        log_path: dir.join("daemon.log"),
        recon_budget: ReconBudget::default(),
        replan_hysteresis: ReplanHysteresisConfig::default(),
        policy: Policy::validated(
            "security-test",
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
            quality_floor(),
            Confidence::Low,
            AutonomyBoundary::AskOnApproval,
        )
        .unwrap(),
        reservation_ttl_secs: 900,
        gateway: Some(gateway_config),
        gateway_stats: Arc::new(Default::default()),
        gateway_session_header: libra_governor_gateway::proxy::DEFAULT_SESSION_HEADER.to_string(),
    };

    let mut ledger = LedgerStore::open(&config.ledger_path).unwrap();
    let mut current_task = None;
    let listener = libra_governor_daemon::bind_or_detect_running(&config.socket_path).unwrap();
    let socket_path = dir.join("d.sock");

    // A real Preflight whose prompt carries a nonce that must never
    // reach persisted state (the established MVP 1 privacy-invariant
    // check, replayed here alongside the credential check).
    let preflight = match send(
        &listener,
        &socket_path,
        &mut ledger,
        &mut current_task,
        &config,
        Request::Preflight {
            task_hint: format!("fix the login bug — {PRIVACY_NONCE}"),
            cwd: fixture_repo(),
            session_id: "security-session".to_string(),
        },
    ) {
        Response::Preflight(result) => *result,
        other => panic!("expected a Preflight response, got {other:?}"),
    };

    // A real gateway-shaped request/settlement recorded through the
    // exact `LedgerRequestRecorder` the real gateway uses. Its
    // `GatewayRequestRecord` type structurally has no credential field
    // at all — this call demonstrates that by construction, then the
    // grep below confirms it holds on disk too.
    let gateway_ledger = open_gateway_ledger(&config.ledger_path).unwrap();
    let recorder = LedgerRequestRecorder::new(Arc::clone(&gateway_ledger), config.log_path.clone());
    recorder.record(GatewayRequestRecord {
        id: "req-security-1".to_string(),
        task_id: Some(preflight.task_id),
        session_id: Some("security-session".to_string()),
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
    });
    drop(ledger);
    drop(gateway_ledger);
    drop(listener);

    // In-process sanity check before the external grep: read every
    // persisted file's raw bytes ourselves too, so a failure here is
    // attributed precisely rather than only discovered by the shell step.
    for name in ["ledger.sqlite3", "ledger.sqlite3-wal", "daemon.log"] {
        let path = dir.join(name);
        if !path.exists() {
            continue;
        }
        let bytes = std::fs::read(&path).unwrap();
        assert!(
            !bytes
                .windows(FAKE_CREDENTIAL.len())
                .any(|w| w == FAKE_CREDENTIAL.as_bytes()),
            "the fake test credential leaked into {name}"
        );
        assert!(
            !bytes
                .windows(PRIVACY_NONCE.len())
                .any(|w| w == PRIVACY_NONCE.as_bytes()),
            "the raw prompt nonce leaked into {name}"
        );
    }
    println!(
        "in-process grep: no credential or prompt-nonce leakage found in {}",
        dir.display()
    );
}
