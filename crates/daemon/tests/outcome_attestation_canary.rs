//! Disposable, test-owned canary for the Completion Attestation /
//! Outcome Provider path (HORO-1174's mechanism, audited for HORO-1727's
//! evidence-gap investigation).
//!
//! Every test here runs against a tempdir-scoped `LedgerStore` and a
//! tempdir-scoped Unix socket bound by this test itself, following
//! `renewal_admission_integration.rs`'s established real-seam pattern.
//! **No test in this file ever touches the real dogfood daemon or its
//! state directory** — there is no code path here that could resolve to
//! `~/.local/state/libra-governor`.
//!
//! These tests exist to answer, with evidence rather than assumption,
//! three questions this campaign's root-cause investigation left open:
//! who can submit an authoritative completion claim, what happens when
//! two claims disagree, and what happens when a claim names a stale
//! plan. All three answers below are reported as findings in the
//! HORO-1727 decision packet — this file documents actual behavior, it
//! does not decide what the behavior should be.

use std::io::BufReader;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use libra_governor_daemon::{recon::ReconBudget, DaemonConfig};
use libra_governor_domain::{
    Admission, AutonomyBoundary, CompletionContract, CompletionCriterion, Confidence,
    ConstraintMode, ExecutionOutcome, Policy, ReplanHysteresisConfig, ResourceAmount,
    ResourceBound, TimeBound,
};
use libra_governor_ledger::LedgerStore;
use libra_governor_protocol::{
    wire, OutcomeRecordedOutcome, Request, RequestEnvelope, Response, ResponseEnvelope,
    PROTOCOL_VERSION,
};

fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample_repo")
}

/// The fixed, immutable parts of one test's daemon-over-socket harness,
/// bundled so none of this file's helper functions trips clippy's
/// `too_many_arguments` lint once the per-call domain arguments (task
/// id, source id, idempotency key, outcome) are added on top.
struct Harness {
    listener: UnixListener,
    socket_path: PathBuf,
    config: DaemonConfig,
}

impl Harness {
    fn new(dir: &Path, policy: Policy) -> Self {
        let config = config(dir, policy);
        let listener = libra_governor_daemon::bind_or_detect_running(&config.socket_path).unwrap();
        Harness {
            listener,
            socket_path: config.socket_path.clone(),
            config,
        }
    }

    /// Rebinds a fresh listener against the same socket/ledger paths --
    /// simulating a daemon process restart, where the next process binds
    /// fresh but the ledger file (and everything persisted in it) is
    /// unchanged.
    fn rebind(dir: &Path, policy: Policy, ledger_path: &Path) -> Self {
        let mut config = config(dir, policy);
        config.ledger_path = ledger_path.to_path_buf();
        let _ = std::fs::remove_file(&config.socket_path);
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

fn config(dir: &Path, policy: Policy) -> DaemonConfig {
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
        progressive_interval_secs: libra_governor_daemon::DEFAULT_PROGRESSIVE_INTERVAL_SECS,
    }
}

/// A large, effectively-unconstrained policy -- these tests are about
/// the outcome-attestation path, not admission arithmetic, so every
/// `Preflight` in this file should trivially Admit.
fn permissive_policy() -> Policy {
    Policy::validated(
        "outcome-attestation-canary",
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

/// Opens a second, independent connection to the same ledger file for
/// read-only verification -- exactly `task_scoped_duration.rs`'s
/// established pattern for inspecting raw rows a test doesn't have a
/// higher-level accessor for.
fn verify_conn(ledger_path: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(ledger_path).unwrap()
}

/// Admits a fresh task, returning its `task_id`. Does not finalize it —
/// several tests below deliberately push `RecordOutcome` before any
/// `Finalize` ever ran, exactly the "no receipt to promote yet" case
/// `OutcomeRecordedResult::receipt_updated`'s own docs describe.
fn admit_task(
    harness: &Harness,
    ledger: &mut LedgerStore,
    current_task: &mut Option<libra_governor_protocol::TaskSummary>,
    session_id: &str,
) -> libra_governor_domain::TaskId {
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
            result.task_id
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

fn record_outcome(
    harness: &Harness,
    ledger: &mut LedgerStore,
    current_task: &mut Option<libra_governor_protocol::TaskSummary>,
    task_id: libra_governor_domain::TaskId,
    source_id: &str,
    idempotency_key: &str,
    outcome: ExecutionOutcome,
) -> OutcomeRecordedOutcome {
    match send(
        harness,
        ledger,
        current_task,
        Request::RecordOutcome {
            task_id,
            plan_id: None,
            source_id: source_id.to_string(),
            idempotency_key: idempotency_key.to_string(),
            outcome,
        },
    ) {
        Response::OutcomeRecorded(outcome) => outcome,
        other => panic!("expected OutcomeRecorded, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// 1. Positive: a genuine external push is recorded and promotes
// ---------------------------------------------------------------------

#[test]
fn a_completed_push_from_an_outcome_provider_is_recorded_and_promotes_the_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let harness = Harness::new(dir.path(), permissive_policy());
    let mut ledger = LedgerStore::open(&harness.config.ledger_path).unwrap();
    let mut current_task = None;

    let task_id = admit_task(&harness, &mut ledger, &mut current_task, "canary-positive");

    // Finalize first, exactly like a real session's Stop hook, so there
    // is a receipt to promote.
    finalize(&harness, &mut ledger, &mut current_task, "canary-positive");

    let outcome = record_outcome(
        &harness,
        &mut ledger,
        &mut current_task,
        task_id,
        "canary-ci-system",
        "canary-run-1",
        ExecutionOutcome::Completed {
            evidence: vec!["https://ci.example.com/canary/1".to_string()],
        },
    );

    let result = match outcome {
        OutcomeRecordedOutcome::Recorded(result) => *result,
        other => panic!("expected Recorded, got {other:?}"),
    };
    assert!(
        result.receipt_updated,
        "a push after Finalize must promote the existing receipt"
    );
}

// ---------------------------------------------------------------------
// 2. Negative: a nonexistent task is refused
// ---------------------------------------------------------------------

#[test]
fn a_push_for_a_nonexistent_task_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let harness = Harness::new(dir.path(), permissive_policy());
    let mut ledger = LedgerStore::open(&harness.config.ledger_path).unwrap();
    let mut current_task = None;

    let outcome = record_outcome(
        &harness,
        &mut ledger,
        &mut current_task,
        libra_governor_domain::TaskId::new(),
        "canary-ci-system",
        "canary-run-ghost",
        ExecutionOutcome::Completed { evidence: vec![] },
    );
    assert_eq!(outcome, OutcomeRecordedOutcome::NoSuchTask);
}

// ---------------------------------------------------------------------
// 3. Duplicate: the same (task, source, idempotency_key) replays safely
// ---------------------------------------------------------------------

#[test]
fn a_duplicate_push_replays_safely_without_a_second_write() {
    let dir = tempfile::tempdir().unwrap();
    let harness = Harness::new(dir.path(), permissive_policy());
    let mut ledger = LedgerStore::open(&harness.config.ledger_path).unwrap();
    let mut current_task = None;

    let task_id = admit_task(&harness, &mut ledger, &mut current_task, "canary-duplicate");

    let first = record_outcome(
        &harness,
        &mut ledger,
        &mut current_task,
        task_id,
        "canary-ci-system",
        "canary-run-dup",
        ExecutionOutcome::Completed { evidence: vec![] },
    );
    assert!(matches!(first, OutcomeRecordedOutcome::Recorded(_)));

    let second = record_outcome(
        &harness,
        &mut ledger,
        &mut current_task,
        task_id,
        "canary-ci-system",
        "canary-run-dup",
        ExecutionOutcome::Completed { evidence: vec![] },
    );
    assert_eq!(second, OutcomeRecordedOutcome::Duplicate);

    let count: i64 = verify_conn(&harness.config.ledger_path)
        .query_row(
            "SELECT COUNT(*) FROM outcome_attestations WHERE task_id = ?1",
            [task_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "a duplicate idempotency key must not write twice");
}

// ---------------------------------------------------------------------
// 4. FINDING: nothing distinguishes a real external provider from a
//    governed agent self-attesting its own completion
// ---------------------------------------------------------------------

#[test]
fn finding_a_local_caller_can_self_attest_completion_with_no_distinguishing_signal() {
    // This test's name says "finding", not "forged push rejected",
    // because the push is NOT rejected -- that is exactly the point.
    // `Request::RecordOutcome`'s own doc comment already discloses that
    // `source_id` is a recorded claim, not an authenticated identity,
    // and that the trust boundary is the socket's filesystem
    // permissions (0600) -- consistent with every other `Request`
    // variant. What this test demonstrates, which was NOT already
    // disclosed anywhere: `handle_record_outcome` unconditionally
    // constructs `AttestationSource::Provider` for every push, with no
    // branch that could ever produce `AttestationSource::Agent`. There
    // is no way, via the real protocol, for a caller to honestly
    // self-tag as "this is the agent's own unverified claim" -- every
    // push through this one real ingestion path is authoritative by
    // construction, regardless of who is actually on the other end of
    // the socket.
    let dir = tempfile::tempdir().unwrap();
    let harness = Harness::new(dir.path(), permissive_policy());
    let mut ledger = LedgerStore::open(&harness.config.ledger_path).unwrap();
    let mut current_task = None;

    let task_id = admit_task(
        &harness,
        &mut ledger,
        &mut current_task,
        "canary-self-attest",
    );

    // Nothing here is a real CI system or an external reviewer -- this
    // is the test process itself, the same way the governed agent could
    // shell out to `libra-governor outcome record` directly. The
    // `source_id` string is entirely self-chosen.
    let outcome = record_outcome(
        &harness,
        &mut ledger,
        &mut current_task,
        task_id,
        "totally-unverified-self-claim",
        "canary-self-attest-1",
        ExecutionOutcome::Completed {
            evidence: vec!["(no evidence -- an agent could write anything here)".to_string()],
        },
    );

    let result = match outcome {
        OutcomeRecordedOutcome::Recorded(result) => *result,
        other => panic!("expected Recorded, got {other:?}"),
    };
    // The push succeeds and is treated as authoritative -- this is the
    // finding, reported honestly in the HORO-1727 decision packet, not
    // a passing security control.
    assert_eq!(
        result.attested,
        ExecutionOutcome::Completed {
            evidence: vec!["(no evidence -- an agent could write anything here)".to_string()]
        }
    );
    let authoritative: bool = verify_conn(&harness.config.ledger_path)
        .query_row(
            "SELECT authoritative FROM outcome_attestations WHERE task_id = ?1",
            [task_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        authoritative,
        "FINDING: an unverified self-claim is stored as authoritative=true, \
         identically to a real external Outcome Provider push -- the \
         AttestationSource::Agent variant has no production path that can \
         ever construct it"
    );
}

// ---------------------------------------------------------------------
// 5. FINDING: two disagreeing authoritative attestations silently
//    last-writer-wins, with no conflict detection
// ---------------------------------------------------------------------

#[test]
fn finding_two_disagreeing_authoritative_attestations_silently_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let harness = Harness::new(dir.path(), permissive_policy());
    let mut ledger = LedgerStore::open(&harness.config.ledger_path).unwrap();
    let mut current_task = None;

    let task_id = admit_task(&harness, &mut ledger, &mut current_task, "canary-conflict");
    finalize(&harness, &mut ledger, &mut current_task, "canary-conflict");

    let completed = record_outcome(
        &harness,
        &mut ledger,
        &mut current_task,
        task_id,
        "provider-a",
        "conflict-1",
        ExecutionOutcome::Completed {
            evidence: vec!["https://ci.example.com/a".to_string()],
        },
    );
    assert!(matches!(completed, OutcomeRecordedOutcome::Recorded(_)));

    // A second, independently-keyed push (different idempotency_key, so
    // it is NOT deduped) claims the opposite outcome for the same task.
    let failed = record_outcome(
        &harness,
        &mut ledger,
        &mut current_task,
        task_id,
        "provider-b",
        "conflict-2",
        ExecutionOutcome::Failed {
            evidence: vec!["https://ci.example.com/b".to_string()],
        },
    );
    assert!(matches!(failed, OutcomeRecordedOutcome::Recorded(_)));

    // FINDING: both rows exist in outcome_attestations (the append-only
    // audit trail is intact), but receipts.outcome_json now reflects
    // only the LAST push -- Failed -- with no error, no conflict flag,
    // and no record of which attestation "won". A reader of
    // receipts.outcome_json alone cannot tell this task ever had a
    // disagreement.
    let row_count: i64 = verify_conn(&harness.config.ledger_path)
        .query_row(
            "SELECT COUNT(*) FROM outcome_attestations WHERE task_id = ?1",
            [task_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        row_count, 2,
        "both disagreeing attestations are independently recorded"
    );

    let promoted_json: String = verify_conn(&harness.config.ledger_path)
        .query_row(
            "SELECT outcome_json FROM receipts WHERE task_id = ?1 \
             ORDER BY recorded_at DESC LIMIT 1",
            [task_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        promoted_json.contains("\"failed\""),
        "FINDING: the later push silently wins with no conflict signal; \
         promoted outcome_json was {promoted_json}"
    );
}

// ---------------------------------------------------------------------
// 6. Restart: idempotent replay survives reopening the same ledger file
// ---------------------------------------------------------------------

#[test]
fn a_duplicate_push_after_a_simulated_restart_still_replays_safely() {
    let dir = tempfile::tempdir().unwrap();
    let policy = permissive_policy();
    let ledger_path = dir.path().join("ledger.sqlite3");
    let task_id;
    {
        let harness = Harness::new(dir.path(), policy.clone());
        let mut ledger = LedgerStore::open(&harness.config.ledger_path).unwrap();
        let mut current_task = None;
        task_id = admit_task(&harness, &mut ledger, &mut current_task, "canary-restart");
        let first = record_outcome(
            &harness,
            &mut ledger,
            &mut current_task,
            task_id,
            "canary-ci-system",
            "canary-restart-key",
            ExecutionOutcome::Completed { evidence: vec![] },
        );
        assert!(matches!(first, OutcomeRecordedOutcome::Recorded(_)));
        // `harness`/`ledger` drop here -- simulating the daemon process
        // exiting. The ledger file on disk is the only thing that
        // survives into the next block, exactly as a real daemon
        // restart would leave it.
    }

    let harness = Harness::rebind(dir.path(), policy, &ledger_path);
    let mut ledger = LedgerStore::open(&harness.config.ledger_path).unwrap();
    let mut current_task = None;
    let replay = record_outcome(
        &harness,
        &mut ledger,
        &mut current_task,
        task_id,
        "canary-ci-system",
        "canary-restart-key",
        ExecutionOutcome::Completed { evidence: vec![] },
    );
    assert_eq!(
        replay,
        OutcomeRecordedOutcome::Duplicate,
        "idempotency survives a daemon restart because it is persisted in \
         the ledger file, not in-memory state"
    );
}

// ---------------------------------------------------------------------
// 7. Multi-session: two different sources attesting the same outcome
//    for the same task both record, neither is treated as a duplicate
// ---------------------------------------------------------------------

#[test]
fn two_different_sources_attesting_agreement_both_record_independently() {
    let dir = tempfile::tempdir().unwrap();
    let harness = Harness::new(dir.path(), permissive_policy());
    let mut ledger = LedgerStore::open(&harness.config.ledger_path).unwrap();
    let mut current_task = None;

    let task_id = admit_task(
        &harness,
        &mut ledger,
        &mut current_task,
        "canary-multi-source",
    );

    let from_ci = record_outcome(
        &harness,
        &mut ledger,
        &mut current_task,
        task_id,
        "ci-system",
        "multi-source-ci",
        ExecutionOutcome::Completed {
            evidence: vec!["https://ci.example.com/multi".to_string()],
        },
    );
    assert!(matches!(from_ci, OutcomeRecordedOutcome::Recorded(_)));

    let from_reviewer = record_outcome(
        &harness,
        &mut ledger,
        &mut current_task,
        task_id,
        "human-reviewer-tool",
        "multi-source-reviewer",
        ExecutionOutcome::Completed {
            evidence: vec!["reviewed-by:example-reviewer".to_string()],
        },
    );
    assert!(matches!(from_reviewer, OutcomeRecordedOutcome::Recorded(_)));

    let row_count: i64 = verify_conn(&harness.config.ledger_path)
        .query_row(
            "SELECT COUNT(*) FROM outcome_attestations WHERE task_id = ?1",
            [task_id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        row_count, 2,
        "two distinct sources attesting agreement both get their own row \
         -- source_id, not just task_id, is part of the dedupe key"
    );
}
