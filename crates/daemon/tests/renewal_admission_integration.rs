//! End-to-end proof that both real admission seams (`server.rs`'s
//! preflight path and `gateway_authority.rs`'s `LedgerSpendAuthority`)
//! actually respect a renewal grant (HORO-1727) — not just the
//! ledger-crate-level formulas `renewal_adversarial.rs` already covers.
//!
//! `grant_renewal` is called directly from this file (a `tests/` module,
//! not `daemon/src`), which `renewal_not_wired_live.rs`'s guard
//! deliberately permits — there is still no production caller.
//!
//! Drives real `Preflight` requests over a real socket against the real
//! dispatch logic, following `reservation_integration.rs`'s established
//! pattern, and drives `LedgerSpendAuthority` directly, following
//! `gateway_enforcement.rs`'s.

use std::io::BufReader;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use libra_governor_daemon::gateway_authority::{open_gateway_ledger, LedgerSpendAuthority};
use libra_governor_daemon::{recon::ReconBudget, DaemonConfig};
use libra_governor_domain::{
    Admission, AutonomyBoundary, BlockingStatus, CompletionContract, CompletionCriterion,
    Confidence, ConstraintMode, Policy, RenewalAuthority, RenewalBound, RenewalRequest,
    ReplanHysteresisConfig, ReservationClass, ResourceAmount, ResourceBound, TimeBound,
};
use libra_governor_gateway::authority::{SpendAuthority, SpendDecision, SpendDenial, SpendRequest};
use libra_governor_ledger::{LedgerStore, ReserveRequest};
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

fn config(dir: &Path, policy: Policy, with_gateway: bool) -> DaemonConfig {
    DaemonConfig {
        socket_path: dir.join("d.sock"),
        ledger_path: dir.join("ledger.sqlite3"),
        log_path: dir.join("daemon.log"),
        recon_budget: ReconBudget::default(),
        replan_hysteresis: ReplanHysteresisConfig::default(),
        policy,
        reservation_ttl_secs: 900,
        gateway: with_gateway.then(|| {
            libra_governor_gateway::config::GatewayConfig::new(
                "127.0.0.1:0".parse().unwrap(),
                dir.join("gateway.token"),
                libra_governor_gateway::config::GatewayCredentialMode::GovernorHeld {
                    credential: libra_governor_gateway::credential::CredentialCommand::new(
                        "/bin/sh",
                        vec!["-c".to_string(), "printf 'sk-fake-test-key'".to_string()],
                    ),
                },
            )
        }),
        gateway_stats: Arc::new(Default::default()),
        gateway_session_header: libra_governor_gateway::proxy::DEFAULT_SESSION_HEADER.to_string(),
        extensions: None,
        extension_runtime: std::sync::OnceLock::new(),
        progressive_interval_secs: libra_governor_daemon::DEFAULT_PROGRESSIVE_INTERVAL_SECS,
    }
}

/// A `Hard`-mode, no-slack policy (`target == hard_ceiling == 1,000`)
/// with renewals enabled up to a 10,000-token lifetime ceiling. `Hard`
/// mode deliberately — its only two outcomes are `Admit`/`Deny`, so a
/// flip from one to the other is unambiguous, unlike `Elastic`'s
/// three-way band.
fn renewable_hard_policy() -> Policy {
    Policy::validated(
        "renewal-admission-test",
        ResourceBound {
            mode: ConstraintMode::Hard,
            target: ResourceAmount::Tokens(1_000),
            elastic_ceiling: None,
            hard_ceiling: ResourceAmount::Tokens(1_000),
        },
        TimeBound {
            mode: ConstraintMode::Hard,
            target_secs: 3600,
            elastic_ceiling_secs: None,
            hard_ceiling_secs: Some(3600),
            deadline: None,
        },
        CompletionContract::first(vec![CompletionCriterion::required(
            "required verification (tests/build/lint) passes",
        )]),
        Confidence::Low,
        AutonomyBoundary::AskOnApproval,
    )
    .unwrap()
    .with_renewal(RenewalBound {
        lifetime_ceiling: ResourceAmount::Tokens(10_000),
        max_renewals: 1,
    })
    .unwrap()
}

fn renewal_request(amount: u64, key: &str) -> RenewalRequest {
    RenewalRequest {
        amount: ResourceAmount::Tokens(amount),
        authority: RenewalAuthority::Operator {
            operator_id: "integration-test-operator".to_string(),
        },
        contract_revision: 1,
        reason: "integration test grant".to_string(),
        idempotency_key: key.to_string(),
    }
}

// ---------------------------------------------------------------------
// (1) The preflight (RequiredWork) admission seam
// ---------------------------------------------------------------------

/// A projection strictly between the original `hard_ceiling` (1,000) and
/// the post-grant `effective_hard_limit` (2,000) must be `Deny` before
/// the grant and `Admit` after it — proof that `server.rs`'s admission
/// site actually reads `ResourceBound::extended_by(granted)`, not just
/// the ledger crate's own formulas.
#[test]
fn a_preflight_projection_between_original_and_extended_ceiling_flips_from_deny_to_admit() {
    let dir = tempfile::tempdir().unwrap();
    let policy = renewable_hard_policy();
    let daemon_config = config(dir.path(), policy, false);
    let listener =
        libra_governor_daemon::bind_or_detect_running(&daemon_config.socket_path).unwrap();
    let mut ledger = LedgerStore::open(&daemon_config.ledger_path).unwrap();
    let mut current_task = None;
    let session_id = "renewal-preflight-session".to_string();

    // First preflight: a fresh task is always admitted trivially
    // (committed=0, projected=target=1,000<=hard_ceiling=1,000).
    let first = match send(
        &listener,
        &daemon_config.socket_path,
        &mut ledger,
        &mut current_task,
        &daemon_config,
        Request::Preflight {
            task_hint: "implement the thing".to_string(),
            cwd: fixture_repo(),
            session_id: session_id.clone(),
        },
    ) {
        Response::Preflight(result) => *result,
        other => panic!("expected Preflight, got {other:?}"),
    };
    assert_eq!(
        first.admission.as_ref().map(|d| &d.admission),
        Some(&Admission::Admit)
    );
    let task_id = first.task_id;

    // A manual, plan-independent RequiredWork reservation (`plan_id:
    // None`, exactly how the gateway's own OptionalWork reservations are
    // shaped — see `gateway_enforcement.rs`): unlike a preflight's own
    // plan-level envelope, this is NOT released by the next preflight's
    // "release the outgoing plan's reservation" step, so it persists and
    // gives this test exact, deterministic control over `committed`
    // without needing to replicate `completion_reserve_for`'s own
    // arithmetic. 100 tokens comfortably fits the real headroom left
    // after the first preflight's own (much larger) completion-reserve-
    // aware work envelope under any plausible reserve fraction.
    ledger
        .reserve(ReserveRequest {
            task_id,
            session_id: "manual-bump",
            plan_id: None,
            class: ReservationClass::RequiredWork,
            amount: ResourceAmount::Tokens(100),
            idempotency_key: "manual-bump-1",
            now: time::OffsetDateTime::now_utc(),
            ttl_secs: 900,
        })
        .unwrap();

    // Second preflight (same session => same task): the first plan's own
    // reservation is released before this admission decision runs, so
    // `committed` is exactly the 100-token manual bump. `projected =
    // 100 + 1,000 = 1,100 > hard_ceiling (1,000)` => Deny.
    let second = match send(
        &listener,
        &daemon_config.socket_path,
        &mut ledger,
        &mut current_task,
        &daemon_config,
        Request::Preflight {
            task_hint: "implement the thing".to_string(),
            cwd: fixture_repo(),
            session_id: session_id.clone(),
        },
    ) {
        Response::Preflight(result) => *result,
        other => panic!("expected Preflight, got {other:?}"),
    };
    assert!(
        matches!(
            second.admission.as_ref().map(|d| &d.admission),
            Some(Admission::Deny(_))
        ),
        "a projection of 1,100 against a hard_ceiling of 1,000 must Deny: {:?}",
        second.admission
    );

    // Grant a renewal: effective_hard_limit becomes 1,000 + 1,000 =
    // 2,000.
    let outcome = ledger
        .grant_renewal(
            task_id,
            renewal_request(1_000, "preflight-grant-1"),
            BlockingStatus::NotBlocking,
            time::OffsetDateTime::now_utc(),
        )
        .unwrap();
    assert!(matches!(
        outcome,
        libra_governor_ledger::GrantRenewalOutcome::Granted(_)
    ));

    // Third preflight (same session again): `committed` is still exactly
    // the 100-token manual bump (the second preflight's plan had no
    // reservation of its own, since it was Denied). `projected = 100 +
    // 1,000 = 1,100`, now comfortably under the extended ceiling of
    // 2,000 => Admit.
    let third = match send(
        &listener,
        &daemon_config.socket_path,
        &mut ledger,
        &mut current_task,
        &daemon_config,
        Request::Preflight {
            task_hint: "implement the thing".to_string(),
            cwd: fixture_repo(),
            session_id,
        },
    ) {
        Response::Preflight(result) => *result,
        other => panic!("expected Preflight, got {other:?}"),
    };
    assert_eq!(
        third.admission.as_ref().map(|d| &d.admission),
        Some(&Admission::Admit),
        "a projection of 1,100 must Admit once the effective ceiling is 2,000: {:?}",
        third.admission
    );
}

// ---------------------------------------------------------------------
// (2) The gateway (OptionalWork) admission seam
// ---------------------------------------------------------------------

/// Proves the self-review correction documented in the PR: the gateway's
/// `committed` figure must keep its `+ completion_reserve()` term. A
/// request sized so that `settled + active + request` fits the extended
/// ceiling, but `settled + active + completion_reserve + request` does
/// NOT, must be refused as `PolicyDenied` (the policy projection itself
/// says no) — never `Insufficient` (the ledger's independent capacity
/// gate catching what the policy projection incorrectly let through).
#[test]
fn a_gateway_request_that_only_fits_without_the_reserve_term_is_policy_denied_not_insufficient() {
    let dir = tempfile::tempdir().unwrap();
    let policy = renewable_hard_policy();
    let daemon_config = config(dir.path(), policy, true);
    let listener = UnixListener::bind(&daemon_config.socket_path).unwrap();
    let mut ledger = LedgerStore::open(&daemon_config.ledger_path).unwrap();
    let mut current_task = None;
    let session_id = "renewal-gateway-session".to_string();

    send(
        &listener,
        &daemon_config.socket_path,
        &mut ledger,
        &mut current_task,
        &daemon_config,
        Request::Preflight {
            task_hint: "implement the thing".to_string(),
            cwd: fixture_repo(),
            session_id: session_id.clone(),
        },
    );
    drop(ledger);

    let gateway_ledger = open_gateway_ledger(&daemon_config.ledger_path).unwrap();
    let authority = LedgerSpendAuthority::new(Arc::clone(&gateway_ledger));
    let context = authority
        .budget_context(&session_id)
        .unwrap()
        .expect("the preflighted session resolves to a task with a budget");

    // With the gateway enabled, preflight reserves no plan-level
    // envelope (see `gateway_enforcement.rs`'s
    // `enabling_the_gateway_suppresses_the_plan_level_work_envelope`),
    // so `settled = active = 0` here — `committed` without the reserve
    // term is exactly 0, and with it, exactly the completion reserve
    // (always > 0: at least `COMPLETION_RESERVE_BASE_FRACTION` of the
    // 1,000-token target).
    let reserve_before = gateway_ledger
        .lock()
        .unwrap()
        .task_budget(context.task_id)
        .unwrap()
        .unwrap()
        .completion_reserve;
    assert!(
        reserve_before.as_f64() > 0.0,
        "premise: a real reserve exists"
    );

    let grant_outcome = gateway_ledger
        .lock()
        .unwrap()
        .grant_renewal(
            context.task_id,
            renewal_request(1_000, "gateway-grant-1"),
            BlockingStatus::NotBlocking,
            time::OffsetDateTime::now_utc(),
        )
        .unwrap();
    assert!(matches!(
        grant_outcome,
        libra_governor_ledger::GrantRenewalOutcome::Granted(_)
    ));
    // effective_hard_limit is now 1,000 + 1,000 = 2,000.

    // Sized so that `0 + 0 + 2,000 <= 2,000` (would Admit if the reserve
    // term were dropped) but `0 + 0 + reserve_before + 2,000 > 2,000`
    // (must Deny with the reserve term kept, since reserve_before > 0).
    let decision = authority
        .authorize(SpendRequest {
            task_id: context.task_id,
            session_id: &session_id,
            amount: ResourceAmount::Tokens(2_000),
            idempotency_key: "gw:reserve-term-probe",
            ttl_secs: 600,
        })
        .unwrap();

    match decision {
        SpendDecision::Denied(SpendDenial::PolicyDenied { detail }) => {
            assert!(
                detail.contains("ResourceExceedsHardCeiling"),
                "must be denied by the policy projection itself, not a generic refusal: {detail}"
            );
        }
        other => panic!(
            "expected PolicyDenied (proving the completion-reserve term is still counted in \
             `committed`), got {other:?}"
        ),
    }

    let ledger = gateway_ledger.lock().unwrap();
    assert!(
        ledger
            .reservations_for_task(context.task_id)
            .unwrap()
            .iter()
            .all(|r| r.idempotency_key != "gw:reserve-term-probe"),
        "a PolicyDenied request must never reach the ledger's own reserve() call at all"
    );
}
