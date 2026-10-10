//! HORO-1723: what `actual_duration_secs` on a receipt is a measurement
//! *of*.
//!
//! The daemon used to derive it from `session_started_at` — the one
//! `session_tasks.created_at` row, which `resolve_or_create_task_for_session`
//! writes exactly once per session and never updates. So every receipt
//! recorded the age of the host session rather than the duration of the
//! work: successive `Stop`s in one long session each reported a larger
//! number than the last, and the figure included all of the operator's
//! idle time between turns. On the workstation where this was found, a
//! receipt for a task with 91 tool calls recorded 608,465 seconds — about
//! 169 hours.
//!
//! That is not cosmetic. Those receipts are the estimator's samples, so
//! the same long-lived session poisoned history repeatedly, the duration
//! P90 for an ordinary task landed in the hundreds of thousands of
//! seconds, and admission then returned `Deny(TimeExceedsHardCeiling)` —
//! which is why no live task ever held a reservation, which is what
//! HORO-1708 had to report as an unknown budget.
//!
//! **The shape that catches this is a session materially older than the
//! work.** The error equals the session's age, so it is identically zero
//! wherever a session is seconds old — which is every pre-existing test
//! and every CI run. That is precisely how a fully green suite shipped
//! it, and why each test below seeds the session's task through
//! `resolve_or_create_task_for_session` with a backdated timestamp before
//! the first `Preflight`. The daemon's own `Preflight` then early-returns
//! that existing task and writes a plan at the current time, reproducing
//! the live shape exactly: an old session, a fresh turn.
//!
//! The clock itself is not injectable into `handle_finalize`, so these
//! tests assert the duration is *small* rather than exact. They are not
//! loose for it: the gap between the correct answer (under a second) and
//! the old one (hours to days) is six orders of magnitude, and the bound
//! each test uses is stated against the policy ceiling the inflated
//! figure used to breach.
//!
//! Which *row* the anchor comes from is pinned separately and exactly, by
//! `libra_governor_ledger::query::plan_lineage_tests`, where every
//! timestamp is an explicit offset from a fixed base.

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use libra_governor_daemon::{recon::ReconBudget, DaemonConfig};
use libra_governor_domain::{
    Admission, AutonomyBoundary, CompletionContract, CompletionCriterion, Confidence,
    ConstraintMode, Policy, ReplanHysteresisConfig, ResourceAmount, ResourceBound, TimeBound,
};
use libra_governor_ledger::LedgerStore;
use libra_governor_protocol::{
    wire, FinalizeOutcome, FinalizeResult, ReplanState, Request, RequestEnvelope, Response,
    ResponseEnvelope, PROTOCOL_VERSION,
};
use time::{Duration, OffsetDateTime};

/// The hard time ceiling the policy below declares, and the number the
/// live defect blew past by two orders of magnitude. Every duration
/// assertion is stated against it rather than against a bare constant,
/// because breaching it is the specific economic consequence — admission
/// returns `Deny(TimeExceedsHardCeiling)` and the task gets no
/// reservation at all.
const HARD_CEILING_SECS: u64 = 5400;

/// How old each test's session is before its first turn starts. Taken
/// from the live receipt that exposed this (608,465 s), rounded down:
/// long enough that no plausible test-execution time could be mistaken
/// for it.
fn session_age() -> Duration {
    Duration::hours(169)
}

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

/// A policy that admits a cold-start preflight, so the finalized half of
/// the lifecycle is reachable. `Confidence::Low` because a cold-start
/// estimate always carries `Low` and a higher floor would Deny on
/// confidence before anything here was exercised — the same reasoning
/// `reservation_integration.rs::generous_policy` records.
fn admitting_policy() -> Policy {
    Policy::validated(
        "task-scoped-duration",
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
            hard_ceiling_secs: Some(HARD_CEILING_SECS),
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
    _dir: tempfile::TempDir,
    config: DaemonConfig,
    ledger: LedgerStore,
    current_task: Option<libra_governor_protocol::TaskSummary>,
    listener: std::os::unix::net::UnixListener,
    socket_path: PathBuf,
}

impl Harness {
    fn new(replan_hysteresis: ReplanHysteresisConfig) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = DaemonConfig {
            socket_path: dir.path().join("d.sock"),
            ledger_path: dir.path().join("ledger.sqlite3"),
            log_path: dir.path().join("daemon.log"),
            recon_budget: ReconBudget::default(),
            replan_hysteresis,
            policy: admitting_policy(),
            reservation_ttl_secs: 900,
            gateway: None,
            gateway_stats: std::sync::Arc::new(Default::default()),
            gateway_session_header: libra_governor_gateway::proxy::DEFAULT_SESSION_HEADER
                .to_string(),
            extensions: None,
            extension_runtime: std::sync::OnceLock::new(),

            outcome_authority: None,
            progressive_interval_secs: libra_governor_daemon::DEFAULT_PROGRESSIVE_INTERVAL_SECS,
        };
        let ledger = LedgerStore::open(&config.ledger_path).unwrap();
        let listener = libra_governor_daemon::bind_or_detect_running(&config.socket_path).unwrap();
        let socket_path = dir.path().join("d.sock");
        Self {
            _dir: dir,
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

    /// Binds `session_id` to a task that was created `session_age()` ago,
    /// through the same public call the daemon's own `Preflight` uses.
    /// Because that call early-returns an already-resolved session, the
    /// subsequent `Preflight` adopts this aged task rather than creating
    /// a fresh one — which is the live shape, and the only shape in which
    /// this defect is observable at all.
    fn seed_aged_session(&mut self, session_id: &str) {
        let backdated = OffsetDateTime::now_utc() - session_age();
        self.ledger
            .resolve_or_create_task_for_session(session_id, backdated)
            .unwrap();
        assert_eq!(
            self.ledger.session_started_at(session_id).unwrap(),
            Some(backdated),
            "precondition: the session must really be backdated, or this test proves nothing"
        );
    }

    fn preflight(&mut self, session_id: &str) -> libra_governor_protocol::PreflightResult {
        match self.send(Request::Preflight {
            task_hint: "fix the login bug".to_string(),
            cwd: fixture_repo(),
            session_id: session_id.to_string(),
        }) {
            Response::Preflight(result) => *result,
            other => panic!("expected a Preflight response, got {other:?}"),
        }
    }

    fn finalize(&mut self, session_id: &str) -> FinalizeResult {
        match self.send(Request::Finalize {
            session_id: session_id.to_string(),
            model: None,
            provider: None,
            transcript_path: None,
        }) {
            Response::Finalize(FinalizeOutcome::Finalized(result)) => *result,
            other => panic!("expected a Finalized outcome, got {other:?}"),
        }
    }

    /// Asserts the preflight was admitted, so a test that depends on the
    /// admitted path says so rather than passing against a denied one.
    fn assert_admitted(preflight: &libra_governor_protocol::PreflightResult) {
        assert_eq!(
            preflight.admission.as_ref().map(|d| &d.admission),
            Some(&Admission::Admit),
            "precondition: this scenario is about an admitted task's recorded duration"
        );
    }
}

// --- the HORO-1723 regression proper -----------------------------------

#[test]
fn a_long_lived_session_does_not_inflate_a_fresh_turns_duration() {
    let mut harness = Harness::new(ReplanHysteresisConfig::default());
    harness.seed_aged_session("aged-session");

    let preflight = harness.preflight("aged-session");
    Harness::assert_admitted(&preflight);

    let recorded = harness
        .finalize("aged-session")
        .receipt
        .actual_duration_secs;

    // The old anchor would put this at ~608,400 — the session's age.
    assert!(
        recorded < HARD_CEILING_SECS,
        "a turn that has just started must not be recorded as {recorded}s of work in a \
         {}h-old session; anything at or above the {HARD_CEILING_SECS}s hard time ceiling \
         becomes an estimator sample that denies the next task on TimeExceedsHardCeiling",
        session_age().whole_hours(),
    );
    // Tighter than the ceiling, to catch a partial fix that merely
    // divided the session's age down rather than stopping measuring it.
    assert!(
        recorded < 300,
        "a turn with one preflight and no tool calls took {recorded}s of wall clock, \
         which is not a plausible measurement of this test's own runtime"
    );
}

/// The mirror-image dishonesty, and the only assertion in this file with
/// a *lower* bound. Every other test here bounds the duration from above,
/// and an implementation that simply reported `0` would satisfy all of
/// them while measuring nothing at all — a receipt claiming a turn took
/// no time is as false as one claiming it took 169 hours, and it would
/// drag the estimator's P90 toward zero instead of toward infinity.
///
/// The sleep is what makes the property observable: with a one-second
/// resolution there is no way to distinguish "measured a short turn" from
/// "did not measure" unless the turn is at least a second long. 1.1 s
/// gives a 100 ms margin on the truncation boundary.
#[test]
fn a_turn_that_really_took_time_reports_a_nonzero_duration() {
    let mut harness = Harness::new(ReplanHysteresisConfig::default());
    harness.seed_aged_session("measurable-session");

    let preflight = harness.preflight("measurable-session");
    Harness::assert_admitted(&preflight);
    std::thread::sleep(std::time::Duration::from_millis(1100));

    let recorded = harness
        .finalize("measurable-session")
        .receipt
        .actual_duration_secs;

    assert!(
        recorded >= 1,
        "a turn that spent at least 1.1s between Preflight and Finalize recorded \
         {recorded}s; the duration is not being measured at all"
    );
    assert!(
        recorded < 300,
        "and it must still be the turn's duration, not the session's age: got {recorded}s"
    );
}

#[test]
fn successive_turns_in_one_session_do_not_report_growing_durations() {
    let mut harness = Harness::new(ReplanHysteresisConfig::default());
    harness.seed_aged_session("multi-turn-session");

    let first = harness.preflight("multi-turn-session");
    Harness::assert_admitted(&first);
    let first_duration = harness
        .finalize("multi-turn-session")
        .receipt
        .actual_duration_secs;

    // A second prompt in the same session. `resolve_or_create_task_for_session`
    // early-returns, so this is the *same* task with a new plan — the
    // exact path along which the old anchor accumulated.
    let second = harness.preflight("multi-turn-session");
    assert_eq!(
        second.task_id, first.task_id,
        "precondition: a second prompt in one session must reuse the session's task, \
         or this is not the accumulating path"
    );
    let second_duration = harness
        .finalize("multi-turn-session")
        .receipt
        .actual_duration_secs;

    assert!(
        second_duration < HARD_CEILING_SECS && first_duration < HARD_CEILING_SECS,
        "neither turn may be recorded as the session's age (first {first_duration}s, \
         second {second_duration}s)"
    );
    assert!(
        second_duration.saturating_sub(first_duration) < 300,
        "the second turn ({second_duration}s) must not be recorded as longer than the \
         first ({first_duration}s) merely because the session has aged between them"
    );
}

/// A replan supersedes the in-flight plan mid-turn, so the plan being
/// finalized is not the one the turn started with. The duration must
/// cover the whole turn — anchoring to the replacement plan alone would
/// silently discard however long the pre-replan work took — while still
/// excluding the session's age.
///
/// Both halves need to be observable at once, so the turn's root plan is
/// backdated by ten minutes after the replan fires. Without that, every
/// plan in a test is created within the same second and the two wrong
/// answers ("measure from the replan" and "measure from the turn's
/// start") are indistinguishable; the assertion would pass against
/// either. The backdating is a direct `UPDATE` over a second connection
/// to the same ledger file because no public API sets a plan's
/// `created_at` after the fact — and nothing in the daemon should ever
/// gain one.
#[test]
fn a_replanned_turn_is_measured_from_the_turns_start_not_from_the_replan() {
    let mut harness = Harness::new(ReplanHysteresisConfig {
        cooldown_secs: 0,
        ..ReplanHysteresisConfig::default()
    });
    harness.seed_aged_session("replan-session");

    let preflight = harness.preflight("replan-session");
    Harness::assert_admitted(&preflight);

    // A streak of identical tool calls trips `PossibleToolLoop`, the
    // cheapest real replan trigger (`replan_integration.rs` owns the
    // trigger semantics themselves).
    for _ in 0..4 {
        harness.send(Request::ToolInvoked {
            session_id: "replan-session".to_string(),
            tool_name: "Bash".to_string(),
        });
    }

    let summary = match harness.send(Request::Status) {
        Response::Status(result) => result.current_task.expect("an active task"),
        other => panic!("expected a Status response, got {other:?}"),
    };
    assert_eq!(
        summary.replan_state,
        ReplanState::Replanned { count: 1 },
        "precondition: this scenario is about finalizing a plan that replaced another one"
    );
    assert_ne!(
        summary.plan_id, preflight.plan_id,
        "precondition: a replan must have produced a different plan id"
    );

    // Move the turn's *root* plan ten minutes into the past, leaving the
    // replacement plan where it is. The turn is now unambiguously ten
    // minutes long, and it is still nothing like the session's age.
    const ROOT_AGE_SECS: u64 = 600;
    let ledger_path = harness.config.ledger_path.clone();
    let backdated = OffsetDateTime::now_utc() - Duration::seconds(ROOT_AGE_SECS as i64);
    let conn = rusqlite::Connection::open(&ledger_path).unwrap();
    let rows = conn
        .execute(
            "UPDATE plans SET created_at = ?1 WHERE id = ?2",
            rusqlite::params![
                backdated
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap(),
                preflight.plan_id.0.to_string(),
            ],
        )
        .unwrap();
    assert_eq!(
        rows, 1,
        "precondition: the root plan row must have been backdated"
    );
    drop(conn);

    let recorded = harness
        .finalize("replan-session")
        .receipt
        .actual_duration_secs;

    assert!(
        recorded >= ROOT_AGE_SECS - 60,
        "the turn began {ROOT_AGE_SECS}s ago and was replanned part-way through; recording \
         only {recorded}s discards the pre-replan work, which is the half of this turn the \
         operator already paid for"
    );
    assert!(
        recorded < HARD_CEILING_SECS,
        "and a replanned turn in a {}h-old session must still not be recorded as {recorded}s",
        session_age().whole_hours(),
    );
    assert!(
        recorded < ROOT_AGE_SECS + 300,
        "the turn took {ROOT_AGE_SECS}s; {recorded}s is measuring something larger than it"
    );
}
