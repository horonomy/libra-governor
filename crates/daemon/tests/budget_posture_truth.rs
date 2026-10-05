//! HORO-1708: what a `Status` reply is allowed to claim about a governed
//! task's budget.
//!
//! The defect this file exists to prevent is not "the percentage was too
//! high". It is that the daemon converted *absence of economic
//! observation* into a positive claim of a full envelope. A task holding
//! no reservation in any state has settled spend and active holds of
//! zero, so the canonical share — `(hard_limit - settled - active) /
//! hard_limit`, the formula `LedgerStore::available` owns — comes out at
//! exactly `1.0`, and the statusline rendered "100% budget left".
//!
//! That is not a rounding artifact or a rare edge. A plan whose admission
//! returns `Deny` or `ApprovalRequired` gets no plan-level reservation by
//! design (`reservation_integration.rs` owns that property directly), so
//! every task in that state reported a pristine envelope for its entire
//! life. On the workstation where this was found, *every* live task was
//! in that state.
//!
//! These tests go through `handle_connection` over a real socket against
//! a real `LedgerStore`, following the pattern
//! `reservation_integration.rs` and `replan_integration.rs` established,
//! because the bug was in the *derivation* of the posture from the
//! ledger. Before this ticket, `budget_posture` had no test of any kind:
//! the provider's rendering was covered against hand-constructed
//! postures, which is precisely the shape of coverage that cannot catch a
//! daemon that builds the wrong posture faithfully.
//!
//! Two properties are asserted negatively on purpose, because they are
//! the ones a plausible future refactor would quietly destroy:
//!
//! * a denied task must not report `Remaining`, whatever the share — a
//!   test that only asserted `Uncommitted` would pass against code that
//!   returned `Remaining { fraction_left: 1.0 }` for an *admitted* task
//!   that had somehow lost its reservation; and
//! * an idle daemon must carry no budget field at all. There is no
//!   active-task denominator when no task is running, so a share of any
//!   value is a fabrication, and `100%` is the specific fabrication that
//!   would look like good news.
//!
//! What this file deliberately does *not* assert: that the settled
//! amounts reflect *measured* resource usage. They do not. Settlement
//! currently records the estimate at face value, and no measured usage
//! has ever reached this ledger — see the attribution limitation recorded
//! on HORO-1708 and HORO-1112. Every assertion below is about
//! reservation-level accounting, which is real accounting, and none of
//! them would pass if the share were fabricated.

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use libra_governor_daemon::{recon::ReconBudget, DaemonConfig};
use libra_governor_domain::{
    Admission, AutonomyBoundary, CompletionContract, CompletionCriterion, Confidence,
    ConstraintMode, Policy, PolicyPresetInputs, ReplanHysteresisConfig, ReservationClass,
    ResourceAmount, ResourceBound, TimeBound,
};
use libra_governor_ledger::{LedgerStore, ReserveOutcome, ReserveRequest};
use libra_governor_protocol::{
    wire, BudgetPosture, Request, RequestEnvelope, Response, ResponseEnvelope, StatusResult,
    PROTOCOL_VERSION,
};
use time::OffsetDateTime;

fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample_repo")
}

/// One full request/response round trip against a live listening daemon —
/// the same helper `reservation_integration.rs` uses, kept identical so
/// the two files exercise the same path rather than two approximations of
/// it.
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
        extensions: None,
        extension_runtime: std::sync::OnceLock::new(),
        progressive_interval_secs: libra_governor_daemon::DEFAULT_PROGRESSIVE_INTERVAL_SECS,
    }
}

/// A policy a fresh task is admitted under, so the admitted half of the
/// lifecycle is reachable. `Confidence::Low` because a cold-start
/// estimate always carries `Low` and a higher floor would Deny on
/// confidence before the resource path was ever consulted — the same
/// reasoning `reservation_integration.rs::generous_policy` records.
fn admitting_policy() -> Policy {
    Policy::validated(
        "budget-truth-admit",
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

/// A policy that Denies deterministically on the first preflight: a
/// cold-start estimate is always `Confidence::Low` and `strict_budget`
/// requires `High`, so no ledger setup is needed to reach the state that
/// produced the live defect.
fn denying_policy() -> Policy {
    Policy::strict_budget(PolicyPresetInputs {
        resource_target: ResourceAmount::Tokens(1000),
        time_target_secs: 600,
        quality_floor: CompletionContract::first(vec![CompletionCriterion::required(
            "required verification (tests/build/lint) passes",
        )]),
    })
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
    fn new(policy: Policy) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = base_config(dir.path(), policy);
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

    fn status(&mut self) -> StatusResult {
        match self.send(Request::Status) {
            Response::Status(result) => *result,
            other => panic!("expected a Status response, got {other:?}"),
        }
    }
}

/// The share, or a panic naming what came instead. Used where a test's
/// subject is how the number *moves*, so a non-`Remaining` posture is a
/// setup failure rather than the thing under test.
fn share(posture: Option<BudgetPosture>) -> f64 {
    match posture {
        Some(BudgetPosture::Remaining { fraction_left }) => fraction_left,
        other => panic!("expected a Remaining posture, got {other:?}"),
    }
}

// --- the HORO-1708 regression proper -----------------------------------

#[test]
fn a_task_with_no_reservation_reports_uncommitted_rather_than_a_full_envelope() {
    let mut harness = Harness::new(denying_policy());
    let preflight = harness.preflight("denied-session");

    // The precondition, asserted rather than assumed: this test is only
    // about the posture if the task really did reach the no-reservation
    // state. If a future admission change admitted this preflight, the
    // assertions below would be measuring a different scenario, and this
    // would say so instead of passing vacuously.
    assert!(
        matches!(
            preflight.admission.as_ref().map(|d| &d.admission),
            Some(Admission::Deny(_))
        ),
        "precondition: a cold-start estimate under a High-confidence floor must Deny, got {:?}",
        preflight.admission
    );
    assert!(
        harness
            .ledger
            .reservations_for_task(preflight.task_id)
            .unwrap()
            .is_empty(),
        "precondition: a denied plan must hold no reservation"
    );
    // And the envelope itself exists — so this is not `NotEstablished`
    // wearing a different name. The budget row is present and readable;
    // the only missing thing is a draw against it.
    let budget = harness
        .ledger
        .task_budget(preflight.task_id)
        .unwrap()
        .expect("precondition: a budget row must exist even for a denied plan");
    assert!(budget.hard_limit.as_f64() > 0.0);

    let status = harness.status();
    assert_eq!(
        status.task_budget,
        Some(BudgetPosture::Uncommitted),
        "a task nothing has drawn against must report Uncommitted"
    );

    // The negative half, and the actual regression: not merely "it is
    // Uncommitted" but "it is not a share". `Remaining { 1.0 }` is what
    // shipped, and it is what a reverted fix would produce again.
    assert!(
        !matches!(status.task_budget, Some(BudgetPosture::Remaining { .. })),
        "an unobserved envelope must not be reported as a measured share"
    );
}

#[test]
fn the_unobserved_share_the_old_code_would_have_computed_is_exactly_one() {
    // Anti-vacuity for the test above. It proves `Uncommitted` is
    // reported, but not that the thing it replaced was the "100%" claim —
    // so this reads the canonical formula directly and pins the number.
    // Without this, a future reader cannot tell whether the fix changed a
    // wrong percentage or suppressed a right one.
    let mut harness = Harness::new(denying_policy());
    let preflight = harness.preflight("denied-session");

    let headroom = harness
        .ledger
        .available(preflight.task_id, ReservationClass::RequiredWork)
        .unwrap()
        .expect("a denied task still has a readable envelope");
    let limit = harness
        .ledger
        .task_budget(preflight.task_id)
        .unwrap()
        .unwrap()
        .hard_limit
        .as_f64();

    assert_eq!(
        headroom.value / limit,
        1.0,
        "the canonical share of an undrawn envelope is exactly 1.0, which is \
         why rendering it as a measured share said '100% budget left'"
    );
    assert_eq!(
        harness.status().task_budget,
        Some(BudgetPosture::Uncommitted)
    );
}

#[test]
fn a_draw_that_settled_at_zero_cost_is_a_measured_full_envelope_not_an_unobserved_one() {
    // The case that makes "read the reservation rows" the right test and
    // "check whether the share is 1.0" the wrong one. A hold taken and
    // settled at zero actual cost leaves settled spend and active holds
    // at zero, so the canonical share is exactly 1.0 — identical to the
    // undrawn envelope's — but here the fullness was *observed*. The
    // ledger has rows; something happened and cost nothing.
    //
    // So `Remaining { 1.0 }` is the honest answer, and a shortcut
    // implementation that inferred emptiness from the number would report
    // this as unknown and lose a real measurement. This test is the only
    // thing in the file that distinguishes the two implementations.
    let mut harness = Harness::new(admitting_policy());
    let preflight = harness.preflight("zero-cost-session");
    let budget = harness
        .ledger
        .task_budget(preflight.task_id)
        .unwrap()
        .unwrap();

    // Release the plan's own work-envelope hold first, so the only
    // remaining rows are the ones this test settles.
    harness
        .ledger
        .release_active_for_plan(
            preflight.task_id,
            preflight.plan_id,
            OffsetDateTime::now_utc(),
        )
        .unwrap();
    let hold = match harness
        .ledger
        .reserve(ReserveRequest {
            task_id: preflight.task_id,
            session_id: "zero-cost-session",
            plan_id: None,
            class: ReservationClass::RequiredWork,
            amount: ResourceAmount::from_kind_f64(
                budget.resource_kind,
                budget.hard_limit.as_f64() / 8.0,
            ),
            idempotency_key: "budget-truth-zero-cost-hold",
            now: OffsetDateTime::now_utc(),
            ttl_secs: 900,
        })
        .unwrap()
    {
        ReserveOutcome::Granted(reservation) => *reservation,
        other => panic!("precondition: the hold must be granted, got {other:?}"),
    };
    harness
        .ledger
        .settle(
            hold.id,
            Some(ResourceAmount::from_kind_f64(budget.resource_kind, 0.0)),
            OffsetDateTime::now_utc(),
        )
        .unwrap();

    let headroom = harness
        .ledger
        .available(preflight.task_id, ReservationClass::RequiredWork)
        .unwrap()
        .unwrap();
    assert_eq!(
        headroom.value / budget.hard_limit.as_f64(),
        1.0,
        "precondition: a zero-cost settlement must restore the whole envelope, \
         giving the same 1.0 an undrawn envelope would"
    );
    assert!(
        !harness
            .ledger
            .reservations_for_task(preflight.task_id)
            .unwrap()
            .is_empty(),
        "precondition: the draw must have left rows behind"
    );

    assert_eq!(
        harness.status().task_budget,
        Some(BudgetPosture::Remaining { fraction_left: 1.0 }),
        "an envelope measured as fully available must be reported as a share, \
         not downgraded to unknown — the emptiness test is about whether \
         anything was drawn, not about the value of the share"
    );
}

// --- §8 lifecycle: idle -> active -> economic activity -> idle ---------

#[test]
fn an_idle_daemon_reports_no_budget_at_all() {
    // There is no active-task denominator when no task is running, so
    // every possible share is a fabrication — and `100%` is the one that
    // would read as good news. `None` is the only truthful answer.
    let mut harness = Harness::new(admitting_policy());
    let status = harness.status();
    assert_eq!(status.current_task, None);
    assert_eq!(
        status.task_budget, None,
        "an idle daemon must not synthesize a budget posture"
    );
}

#[test]
fn an_admitted_task_reports_a_share_below_one_from_its_first_status() {
    // The other side of the fix: `Uncommitted` must not swallow the
    // healthy case. An admitted plan has its work envelope reserved
    // before any `Status` can be answered, so the share is below one
    // immediately — which is also why a share of exactly 1.0 in practice
    // *means* "nothing was ever committed".
    let mut harness = Harness::new(admitting_policy());
    let preflight = harness.preflight("admitted-session");
    assert_eq!(
        preflight.admission.as_ref().map(|d| &d.admission),
        Some(&Admission::Admit),
        "precondition: this policy must admit a fresh task"
    );

    let fraction = share(harness.status().task_budget);
    assert!(
        fraction > 0.0 && fraction < 1.0,
        "an admitted task holds an active work-envelope reservation, so its \
         share must be strictly between zero and one, got {fraction}"
    );
}

#[test]
fn committing_more_capacity_moves_the_share_down() {
    // §8's "economic activity changes canonical state". The mutation this
    // kills is a constant: a `fraction_left` hardcoded to 1.0, or to any
    // fixed value, passes every single-reading assertion in this file and
    // fails here.
    let mut harness = Harness::new(admitting_policy());
    let preflight = harness.preflight("activity-session");
    let before = share(harness.status().task_budget);

    let budget = harness
        .ledger
        .task_budget(preflight.task_id)
        .unwrap()
        .unwrap();
    // A tenth of the envelope, through the ledger's own `reserve` — the
    // canonical way capacity is committed, not a hand-written row.
    let outcome = harness
        .ledger
        .reserve(ReserveRequest {
            task_id: preflight.task_id,
            session_id: "activity-session",
            plan_id: None,
            class: ReservationClass::RequiredWork,
            amount: ResourceAmount::from_kind_f64(
                budget.resource_kind,
                budget.hard_limit.as_f64() / 10.0,
            ),
            idempotency_key: "budget-truth-extra-hold",
            now: OffsetDateTime::now_utc(),
            ttl_secs: 900,
        })
        .unwrap();
    assert!(
        matches!(outcome, ReserveOutcome::Granted(_)),
        "precondition: the extra hold must be granted, got {outcome:?}"
    );

    let after = share(harness.status().task_budget);
    assert!(
        after < before,
        "committing a tenth of the envelope must reduce the share, got \
         {before} then {after}"
    );
}

#[test]
fn a_finished_task_leaves_no_budget_behind_in_idle() {
    // §8's "task ends": the task-specific share must disappear, not
    // linger as the last reading. A stale share is worse than no share —
    // it is attributed to whatever the user is doing next.
    let mut harness = Harness::new(admitting_policy());
    harness.preflight("finished-session");
    assert!(
        harness.status().task_budget.is_some(),
        "precondition: the task must have a posture before it finishes"
    );

    match harness.send(Request::Finalize {
        session_id: "finished-session".to_string(),
        model: None,
        provider: None,
        transcript_path: None,
    }) {
        Response::Finalize(_) => {}
        other => panic!("expected a Finalize response, got {other:?}"),
    }

    let status = harness.status();
    assert_eq!(
        status.current_task, None,
        "precondition: finalize must clear the current task"
    );
    assert_eq!(
        status.task_budget, None,
        "a finished task's budget must not leak into the idle reading"
    );
}

#[test]
fn a_second_task_reports_its_own_envelope_and_not_its_predecessors() {
    // §8's "new task" plus §12's wrong-task lookup. The first task is
    // driven to a deliberately distinctive share, so a posture that read
    // the wrong task's rows would be visible as a number rather than
    // having to be inferred.
    let mut harness = Harness::new(admitting_policy());
    let first = harness.preflight("first-session");
    let first_budget = harness.ledger.task_budget(first.task_id).unwrap().unwrap();
    harness
        .ledger
        .reserve(ReserveRequest {
            task_id: first.task_id,
            session_id: "first-session",
            plan_id: None,
            class: ReservationClass::RequiredWork,
            amount: ResourceAmount::from_kind_f64(
                first_budget.resource_kind,
                first_budget.hard_limit.as_f64() / 2.0,
            ),
            idempotency_key: "budget-truth-first-half",
            now: OffsetDateTime::now_utc(),
            ttl_secs: 900,
        })
        .unwrap();
    let first_share = share(harness.status().task_budget);

    let second = harness.preflight("second-session");
    assert_ne!(
        second.task_id, first.task_id,
        "precondition: the second preflight must mint a distinct task"
    );
    let second_share = share(harness.status().task_budget);

    assert!(
        second_share > first_share,
        "the second task must be read against its own envelope: the first \
         was driven to {first_share}, the second reports {second_share}"
    );
    // Not merely "different": the second task's share must be derivable
    // from its own rows alone.
    let headroom = harness
        .ledger
        .available(second.task_id, ReservationClass::RequiredWork)
        .unwrap()
        .unwrap();
    let limit = harness
        .ledger
        .task_budget(second.task_id)
        .unwrap()
        .unwrap()
        .hard_limit
        .as_f64();
    assert_eq!(
        second_share,
        headroom.value / limit,
        "the reported share must be this task's own headroom over this \
         task's own limit"
    );
}

// --- §12 anti-vacuity: denominator, snapshot, restart -----------------

#[test]
fn the_denominator_is_the_tasks_own_hard_limit_not_the_policy_target() {
    // §12's wrong-envelope denominator. The policy's *target* (100_000)
    // and the task's *hard limit* (150_000) are deliberately different
    // numbers in `admitting_policy`, so a share computed against the
    // configured target rather than the task's effective envelope lands
    // on a visibly different value instead of coinciding.
    let mut harness = Harness::new(admitting_policy());
    let preflight = harness.preflight("denominator-session");
    let reported = share(harness.status().task_budget);

    let budget = harness
        .ledger
        .task_budget(preflight.task_id)
        .unwrap()
        .unwrap();
    let headroom = harness
        .ledger
        .available(preflight.task_id, ReservationClass::RequiredWork)
        .unwrap()
        .unwrap();
    let against_limit = headroom.value / budget.hard_limit.as_f64();
    let against_target = headroom.value / 100_000.0;
    assert_ne!(
        against_limit, against_target,
        "precondition: the two denominators must differ or this test proves nothing"
    );
    assert_eq!(
        reported, against_limit,
        "the share must be read against the task's own hard limit"
    );
}

#[test]
fn the_share_is_recomputed_per_request_rather_than_cached() {
    // §12's stale-cached-snapshot mutation. Two readings either side of a
    // real ledger change, with no replan in between: a posture captured
    // once and reused would return the first value twice.
    let mut harness = Harness::new(admitting_policy());
    let preflight = harness.preflight("snapshot-session");
    let first = share(harness.status().task_budget);
    let second = share(harness.status().task_budget);
    assert_eq!(
        first, second,
        "two readings with no change between them must agree"
    );

    let budget = harness
        .ledger
        .task_budget(preflight.task_id)
        .unwrap()
        .unwrap();
    harness
        .ledger
        .reserve(ReserveRequest {
            task_id: preflight.task_id,
            session_id: "snapshot-session",
            plan_id: None,
            class: ReservationClass::RequiredWork,
            amount: ResourceAmount::from_kind_f64(
                budget.resource_kind,
                budget.hard_limit.as_f64() / 5.0,
            ),
            idempotency_key: "budget-truth-snapshot-hold",
            now: OffsetDateTime::now_utc(),
            ttl_secs: 900,
        })
        .unwrap();

    let third = share(harness.status().task_budget);
    assert!(
        third < second,
        "the share must be recomputed while answering each Status, got \
         {second} then {third} after a real commitment"
    );
}

#[test]
fn reopening_the_ledger_does_not_reset_the_share() {
    // §10/§12's restart case, at the layer that owns the fact: the share
    // is derived from durable rows, so a daemon restart must not hand the
    // user back a pristine envelope. Only the ledger is reopened — the
    // in-memory `current_task` is what a real restart also loses, and
    // reconstructing it is a separate concern from whether the durable
    // accounting survived.
    let mut harness = Harness::new(admitting_policy());
    let preflight = harness.preflight("restart-session");
    let budget = harness
        .ledger
        .task_budget(preflight.task_id)
        .unwrap()
        .unwrap();
    harness
        .ledger
        .reserve(ReserveRequest {
            task_id: preflight.task_id,
            session_id: "restart-session",
            plan_id: None,
            class: ReservationClass::RequiredWork,
            amount: ResourceAmount::from_kind_f64(
                budget.resource_kind,
                budget.hard_limit.as_f64() / 4.0,
            ),
            idempotency_key: "budget-truth-restart-hold",
            now: OffsetDateTime::now_utc(),
            ttl_secs: 900,
        })
        .unwrap();
    let before = share(harness.status().task_budget);

    harness.ledger = LedgerStore::open(&harness.config.ledger_path).unwrap();
    let after = share(harness.status().task_budget);

    assert_eq!(
        after, before,
        "a reopened ledger must report the same share, not a reset envelope"
    );
    assert!(
        after < 1.0,
        "and in particular must not report a full envelope after a restart, got {after}"
    );
}
