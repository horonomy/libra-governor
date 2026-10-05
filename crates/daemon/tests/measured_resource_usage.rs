//! HORO-1725: whether a receipt ever records what a task actually spent,
//! and whether the reserved envelope tracks it.
//!
//! `handle_finalize` wrote `actual_usage: vec![]` on every receipt. The
//! stated reason was that Claude Code's hook payloads expose no token
//! data — which is false: the `Stop` payload carries `transcript_path`,
//! and the host records `input_tokens`, `cache_creation_input_tokens`,
//! `cache_read_input_tokens` and `output_tokens` per assistant record.
//!
//! Because no receipt carried usage, `resource_quantiles` returned
//! `(None, None, None)` for every task, so `Estimate::resource_p80` was
//! unconditionally `None`, so the admission projection in
//! `handle_preflight` always fell back to `config.policy.resource.target`.
//! Every task on the workstation where this was found reserved the
//! identical constant: `select distinct amount from reservations` over 57
//! receipts returned exactly one row, 70000.0, and the rendered share sat
//! at `(150000 - 70000 - 0) / 150000 = 0.5333` — "53% budget left" —
//! forever, regardless of what the task was or how much it used.
//!
//! **The shape that catches this is two tasks whose real usage differs by
//! an order of magnitude.** A suite in which every task is estimated from
//! the same constant cannot tell a tracking envelope from a frozen one,
//! which is how a green suite shipped it. So each test below seeds real
//! measured history through the full `Preflight` → `Stop` lifecycle, with
//! a host transcript whose token counts it chooses.
//!
//! Two bounds are stated as live economics rather than as bare numbers:
//! `SMALL_TURN_TOKENS` and `LARGE_TURN_TOKENS` straddle the policy target
//! below, so an implementation that ignored measurement and kept using
//! the target would land between them and fail the comparison, and
//! `HARD_CEILING` is set high enough that neither is denied — the point
//! here is what gets reserved, not what gets refused.

use std::io::BufReader;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use libra_governor_daemon::{recon::ReconBudget, DaemonConfig};
use libra_governor_domain::{
    Admission, AutonomyBoundary, CompletionContract, CompletionCriterion, Confidence,
    ConstraintMode, Policy, ReplanHysteresisConfig, ReservationState, ResourceAmount,
    ResourceBound, ResourceKind, TimeBound,
};
use libra_governor_ledger::LedgerStore;
use libra_governor_protocol::{
    wire, BudgetPosture, FinalizeOutcome, FinalizeResult, Request, RequestEnvelope, Response,
    ResponseEnvelope, PROTOCOL_VERSION,
};
use time::OffsetDateTime;

/// One modest turn's measured fresh tokens. Below the policy target.
const SMALL_TURN_TOKENS: u64 = 20_000;
/// One heavy turn's measured fresh tokens. Above the policy target, and
/// an order of magnitude above `SMALL_TURN_TOKENS`, so "materially
/// different" needs no interpretation.
const LARGE_TURN_TOKENS: u64 = 400_000;
/// The constant every task used to reserve, and the value an
/// unmeasured task still falls back to.
const POLICY_TARGET: u64 = 100_000;
/// Deliberately generous: this file is about the size of the envelope,
/// not about admission refusing an oversized one.
const HARD_CEILING: u64 = 4_000_000;

fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample_repo")
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

/// `Confidence::Low` because a cold-start estimate carries `Low` and a
/// higher floor would Deny on confidence before any of this was
/// exercised — the same reasoning `reservation_integration.rs::generous_policy`
/// records.
fn admitting_policy() -> Policy {
    Policy::validated(
        "measured-resource-usage",
        ResourceBound {
            mode: ConstraintMode::Elastic,
            target: ResourceAmount::Tokens(POLICY_TARGET),
            elastic_ceiling: Some(ResourceAmount::Tokens(HARD_CEILING / 2)),
            hard_ceiling: ResourceAmount::Tokens(HARD_CEILING),
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

struct Harness {
    dir: tempfile::TempDir,
    config: DaemonConfig,
    ledger: LedgerStore,
    current_task: Option<libra_governor_protocol::TaskSummary>,
    listener: std::os::unix::net::UnixListener,
    socket_path: PathBuf,
}

impl Harness {
    fn new() -> Self {
        Self::with_policy(admitting_policy())
    }

    fn with_policy(policy: Policy) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = DaemonConfig {
            socket_path: dir.path().join("d.sock"),
            ledger_path: dir.path().join("ledger.sqlite3"),
            log_path: dir.path().join("daemon.log"),
            recon_budget: ReconBudget::default(),
            replan_hysteresis: ReplanHysteresisConfig::default(),
            policy,
            reservation_ttl_secs: 900,
            gateway: None,
            gateway_stats: std::sync::Arc::new(Default::default()),
            gateway_session_header: libra_governor_gateway::proxy::DEFAULT_SESSION_HEADER
                .to_string(),
            extensions: None,
            extension_runtime: std::sync::OnceLock::new(),
            progressive_interval_secs: libra_governor_daemon::DEFAULT_PROGRESSIVE_INTERVAL_SECS,
        };
        let ledger = LedgerStore::open(&config.ledger_path).unwrap();
        let listener = libra_governor_daemon::bind_or_detect_running(&config.socket_path).unwrap();
        let socket_path = dir.path().join("d.sock");
        Self {
            dir,
            config,
            ledger,
            current_task: None,
            listener,
            socket_path,
        }
    }

    fn send(&mut self, request: Request) -> Response {
        send(
            &self.listener,
            &self.socket_path,
            &mut self.ledger,
            &mut self.current_task,
            &self.config,
            request,
        )
    }

    fn preflight(&mut self, session_id: &str) -> libra_governor_protocol::PreflightResult {
        self.preflight_in(session_id, fixture_repo())
    }

    fn preflight_in(
        &mut self,
        session_id: &str,
        cwd: PathBuf,
    ) -> libra_governor_protocol::PreflightResult {
        match self.send(Request::Preflight {
            task_hint: "fix the login bug".to_string(),
            cwd,
            session_id: session_id.to_string(),
        }) {
            Response::Preflight(result) => *result,
            other => panic!("expected a Preflight response, got {other:?}"),
        }
    }

    /// Writes a one-turn transcript in the host's own shape, timestamped
    /// now so it falls inside the window `handle_finalize` will measure,
    /// and returns its path. `fresh` is split across the three fields the
    /// daemon counts as fresh work; `cached` goes to the field it must
    /// ignore.
    fn write_transcript(&self, session_id: &str, fresh: u64, cached: u64) -> PathBuf {
        let path = self.dir.path().join(format!("{session_id}.jsonl"));
        let ts = OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        // A third each to input/cache_creation/output, remainder on
        // input, so no single field is load-bearing for the total.
        let third = fresh / 3;
        let input = fresh - 2 * third;
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(
            file,
            r#"{{"type":"user","timestamp":"{ts}","message":{{"role":"user"}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","timestamp":"{ts}","message":{{"usage":{{"input_tokens":{input},"cache_creation_input_tokens":{third},"cache_read_input_tokens":{cached},"output_tokens":{third}}}}}}}"#
        )
        .unwrap();
        path
    }

    /// Appends another turn to an existing transcript, the way the host
    /// really does — the file accumulates across a session rather than
    /// being rewritten per turn.
    fn append_turn(&self, path: &Path, fresh: u64) {
        let ts = OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        let third = fresh / 3;
        let input = fresh - 2 * third;
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","timestamp":"{ts}","message":{{"usage":{{"input_tokens":{input},"cache_creation_input_tokens":{third},"cache_read_input_tokens":0,"output_tokens":{third}}}}}}}"#
        )
        .unwrap();
    }

    fn finalize(&mut self, session_id: &str, transcript: Option<&Path>) -> FinalizeResult {
        match self.send(Request::Finalize {
            session_id: session_id.to_string(),
            model: None,
            provider: Some("claude-code".to_string()),
            transcript_path: transcript.map(|p| p.to_string_lossy().into_owned()),
        }) {
            Response::Finalize(FinalizeOutcome::Finalized(result)) => *result,
            other => panic!("expected a Finalized outcome, got {other:?}"),
        }
    }

    /// One complete governed turn whose host really used `fresh` tokens.
    fn turn(&mut self, session_id: &str, fresh: u64) -> FinalizeResult {
        let preflight = self.preflight(session_id);
        Self::assert_admitted(&preflight);
        let transcript = self.write_transcript(session_id, fresh, fresh * 10);
        self.finalize(session_id, Some(&transcript))
    }

    /// The share `Status` would render for the active task right now.
    fn posture(&mut self) -> Option<BudgetPosture> {
        match self.send(Request::Status) {
            Response::Status(result) => result.task_budget,
            other => panic!("expected a Status response, got {other:?}"),
        }
    }

    /// Every reservation amount on this store, which is the exact query
    /// that exposed the live defect.
    fn distinct_reservation_amounts(&self) -> Vec<f64> {
        let conn = rusqlite::Connection::open(&self.config.ledger_path).unwrap();
        let mut stmt = conn
            .prepare("SELECT DISTINCT amount FROM reservations ORDER BY amount")
            .unwrap();
        let rows: Vec<f64> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        rows
    }

    fn assert_admitted(preflight: &libra_governor_protocol::PreflightResult) {
        assert_eq!(
            preflight.admission.as_ref().map(|d| &d.admission),
            Some(&Admission::Admit),
            "precondition: these scenarios are about an admitted task's envelope"
        );
    }
}

fn fraction_left(posture: Option<BudgetPosture>) -> f64 {
    match posture {
        Some(BudgetPosture::Remaining { fraction_left }) => fraction_left,
        other => panic!("expected a Remaining posture with a real share, got {other:?}"),
    }
}

// --- AC 6: settlement records a measurement, or says it did not -------

/// The defect itself, at its narrowest: a receipt from a turn whose host
/// transcript was readable must carry the measured amount. An empty
/// `actual_usage` here is the whole bug.
#[test]
fn a_receipt_records_the_measured_token_usage_of_its_own_turn() {
    let mut harness = Harness::new();
    let receipt = harness.turn("measured-session", SMALL_TURN_TOKENS).receipt;

    assert_eq!(
        receipt.actual_usage,
        vec![ResourceAmount::Tokens(SMALL_TURN_TOKENS)],
        "the host recorded {SMALL_TURN_TOKENS} fresh tokens for this turn; a receipt that \
         carries no usage at all is what kept every estimate's resource dimension null"
    );
}

/// The exclusion is a semantic choice, so it is pinned rather than left
/// to the reader: cache reads re-present an already-ingested prefix, and
/// summing them counts the same context once per turn — about 25x
/// inflation on this workstation's measured transcripts.
#[test]
fn a_cached_prefix_reread_is_not_counted_as_fresh_spend() {
    let mut harness = Harness::new();
    // `turn` writes ten times the fresh amount as cache_read.
    let receipt = harness.turn("cached-session", SMALL_TURN_TOKENS).receipt;

    assert_eq!(
        receipt.actual_usage,
        vec![ResourceAmount::Tokens(SMALL_TURN_TOKENS)],
        "the transcript carried {} cached-prefix tokens alongside {SMALL_TURN_TOKENS} fresh \
         ones; counting them would bill the same context once per turn",
        SMALL_TURN_TOKENS * 10
    );
}

/// The second branch of AC 6, and the honest half. Codex's `Stop` payload
/// exposes no transcript, so there is nothing to measure — that case must
/// record the absence rather than invent a zero, and the reservation must
/// settle conservatively with `usage_known == Some(false)`.
#[test]
fn a_turn_with_no_host_transcript_records_an_unobserved_estimate_not_a_zero() {
    let mut harness = Harness::new();
    let preflight = harness.preflight("no-transcript-session");
    Harness::assert_admitted(&preflight);
    let receipt = harness.finalize("no-transcript-session", None).receipt;

    assert!(
        receipt.actual_usage.is_empty(),
        "with no transcript there is nothing to measure, and an invented amount would be \
         worse than none: got {:?}",
        receipt.actual_usage
    );

    let settled: Vec<_> = harness
        .ledger
        .reservations_for_task(receipt.task_id)
        .unwrap()
        .into_iter()
        .filter(|r| r.state == ReservationState::Settled)
        .collect();
    assert!(
        !settled.is_empty(),
        "precondition: the turn's work envelope must have been settled"
    );
    for reservation in settled {
        assert_eq!(
            reservation.usage_known,
            Some(false),
            "an unmeasured settlement must be recorded as unobserved, not as a measurement"
        );
    }
}

/// And the measured counterpart: `usage_known == Some(true)` is the
/// provenance flag every downstream consumer reads, and before this
/// ticket nothing could ever set it.
#[test]
fn a_measured_turn_settles_its_reservation_as_observed() {
    let mut harness = Harness::new();
    let receipt = harness.turn("observed-session", SMALL_TURN_TOKENS).receipt;

    let settled: Vec<_> = harness
        .ledger
        .reservations_for_task(receipt.task_id)
        .unwrap()
        .into_iter()
        .filter(|r| r.state == ReservationState::Settled)
        .collect();
    assert!(
        !settled.is_empty(),
        "precondition: the turn's work envelope must have been settled"
    );
    for reservation in settled {
        assert_eq!(
            reservation.usage_known,
            Some(true),
            "a turn with a readable transcript settled as unobserved, so the measurement \
             never reached the ledger"
        );
        assert_eq!(
            reservation.settled_amount,
            Some(ResourceAmount::Tokens(SMALL_TURN_TOKENS)),
            "and it must settle at the measured amount, not at the reserved constant"
        );
    }
}

/// What a measurement is a measurement *of*, upper bound: no turn may be
/// billed twice. `Stop` fires once per turn, and a window reaching back
/// to the session's start would inflate every receipt by the sum of its
/// predecessors.
///
/// The second turn here is deliberately the *smaller* one: an
/// implementation that re-counted from the start would report
/// `SMALL + LARGE` and so land above the first receipt, which a
/// monotonic-growth assertion alone would not distinguish from a correct
/// larger second turn.
#[test]
fn successive_turns_do_not_recount_the_previous_turns_tokens() {
    let mut harness = Harness::new();

    let first = harness.preflight("multi-turn");
    Harness::assert_admitted(&first);
    let transcript = harness.write_transcript("multi-turn", LARGE_TURN_TOKENS, 0);
    let first_receipt = harness.finalize("multi-turn", Some(&transcript)).receipt;
    assert_eq!(
        first_receipt.actual_usage,
        vec![ResourceAmount::Tokens(LARGE_TURN_TOKENS)],
        "precondition: the first turn must itself be measured"
    );

    // A second prompt in the same session reuses the session's task, so
    // this is the path along which a start-anchored window accumulates.
    let second = harness.preflight("multi-turn");
    assert_eq!(
        second.task_id, first_receipt.task_id,
        "precondition: a second prompt in one session must reuse the session's task"
    );
    harness.append_turn(&transcript, SMALL_TURN_TOKENS);
    let second_receipt = harness.finalize("multi-turn", Some(&transcript)).receipt;

    assert_eq!(
        second_receipt.actual_usage,
        vec![ResourceAmount::Tokens(SMALL_TURN_TOKENS)],
        "the second turn used {SMALL_TURN_TOKENS} tokens; recording \
         {} would bill the first turn twice and poison every quantile computed from it",
        SMALL_TURN_TOKENS + LARGE_TURN_TOKENS
    );
}

/// And the lower bound, which is the half the obvious implementation gets
/// wrong. The turn's own `plan_lineage_started_at` — the anchor
/// HORO-1723 established for duration — also avoids double-counting, so
/// the test above passes against it too. What it does not do is cover the
/// gap: a host keeps writing assistant records after `Stop` fires (a
/// compaction, a continuation, a subagent finishing), and those tokens
/// predate the next turn's plan. Anchored at the plan, they are
/// attributed to no receipt at all and vanish from the estimator's
/// history.
///
/// So the windows must *tile* the session, not merely avoid overlapping
/// it. The between-turns record here is larger than the turn it precedes,
/// so dropping it cannot be mistaken for rounding.
#[test]
fn spend_recorded_between_two_turns_is_attributed_to_the_next_receipt() {
    let mut harness = Harness::new();

    let first = harness.preflight("gap-session");
    Harness::assert_admitted(&first);
    let transcript = harness.write_transcript("gap-session", SMALL_TURN_TOKENS, 0);
    let first_receipt = harness.finalize("gap-session", Some(&transcript)).receipt;
    assert_eq!(
        first_receipt.actual_usage,
        vec![ResourceAmount::Tokens(SMALL_TURN_TOKENS)],
        "precondition: the first turn must itself be measured"
    );

    // After `Stop`, before the next prompt: real tokens, no governed plan
    // in flight to attribute them to yet.
    harness.append_turn(&transcript, LARGE_TURN_TOKENS);

    let second = harness.preflight("gap-session");
    assert_eq!(
        second.task_id, first_receipt.task_id,
        "precondition: a second prompt in one session must reuse the session's task"
    );
    harness.append_turn(&transcript, SMALL_TURN_TOKENS);
    let second_receipt = harness.finalize("gap-session", Some(&transcript)).receipt;

    assert_eq!(
        second_receipt.actual_usage,
        vec![ResourceAmount::Tokens(LARGE_TURN_TOKENS + SMALL_TURN_TOKENS)],
        "the host recorded {LARGE_TURN_TOKENS} tokens after the first Stop and \
         {SMALL_TURN_TOKENS} during the second turn; a window anchored at this turn's plan \
         reports only {SMALL_TURN_TOKENS} and loses the rest, which biases every quantile \
         computed from this history low"
    );
}

/// One aggregate measurement, two envelopes to charge it to, and no
/// evidence for how to divide it. The measurement is a single number for
/// the whole window — the transcript does not say which reservation each
/// token belonged to — so attributing it to both would double-count the
/// same spend, and splitting it on a ratio would write a precise-looking
/// figure no observation supports. Both settle conservatively instead,
/// which is visible as `usage_known == Some(false)` rather than silent.
///
/// Reachable in normal operation: per-request gateway reservations,
/// subaccount leases (HORO-1668) and optional-work envelopes all add
/// rows to a plan that already holds its work envelope.
#[test]
fn a_plan_with_two_active_reservations_settles_conservatively() {
    use libra_governor_domain::ReservationClass;
    use libra_governor_ledger::{ReserveOutcome, ReserveRequest};

    let mut harness = Harness::new();
    let preflight = harness.preflight("ambiguous-session");
    Harness::assert_admitted(&preflight);

    // A second envelope on the same plan, alongside the work envelope
    // `handle_preflight` already reserved.
    let outcome = harness
        .ledger
        .reserve(ReserveRequest {
            task_id: preflight.task_id,
            session_id: "ambiguous-session",
            plan_id: Some(preflight.plan_id),
            class: ReservationClass::OptionalWork,
            amount: ResourceAmount::Tokens(1_000),
            idempotency_key: "test:second-envelope",
            now: OffsetDateTime::now_utc(),
            ttl_secs: 900,
        })
        .unwrap();
    assert!(
        matches!(outcome, ReserveOutcome::Granted(_)),
        "precondition: the second envelope must really have been granted, got {outcome:?}"
    );
    let active_before = harness
        .ledger
        .reservations_for_task(preflight.task_id)
        .unwrap()
        .into_iter()
        .filter(|r| r.plan_id == Some(preflight.plan_id) && r.state == ReservationState::Active)
        .count();
    assert_eq!(
        active_before, 2,
        "precondition: this scenario needs two concurrent envelopes on one plan"
    );

    let transcript = harness.write_transcript("ambiguous-session", SMALL_TURN_TOKENS, 0);
    let receipt = harness
        .finalize("ambiguous-session", Some(&transcript))
        .receipt;

    // The receipt still records the real aggregate: what is ambiguous is
    // attribution between envelopes, not how much the turn used.
    assert_eq!(
        receipt.actual_usage,
        vec![ResourceAmount::Tokens(SMALL_TURN_TOKENS)],
        "the window's total is observed regardless of how many envelopes are open"
    );

    let settled: Vec<_> = harness
        .ledger
        .reservations_for_task(receipt.task_id)
        .unwrap()
        .into_iter()
        .filter(|r| r.state == ReservationState::Settled)
        .collect();
    assert_eq!(
        settled.len(),
        2,
        "precondition: both envelopes must have been settled by Finalize"
    );
    for reservation in settled {
        assert_eq!(
            reservation.usage_known,
            Some(false),
            "charging one aggregate measurement to each of two envelopes books the same \
             {SMALL_TURN_TOKENS} tokens twice; with no evidence for a split, the honest \
             record is that usage was not observed per envelope"
        );
    }
}

/// A token count is not a price, and an envelope denominated in money
/// must not be settled with one. `ResourceAmount`'s own module docs exist
/// to stop exactly this: mixing USD cents, raw tokens and quota
/// percentages must never be silently summed.
///
/// The ledger already refuses the mismatch (`settle` returns
/// `ResourceKindMismatch`), so offering it anyway would not quietly
/// corrupt the books — it would fail the whole `Finalize`, losing the
/// receipt and the duration measurement with it. The measurement is
/// kind-matched before it is offered, and this task settles as
/// unobserved, which is the truth: nothing here measured dollars.
#[test]
fn a_token_measurement_is_not_offered_to_a_usd_denominated_envelope() {
    let usd_policy = Policy::validated(
        "measured-resource-usage-usd",
        ResourceBound {
            mode: ConstraintMode::Elastic,
            target: ResourceAmount::UsdCents(500),
            elastic_ceiling: Some(ResourceAmount::UsdCents(1_500)),
            hard_ceiling: ResourceAmount::UsdCents(5_000),
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
    .unwrap();

    let mut harness = Harness::with_policy(usd_policy);
    let preflight = harness.preflight("usd-session");
    Harness::assert_admitted(&preflight);
    let transcript = harness.write_transcript("usd-session", SMALL_TURN_TOKENS, 0);

    // A `Finalize` that errored would come back as something other than
    // `Finalized`, and `Harness::finalize` panics on that — so reaching
    // the assertions below is itself part of the proof.
    let receipt = harness.finalize("usd-session", Some(&transcript)).receipt;

    assert_eq!(
        receipt.actual_usage,
        vec![ResourceAmount::Tokens(SMALL_TURN_TOKENS)],
        "the measurement is still recorded in the kind it was measured in"
    );

    let settled: Vec<_> = harness
        .ledger
        .reservations_for_task(receipt.task_id)
        .unwrap()
        .into_iter()
        .filter(|r| r.state == ReservationState::Settled)
        .collect();
    assert!(
        !settled.is_empty(),
        "precondition: the USD work envelope must have been settled"
    );
    for reservation in settled {
        assert_eq!(
            reservation.amount.kind(),
            ResourceKind::Usd,
            "precondition: this scenario needs a non-token envelope"
        );
        assert_eq!(
            reservation.usage_known,
            Some(false),
            "a token count cannot stand in as a measurement of dollars"
        );
    }
}

// --- AC 5: resource_p80 is not unconditionally None -------------------

/// The permanent regression AC 5 asks for. It fails against merged main,
/// where `resource_p80` is `None` for every task no matter how much
/// history exists.
#[test]
fn a_task_with_measured_history_gets_a_real_resource_p80() {
    let mut harness = Harness::new();
    harness.turn("history-1", SMALL_TURN_TOKENS);

    let next = harness.preflight("history-2");
    let estimate = next.estimate.expect("a preflight always carries an estimate");

    let p80 = estimate.resource_p80.unwrap_or_else(|| {
        panic!(
            "resource_p80 is None for a task with {} usage-bearing receipts of history; the \
             estimate has no resource dimension at all, so admission falls back to the policy \
             target for every task forever",
            1
        )
    });
    assert_eq!(
        p80.kind(),
        ResourceKind::Tokens,
        "the quantile must be denominated in the kind that was measured"
    );
    assert_eq!(
        p80,
        ResourceAmount::Tokens(SMALL_TURN_TOKENS),
        "one sample's P80 is that sample"
    );
}

// --- AC 2 / AC 4: the reserved amount tracks observed scope -----------

/// AC 2 and AC 4 together, in the shape the live store exposed. Two
/// tasks whose observed histories differ by 20x must not reserve the
/// same envelope, and the store must not collapse to one distinct
/// amount.
///
/// The bucketing is what makes the two histories separate: `repo_key`
/// differs between the two `cwd`s, so each preflight's `RepoTopology`
/// tier sees only its own repo's receipts once five have accumulated
/// (`MIN_CLASS_SAMPLES`). Below that threshold both would pool into the
/// global tier and legitimately agree — which is why each side is seeded
/// five times rather than once.
#[test]
fn two_task_classes_with_different_measured_history_reserve_different_envelopes() {
    let mut harness = Harness::new();
    let heavy_repo = harness.dir.path().join("heavy_repo");
    std::fs::create_dir_all(heavy_repo.join("src")).unwrap();
    std::fs::write(heavy_repo.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(heavy_repo.join("Cargo.toml"), "[package]\nname = \"h\"\n").unwrap();

    for i in 0..5 {
        let session = format!("light-{i}");
        let preflight = harness.preflight(&session);
        Harness::assert_admitted(&preflight);
        let transcript = harness.write_transcript(&session, SMALL_TURN_TOKENS, 0);
        harness.finalize(&session, Some(&transcript));
    }
    for i in 0..5 {
        let session = format!("heavy-{i}");
        let preflight = harness.preflight_in(&session, heavy_repo.clone());
        Harness::assert_admitted(&preflight);
        let transcript = harness.write_transcript(&session, LARGE_TURN_TOKENS, 0);
        harness.finalize(&session, Some(&transcript));
    }

    let light = harness.preflight("light-probe");
    Harness::assert_admitted(&light);
    let light_p80 = light
        .estimate
        .as_ref()
        .and_then(|e| e.resource_p80)
        .expect("the light class has five usage-bearing receipts of history");
    let light_share = fraction_left(harness.posture());

    let heavy = harness.preflight_in("heavy-probe", heavy_repo.clone());
    Harness::assert_admitted(&heavy);
    let heavy_p80 = heavy
        .estimate
        .as_ref()
        .and_then(|e| e.resource_p80)
        .expect("the heavy class has five usage-bearing receipts of history");
    let heavy_share = fraction_left(harness.posture());

    assert!(
        heavy_p80.as_f64() > light_p80.as_f64() * 5.0,
        "two task classes whose hosts really used {SMALL_TURN_TOKENS} and \
         {LARGE_TURN_TOKENS} tokens estimated {light_p80:?} and {heavy_p80:?}; the estimate \
         is not tracking observed scope"
    );

    // AC 3: the rendered share, not just the internal estimate.
    assert!(
        heavy_share < light_share,
        "the heavy task reserves a larger envelope, so less of its hard limit can remain: \
         light {light_share}, heavy {heavy_share}"
    );

    // AC 4: the query that exposed the defect on the live store.
    let amounts = harness.distinct_reservation_amounts();
    assert!(
        amounts.len() > 1,
        "every reservation on this store has the same amount ({amounts:?}); that is the live \
         defect exactly — `select distinct amount from reservations` returned one row across \
         57 receipts, and the rendered share never moved"
    );
}

/// The specific frozen value, named. An implementation that ignored
/// measurement would reserve `POLICY_TARGET - completion_reserve` for a
/// task whose history says otherwise, so the assertion is that the
/// envelope moved *off* the fallback rather than merely that two numbers
/// differ.
#[test]
fn a_measured_task_does_not_reserve_the_policy_target_fallback() {
    let mut harness = Harness::new();
    harness.turn("heavy-history", LARGE_TURN_TOKENS);

    let next = harness.preflight("heavy-next");
    Harness::assert_admitted(&next);
    let reserved: Vec<_> = harness
        .ledger
        .reservations_for_task(next.task_id)
        .unwrap()
        .into_iter()
        .filter(|r| r.state == ReservationState::Active)
        .collect();
    assert_eq!(
        reserved.len(),
        1,
        "precondition: an admitted plan holds exactly one work envelope"
    );
    let envelope = reserved[0].amount.as_f64();

    assert!(
        envelope > POLICY_TARGET as f64,
        "history says this class really uses {LARGE_TURN_TOKENS} tokens, but the envelope is \
         {envelope} — at or below the {POLICY_TARGET}-token policy target every task used to \
         fall back to"
    );
}
