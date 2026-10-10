//! Disposable, test-owned canary for the verified-provider signature path
//! (HORO-1727 PR 5b).
//!
//! # What this proves, and what it deliberately does not
//!
//! This exercises the full real wire path — `Request::RecordOutcome`
//! over a real Unix socket, through `handle_connection`/
//! `handle_record_outcome`, into `outcome_authority_wiring::verify_claim`,
//! into the ledger — with `DaemonConfig::outcome_authority` set to
//! `Some(..)`. It proves the verification logic and the wiring between
//! the protocol, daemon, and extension crates actually work together end
//! to end: a validly signed claim is treated as authoritative and
//! promotes a receipt; an absent or wrongly-signed one is not.
//!
//! Per ADR-0017, it does **not** prove HORO-1727 Decision 1 is satisfied
//! on a real deployment — every secret here is a `/bin/sh -c printf`
//! command the SAME test process can read, exactly the single-OS-user
//! same-principal condition ADR-0017 says no code-level fix can close.
//! `DaemonConfig::outcome_authority` stays `None` in the one real daemon
//! entry point (`cli/src/daemon_cmd.rs::run`) regardless of what this
//! file proves — see that field's own docs.
//!
//! Uses a tempdir-scoped `LedgerStore` and a tempdir-scoped Unix socket
//! bound by this test itself, mirroring `outcome_attestation_canary.rs`'s
//! established real-seam pattern. No test here ever touches the real
//! dogfood daemon or its state directory.

use std::io::BufReader;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use libra_governor_daemon::{recon::ReconBudget, DaemonConfig};
use libra_governor_domain::{
    Admission, AutonomyBoundary, CompletionContract, CompletionCriterion, Confidence,
    ConstraintMode, ExecutionOutcome, Policy, ReplanHysteresisConfig, ResourceAmount,
    ResourceBound, TimeBound,
};
use libra_governor_extension::{
    canonical_bytes, OutcomeAuthorityConfig, OutcomeClaimContent, TrustedProvider,
    WebhookSecretCommand,
};
use libra_governor_ledger::LedgerStore;
use libra_governor_protocol::{
    wire, OutcomeRecordedOutcome, OutcomeRecordedResult, Request, RequestEnvelope, Response,
    ResponseEnvelope, SignedOutcomeClaimWire, PROTOCOL_VERSION,
};

fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample_repo")
}

struct Harness {
    listener: UnixListener,
    socket_path: PathBuf,
    config: DaemonConfig,
}

impl Harness {
    fn new(dir: &Path, policy: Policy, outcome_authority: Option<OutcomeAuthorityConfig>) -> Self {
        let config = config(dir, policy, outcome_authority);
        let listener = libra_governor_daemon::bind_or_detect_running(&config.socket_path).unwrap();
        Harness {
            listener,
            socket_path: config.socket_path.clone(),
            config,
        }
    }
}

fn send(
    harness: &Harness,
    ledger: &mut LedgerStore,
    current_task: &mut Option<libra_governor_protocol::TaskSummary>,
    request: Request,
) -> Response {
    let socket_path = harness.socket_path.clone();
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
    let (stream, _) = harness.listener.accept().unwrap();
    libra_governor_daemon::handle_connection(stream, ledger, current_task, &harness.config)
        .unwrap();
    client.join().unwrap()
}

fn config(
    dir: &Path,
    policy: Policy,
    outcome_authority: Option<OutcomeAuthorityConfig>,
) -> DaemonConfig {
    DaemonConfig {
        socket_path: dir.join("d.sock"),
        ledger_path: dir.join("ledger.sqlite3"),
        log_path: dir.join("daemon.log"),
        recon_budget: ReconBudget::default(),
        replan_hysteresis: ReplanHysteresisConfig::default(),
        policy,
        reservation_ttl_secs: 900,
        gateway: None,
        gateway_stats: Default::default(),
        gateway_session_header: libra_governor_gateway::proxy::DEFAULT_SESSION_HEADER.to_string(),
        extensions: None,
        extension_runtime: std::sync::OnceLock::new(),
        outcome_authority,
        progressive_interval_secs: libra_governor_daemon::DEFAULT_PROGRESSIVE_INTERVAL_SECS,
    }
}

fn permissive_policy() -> Policy {
    Policy::validated(
        "outcome-authority-canary",
        ResourceBound {
            mode: ConstraintMode::Hard,
            target: ResourceAmount::Tokens(1_000_000),
            elastic_ceiling: None,
            hard_ceiling: ResourceAmount::Tokens(1_000_000),
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
}

fn fake_secret(value: &str) -> libra_governor_extension::WebhookSecret {
    WebhookSecretCommand::new("/bin/sh", vec!["-c".to_string(), format!("printf {value}")])
        .resolve()
        .unwrap()
}

/// Admits a fresh task via a real `Preflight`, returning its
/// `(task_id, plan_id)`.
fn admit_task(
    harness: &Harness,
    ledger: &mut LedgerStore,
    current_task: &mut Option<libra_governor_protocol::TaskSummary>,
    session_id: &str,
) -> (libra_governor_domain::TaskId, libra_governor_domain::PlanId) {
    match send(
        harness,
        ledger,
        current_task,
        Request::Preflight {
            task_hint: "canary task".to_string(),
            cwd: fixture_repo(),
            session_id: session_id.to_string(),
        },
    ) {
        Response::Preflight(result) => {
            assert_eq!(
                result.admission.as_ref().map(|d| &d.admission),
                Some(&Admission::Admit),
                "canary setup requires a trivially-admitted task"
            );
            (result.task_id, result.plan_id)
        }
        other => panic!("expected Preflight, got {other:?}"),
    }
}

fn finalize(
    harness: &Harness,
    ledger: &mut LedgerStore,
    current_task: &mut Option<libra_governor_protocol::TaskSummary>,
    session_id: &str,
) {
    let response = send(
        harness,
        ledger,
        current_task,
        Request::Finalize {
            session_id: session_id.to_string(),
            model: None,
            provider: None,
            transcript_path: None,
        },
    );
    assert!(matches!(response, Response::Finalize(_)));
}

/// Builds the exact `evidence_digest` `handle_record_outcome` itself
/// computes — sha256 hex of `serde_json::to_string(outcome.evidence())` —
/// so a test-signed claim matches what the daemon will independently
/// derive from the same `outcome` field.
fn evidence_digest(outcome: &ExecutionOutcome) -> String {
    use sha2::Digest as _;
    let evidence_json = serde_json::to_string(outcome.evidence()).unwrap();
    let digest = sha2::Sha256::digest(evidence_json.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn outcome_kind_str(outcome: &ExecutionOutcome) -> &'static str {
    match outcome {
        ExecutionOutcome::Completed { .. } => "completed",
        ExecutionOutcome::Failed { .. } => "failed",
        ExecutionOutcome::Aborted { .. } => "aborted",
        ExecutionOutcome::Unknown => "unknown",
    }
}

#[test]
fn a_validly_signed_claim_is_treated_as_authoritative_and_promotes_the_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let secret = fake_secret("sk-fake-canary-provider");
    let harness = Harness::new(
        dir.path(),
        permissive_policy(),
        Some(OutcomeAuthorityConfig {
            providers: vec![TrustedProvider {
                source_id: "canary-ci".to_string(),
                secret,
            }],
            max_skew_secs: 300,
        }),
    );
    let mut ledger = LedgerStore::open(&harness.config.ledger_path).unwrap();
    let mut current_task = None;

    let (task_id, plan_id) = admit_task(&harness, &mut ledger, &mut current_task, "canary-signed");
    finalize(&harness, &mut ledger, &mut current_task, "canary-signed");

    let outcome = ExecutionOutcome::Completed {
        evidence: vec!["https://ci.example.com/runs/1".to_string()],
    };
    let content = OutcomeClaimContent {
        task_id,
        plan_id: Some(plan_id),
        outcome_kind: outcome_kind_str(&outcome).to_string(),
        evidence_digest: evidence_digest(&outcome),
        idempotency_key: "canary-signed-key".to_string(),
        issued_at: time::OffsetDateTime::now_utc().unix_timestamp() as u64,
    };
    // Re-resolve the same secret the daemon itself is configured with —
    // mirrors how a real external provider would hold its own copy.
    let signing_secret = fake_secret("sk-fake-canary-provider");
    let signature = libra_governor_extension::sign(
        &signing_secret,
        content.issued_at,
        &content.idempotency_key,
        &canonical_bytes(&content),
    );

    let response = send(
        &harness,
        &mut ledger,
        &mut current_task,
        Request::RecordOutcome {
            task_id,
            plan_id: Some(plan_id),
            source_id: "claimed-but-irrelevant".to_string(),
            idempotency_key: content.idempotency_key.clone(),
            outcome,
            signed_claim: Some(SignedOutcomeClaimWire {
                issued_at: content.issued_at,
                signature,
            }),
        },
    );

    let Response::OutcomeRecorded(OutcomeRecordedOutcome::Recorded(result)) = response else {
        panic!("expected Recorded, got {response:?}");
    };
    let OutcomeRecordedResult {
        authoritative,
        receipt_updated,
        contract_revision,
        ..
    } = *result;
    assert!(
        authoritative,
        "a validly signed claim against a configured trusted provider must be authoritative"
    );
    assert!(
        receipt_updated,
        "an authoritative Completed claim at the current revision must promote the receipt"
    );
    assert_eq!(contract_revision, Some(1));
}

#[test]
fn an_unsigned_push_against_a_configured_authority_stays_non_authoritative() {
    let dir = tempfile::tempdir().unwrap();
    let secret = fake_secret("sk-fake-canary-provider");
    let harness = Harness::new(
        dir.path(),
        permissive_policy(),
        Some(OutcomeAuthorityConfig {
            providers: vec![TrustedProvider {
                source_id: "canary-ci".to_string(),
                secret,
            }],
            max_skew_secs: 300,
        }),
    );
    let mut ledger = LedgerStore::open(&harness.config.ledger_path).unwrap();
    let mut current_task = None;

    let (task_id, plan_id) =
        admit_task(&harness, &mut ledger, &mut current_task, "canary-unsigned");
    finalize(&harness, &mut ledger, &mut current_task, "canary-unsigned");

    let response = send(
        &harness,
        &mut ledger,
        &mut current_task,
        Request::RecordOutcome {
            task_id,
            plan_id: Some(plan_id),
            source_id: "self-claimed-ci".to_string(),
            idempotency_key: "canary-unsigned-key".to_string(),
            outcome: ExecutionOutcome::Completed { evidence: vec![] },
            signed_claim: None,
        },
    );

    let Response::OutcomeRecorded(OutcomeRecordedOutcome::Recorded(result)) = response else {
        panic!("expected Recorded, got {response:?}");
    };
    assert!(
        !result.authoritative,
        "a daemon configured with a trusted authority must still refuse to treat an unsigned \
         push as authoritative"
    );
    assert!(!result.receipt_updated);
}

#[test]
fn a_claim_signed_with_the_wrong_key_stays_non_authoritative() {
    let dir = tempfile::tempdir().unwrap();
    let configured_secret = fake_secret("sk-fake-canary-provider");
    let harness = Harness::new(
        dir.path(),
        permissive_policy(),
        Some(OutcomeAuthorityConfig {
            providers: vec![TrustedProvider {
                source_id: "canary-ci".to_string(),
                secret: configured_secret,
            }],
            max_skew_secs: 300,
        }),
    );
    let mut ledger = LedgerStore::open(&harness.config.ledger_path).unwrap();
    let mut current_task = None;

    let (task_id, plan_id) =
        admit_task(&harness, &mut ledger, &mut current_task, "canary-wrong-key");
    finalize(&harness, &mut ledger, &mut current_task, "canary-wrong-key");

    let outcome = ExecutionOutcome::Completed { evidence: vec![] };
    let content = OutcomeClaimContent {
        task_id,
        plan_id: Some(plan_id),
        outcome_kind: outcome_kind_str(&outcome).to_string(),
        evidence_digest: evidence_digest(&outcome),
        idempotency_key: "canary-wrong-key-key".to_string(),
        issued_at: time::OffsetDateTime::now_utc().unix_timestamp() as u64,
    };
    let attacker_secret = fake_secret("sk-fake-attacker-key");
    let signature = libra_governor_extension::sign(
        &attacker_secret,
        content.issued_at,
        &content.idempotency_key,
        &canonical_bytes(&content),
    );

    let response = send(
        &harness,
        &mut ledger,
        &mut current_task,
        Request::RecordOutcome {
            task_id,
            plan_id: Some(plan_id),
            source_id: "canary-ci".to_string(),
            idempotency_key: content.idempotency_key.clone(),
            outcome,
            signed_claim: Some(SignedOutcomeClaimWire {
                issued_at: content.issued_at,
                signature,
            }),
        },
    );

    let Response::OutcomeRecorded(OutcomeRecordedOutcome::Recorded(result)) = response else {
        panic!("expected Recorded, got {response:?}");
    };
    assert!(
        !result.authoritative,
        "a signature from an unconfigured key must never be treated as authoritative, even \
         when source_id matches a real configured provider's name"
    );
}
