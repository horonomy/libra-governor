//! `libra-governor statusline provider` — Libra's side of the shared
//! Horonom statusline provider contract (HORO-1564, implemented here for
//! HORO-1569).
//!
//! The contract is a *wire* contract, not a library: this module links
//! nothing from the host, learns no settings-file format, and chooses no
//! glyph. It prints one versioned JSON document describing what is true
//! about Libra right now, and exits 0. The host owns the single
//! `statusLine.command` slot, the user's original statusline, the
//! iconography, the widths and the degradation ladder.
//!
//! Full contract, including the field table and the truthfulness rules the
//! host re-checks on parse:
//! <https://github.com/horonomy/.github/blob/main/governance/product/statusline-provider-contract.md>
//!
//! # Relationship to [`crate::statusline`]
//!
//! [`crate::statusline`] renders the legacy one-line text form that
//! `libra-governor install` wires into `statusLine.command` directly. It
//! stays, byte-for-byte, because it is a *parsed* compatibility surface:
//! the founder's DogFood wrapper matches on its exact phrases
//! (`"idle"`, `"libra: -"`, `"awaiting approval"`, `"replanned <n>x"`).
//! Changing its wording would break a live statusline, so the corrections
//! below are made here and the legacy line is left to be retired by the
//! migration in `docs/statusline.md`.
//!
//! # Two corrections this module makes to the legacy rendering
//!
//! Both were found by reading the daemon rather than the wrapper, and both
//! are cases where the old text claims more than the state supports.
//!
//! 1. **`preflight: high` was an unqualified word.** A bare `high` beside a
//!    task reads as a risk or priority rating. It is neither: it is the
//!    estimator's confidence in the remaining-work estimate. The contract
//!    has a field pair for exactly this (`confidence` +
//!    `confidence_of: "preflight_estimate"`), so the host renders
//!    `preflight confidence high` and the ambiguity is structurally gone
//!    rather than fixed by a longer abbreviation.
//!
//! 2. **`escalated — awaiting approval` overstates what happened.**
//!    [`ReplanState::EscalatedAwaitingApproval`] means the task's
//!    *automatic*-replan budget is exhausted, so the next material
//!    deviation will not be silently replanned again. Nothing is blocked
//!    and nothing is waiting on an answer — hooks are advisory-only
//!    (ADR-0001), and `server.rs`'s `EscalateApprovalNeeded` arm logs, sets
//!    the state, and returns `Ok(())`; work proceeds. "Awaiting approval"
//!    invites the reader to go and approve something that does not exist.
//!    So this provider says `Replans now need approval`: future tense, about
//!    a rule rather than a queue, naming the decision a user could make
//!    without implying one is pending.
//!
//! # The Clear projection (HORO-1634)
//!
//! Every document declares `clear_authority: "provider"` and a `clear_role`
//! on every segment, so the host's one-line summary of Libra is chosen from
//! Libra's own semantics rather than from host-side severity arithmetic. The
//! host validates the claim and refuses a document that does not hold up
//! (`statusline_contract.ProviderStatus._validate_clear_authority`), which
//! is why the roles are assigned structurally below and asserted over every
//! document this module can emit rather than spot-checked.
//!
//! The declared map, and what each role buys:
//!
//! | segment | role | why |
//! |---|---|---|
//! | `task` (active) | supporting | an opaque id is the reader's handle on the work, not their reason to glance at the line |
//! | `task` (idle) | posture | "nothing is being governed" *is* the whole state |
//! | `task` (escalated) | exception | a decision a human could make |
//! | `estimate` | posture | for a scheduling product the remaining span is the posture |
//! | `budget` | vital, or exception when exhausted | the one reading that qualifies a schedule — and a limit that has been reached is not a posture |
//! | `profile` | vital | a policy edit that has not taken effect changes how everything above reads |
//! | `availability` | exception | there is no reading, which is the only thing to say |
//!
//! Two consequences worth naming because they are not obvious:
//!
//! - **The escalation is not its own segment.** The host accepts at most
//!   four segments per provider and refuses the whole document on the fifth
//!   (`MAX_SEGMENTS_PER_PROVIDER`), so `task` + `estimate` + `budget` +
//!   `profile` is the entire allowance. The replan state rides on the task
//!   segment, which is where two of its three variants already rode — "this
//!   task, replanned twice" and "this task, whose replan budget is spent"
//!   are one fact about one thing. The cost is the eight-character id in
//!   the escalated state only, where the label slot is better spent on the
//!   actionable half; `explain` prints the task and plan ids
//!   unconditionally, and Clear never showed the id in any state.
//! - **Severity is not inflated to win the ladder.** The escalation and an
//!   exhausted budget are both `warn`, never `critical`: `critical` means
//!   work has stopped, and Libra's hooks are advisory, so nothing has.
//!   Where both are true at once the host's documented tie-break — earliest
//!   declared `order_hint` — leads with the approval, which is the half a
//!   user can act on rather than a limit they have already reached.
//!
//! # Why the profile costs a second round trip
//!
//! The provider answers a `Status` request and then, separately, a
//! `Doctor` one, because the running policy preset and whether
//! `config.json` still agrees with it live on [`DoctorResult`] and nowhere
//! else. The alternative considered and rejected was widening
//! [`StatusResult`] to carry them: this repository's convention is to bump
//! `PROTOCOL_VERSION` for any response-shape change (see its docs — nine
//! bumps, one per shape change), and a bump forces every long-lived daemon
//! to be restarted before it will answer again. A second local round trip
//! inside the same deadline is the cheaper price, and it keeps one
//! implementation of "does the running config match disk" rather than two.
//!
//! HORO-1634's budget share went the other way — onto `StatusResult`, at
//! the cost of a bump — and the two decisions are consistent rather than
//! contradictory. The profile is a *diagnostic* the daemon already computes
//! for `Doctor` and that a statusline can lose without lying; the budget is
//! part of the reading itself, so a third round trip would have had to
//! succeed inside the same 200 ms for the line to be complete. Paying a
//! restart once is cheaper than making the hot path depend on three
//! connections.
//!
//! The `Doctor` half is failure-isolated from the `Status` half: if it is
//! slow, unreachable or unparseable, the profile segment is simply absent
//! and the rest of the reading still renders. A diagnostic that cannot be
//! obtained must not cost the user the state that could.
//!
//! # What this module never does
//!
//! It never spawns the daemon (a statusline refreshes on a timer; spawning
//! from it is a race factory — see [`crate::client::connect_only`]), never
//! makes an LLM or network call of its own, never evaluates a policy,
//! never mutates a decision, and offers no field through which a user
//! could approve anything. Rendering is read-only; the statusline is not an
//! approval interface.
//!
//! It also never prints task content, prompts, tool output, cost history,
//! a credential, or a path. Every string it emits is either a fixed literal
//! in this file or a value drawn from a closed set — see
//! [`preset_display`], which is the one place a value reaches the payload
//! from user-writable configuration.

use std::time::{Duration, Instant};

use libra_governor_protocol::{
    BudgetPosture, BudgetSnapshot, Confidence, DoctorResult, ReplanState, Request, ResourceKind,
    Response, StatusResult, TaskSummary,
};
use serde_json::{json, Value};
use time::OffsetDateTime;

use crate::client::{self, ClientError};
use crate::presentation::{self, BudgetDisplay};

/// Version of the cross-product provider contract this module speaks. A
/// host that does not recognise it refuses the whole document rather than
/// guessing at a meaning, which is why it is stated on every answer
/// including the failures.
pub const CONTRACT_VERSION: u32 = 1;

/// Provider id, as registered with the host.
///
/// `libra`, not `libra-governor`: the host derives a display name from this
/// id (`fornax` becomes `Fornax`), and the product is Libra — `libra-governor`
/// is the name of its binary, which is not what a reader of a statusline is
/// being told about.
pub const PROVIDER_ID: &str = "libra";

/// Breadth of the state being reported.
///
/// `host`, and this is a correctness claim rather than a default. The
/// daemon holds exactly one `current_task` for the whole machine — a
/// single `Option<TaskSummary>` on its config, replaced by whichever
/// session most recently completed a preflight (see
/// `server.rs::handle_preflight`). The task shown may therefore belong to a
/// different session than the one being rendered, so declaring `session`
/// would make the host label another session's work as this one's.
pub const SCOPE: &str = "host";

/// Where Libra sits relative to other products on the shared line. Lower
/// sorts earlier; Fornax is 300 and Circinus 400, and the range is
/// deliberately sparse so products can be reordered without renegotiating.
pub const ORDER_HINT: u32 = 500;

/// Who decides which of Libra's facts earns its one Clear-mode phrase
/// (HORO-1634).
///
/// `provider`, on every document including the failures. The host's fallback
/// ladder infers a role from state and position, and for this provider those
/// two signals cannot separate the facts that matter: the task id and the
/// remaining-work span are both `neutral` and adjacent, while the escalation
/// and a policy-drift warning are both `warn` and say entirely different
/// things. Declaring the projection is what makes Libra's Clear line Libra's
/// editorial judgement rather than an artefact of severity arithmetic — and
/// the host switches its ladder off rather than merging with it, because a
/// declaration that can be overridden is not a declaration.
///
/// The claim is validated, not trusted: the host requires a `clear_role` on
/// every segment, at most one posture, and at least one posture or
/// exception, and refuses the whole document otherwise. A refused document
/// renders as nothing at all, so the roles below are assigned structurally
/// rather than case by case, and the tests walk every document this module
/// can produce.
pub const CLEAR_AUTHORITY: &str = "provider";

/// Total time the hot-path probe may spend, across *both* round trips.
///
/// Under the host's own default per-provider budget (250 ms) so that a slow
/// daemon yields a truthful "did not answer in time" document rather than
/// being killed mid-write. A killed provider contributes nothing, and
/// nothing on a statusline reads as all-clear.
pub const HOT_PATH_BUDGET: Duration = Duration::from_millis(200);

/// How long `explain` may wait. Larger on purpose: the user asked a
/// question and is waiting for the answer, so a slow daemon is worth
/// waiting out rather than reporting as a timeout.
pub const EXPLAIN_BUDGET: Duration = Duration::from_secs(3);

/// Longest span the host's contract accepts, mirrored here so an absurd
/// estimate clamps instead of making the host reject the whole document. A
/// refused provider renders as an unknown with no reading at all, which is
/// a worse answer than "implausibly long".
const MAX_DURATION_SECONDS: u64 = 10 * 365 * 24 * 60 * 60;
/// Largest count the host's contract accepts. `ReplanState::Replanned`
/// carries a `u32`, which can exceed it.
const MAX_COUNT: u32 = 1_000_000_000;
/// Longest label the host's contract accepts. Mirrored here because the
/// budget label is the one this module assembles from numbers whose width
/// it does not control, so it has to be able to ask whether a candidate
/// fits — see [`budget_label`].
const MAX_LABEL_CHARS: usize = 48;

/// Why the provider has no live reading to report.
///
/// These are six distinct facts and the contract refuses to collapse them:
/// a stopped daemon is not a daemon reporting nothing, and a probe that
/// failed is not silence. Each maps to its own availability and its own
/// bounded reason code, so `doctor` can tell the user what to do about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoReading {
    /// The state directory — and so the socket path — could not be
    /// resolved at all. Distinct from an unreachable daemon: there is no
    /// address to be unreachable at, and starting a daemon would not help.
    SocketPathUnresolved,
    /// Nothing is listening on the daemon socket.
    DaemonUnreachable,
    /// Something is listening, but did not answer inside the render
    /// budget. A separate fact from a stopped daemon: reporting a slow
    /// daemon as "not running" sends the user to start one that is already
    /// up.
    DaemonTooSlow,
    /// The daemon answered on a different protocol version. A real,
    /// actionable condition — this binary was upgraded and the long-lived
    /// daemon has not been restarted — and emphatically not "no task".
    DaemonProtocolMismatch,
    /// The daemon answered, and its answer was an error.
    DaemonReportedError,
    /// The daemon answered with something this client cannot parse as a
    /// status response.
    ResponseNotUnderstood,
}

impl NoReading {
    /// The contract availability for this outcome.
    ///
    /// Only [`NoReading::DaemonUnreachable`] is `unavailable`, because that
    /// is the one case where asserting "the thing is not there" is true.
    /// A slow daemon leaves the state genuinely unobserved, which is
    /// `unknown` — and the contract is explicit that `unknown` is
    /// interchangeable with neither `available` nor `unavailable`.
    fn availability(self) -> &'static str {
        match self {
            NoReading::DaemonUnreachable => "unavailable",
            NoReading::DaemonTooSlow => "unknown",
            NoReading::SocketPathUnresolved
            | NoReading::DaemonProtocolMismatch
            | NoReading::DaemonReportedError
            | NoReading::ResponseNotUnderstood => "error",
        }
    }

    /// Machine-readable reason, for `doctor` and for tests.
    fn reason_code(self) -> &'static str {
        match self {
            NoReading::SocketPathUnresolved => "socket_path_unresolved",
            NoReading::DaemonUnreachable => "daemon_unreachable",
            NoReading::DaemonTooSlow => "daemon_too_slow",
            NoReading::DaemonProtocolMismatch => "daemon_protocol_mismatch",
            NoReading::DaemonReportedError => "daemon_reported_error",
            NoReading::ResponseNotUnderstood => "response_not_understood",
        }
    }

    /// Short human label. Fixed prose, never an error message: a
    /// [`ClientError`] in this codebase routinely carries the socket path,
    /// and this value is rendered straight into the user's terminal.
    ///
    /// Deliberately the *state* rather than the cause, with the cause left
    /// to [`NoReading::reason_code`] and no `reason_label` at all. The host
    /// renders a reason clause beside the label, so a label that already
    /// names the cause is paid for twice in columns the host then has to
    /// degrade away — and letting the host prettify the code is the shared
    /// presentation the contract intends, rather than Libra's own phrasing
    /// of it.
    fn label(self) -> &'static str {
        match self {
            NoReading::DaemonUnreachable => "Not running",
            NoReading::DaemonTooSlow => "No answer in time",
            NoReading::SocketPathUnresolved | NoReading::DaemonProtocolMismatch => {
                "State not observed"
            }
            NoReading::DaemonReportedError | NoReading::ResponseNotUnderstood => {
                "Answer unreadable"
            }
        }
    }
}

impl From<&ClientError> for NoReading {
    /// Classify a client failure into the fact it actually establishes.
    ///
    /// The timeout case is the one worth care. `connect_only_with_budget`
    /// sets read and write timeouts, and a socket timeout surfaces as
    /// `WouldBlock` or `TimedOut` depending on the platform and on which
    /// side of the transfer expired, so both are checked. Anything else
    /// from the I/O layer is a genuinely unreadable exchange rather than a
    /// slow one.
    fn from(error: &ClientError) -> Self {
        match error {
            ClientError::DaemonUnavailable(..) => NoReading::DaemonUnreachable,
            ClientError::Io(e) => match e.kind() {
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {
                    NoReading::DaemonTooSlow
                }
                _ => NoReading::ResponseNotUnderstood,
            },
            ClientError::Wire(_) => NoReading::ResponseNotUnderstood,
            ClientError::ProtocolMismatch(..) => NoReading::DaemonProtocolMismatch,
            ClientError::DaemonError(_) => NoReading::DaemonReportedError,
        }
    }
}

/// The segment state that goes with a non-live availability.
///
/// Mirrors the host contract's own mapping so the two cannot drift:
/// `error` becomes `warn` because a failed probe may need the user to act,
/// while `unavailable` and `unknown` state a fact without claiming health.
/// None of them may be `ok`, and the host enforces that independently.
fn state_for(availability: &str) -> &'static str {
    match availability {
        "unavailable" => "neutral",
        "error" => "warn",
        _ => "unknown",
    }
}

/// Build the provider document for an outcome with no live reading.
///
/// Note what is *not* here: no segment claiming `ok`, no count, no
/// confidence, no duration, and no empty segment list. An empty answer
/// renders as silence and silence reads as "all clear", which is the
/// failure mode this shape exists to prevent.
///
/// Declares [`CLEAR_AUTHORITY`] like every other document, and the one
/// segment is an exception. Not for emphasis — the host already routes a
/// non-live provider down its own single-reading path — but so that the
/// claim "this payload declares its own projection in full" is true of
/// *every* document Libra can emit. An envelope that declared authority in
/// the success case and not in the failure case would be a declaration the
/// host could only partly rely on, and the failure case is where an
/// inferred role would do the most damage.
pub fn no_reading(kind: NoReading) -> Value {
    let availability = kind.availability();
    json!({
        "contract_version": CONTRACT_VERSION,
        "provider": PROVIDER_ID,
        "provider_version": env!("CARGO_PKG_VERSION"),
        "scope": SCOPE,
        "availability": availability,
        "order_hint": ORDER_HINT,
        "clear_authority": CLEAR_AUTHORITY,
        "segments": [{
            "key": "availability",
            "state": state_for(availability),
            "label": kind.label(),
            "reason_code": kind.reason_code(),
            "explain_key": "libra.availability",
            "clear_role": "exception",
        }],
    })
}

/// The task currently being governed, and how many times it has been
/// automatically replanned.
///
/// `neutral`, never `ok`. A task being governed is not a verdict that
/// anything is well; `ok` on this line would be Libra asserting the work is
/// going fine, which it has not measured. The no-task case is `neutral`
/// for the same reason — "nothing is being governed" is a state, not a
/// pass.
///
/// All three [`ReplanState`] variants ride here rather than in a segment of
/// their own, because "this task, replanned twice" and "this task, whose
/// replan budget is spent" are one fact about one thing. The count is
/// omitted when zero: `0 replans` spends columns to say nothing, and the
/// legacy line's `stable` said the same thing at more length.
///
/// The escalated state is the one case where the label is not the task.
/// `Replans now need approval` replaces the id because the host accepts
/// four segments per provider and refuses the fifth, so this segment and
/// the escalation cannot both exist beside the estimate, the budget and a
/// policy-drift warning — and when they compete for one label slot the
/// actionable half wins. Clear never showed the id in any state, and
/// `explain` prints the task and plan ids unconditionally.
///
/// The plan id the legacy line also carried is deliberately dropped. Two
/// opaque eight-character identifiers on a line shared with the user's own
/// statusline and every other product is what progressive disclosure is
/// for; the task id stays because it is the one handle a reader has on the
/// work, and both appear in full on the `explain` surface.
///
/// Clear roles, in the three shapes this returns: idle is the `posture`,
/// because "nothing is being governed" is the entire state and there is no
/// estimate segment to claim the part; escalated is an `exception`, a
/// decision a human could make; and an ordinary active task is
/// `supporting`, because an opaque id is a reader's handle on the work and
/// not their reason to glance at the line.
fn task_segment(task: Option<&TaskSummary>) -> Value {
    let Some(task) = task else {
        return json!({
            "key": "task",
            "state": "neutral",
            "label": "No task being governed",
            "explain_key": "libra.task",
            "order_hint": 10,
            "clear_role": "posture",
        });
    };
    if task.replan_state == ReplanState::EscalatedAwaitingApproval {
        // `warn`, not `critical`, and future tense. Nothing is blocked:
        // `server.rs`'s `EscalateApprovalNeeded` arm logs, records the state
        // and returns `Ok(())`, and hooks are advisory-only, so the task
        // keeps running. What has changed is that the plan on screen will no
        // longer be silently corrected — worth acting on, not an emergency,
        // and not a request anyone can answer from a statusline.
        return json!({
            "key": "task",
            "state": "warn",
            "label": "Replans now need approval",
            "reason_code": "next_replan_needs_human_approval",
            "explain_key": "libra.escalation",
            "order_hint": 10,
            "clear_role": "exception",
        });
    }
    let mut segment = json!({
        "key": "task",
        "state": "neutral",
        "label": format!("Task {}", crate::statusline::short_task_id(&task.task_id.to_string())),
        "explain_key": "libra.task",
        "order_hint": 10,
        "clear_role": "supporting",
    });
    if let ReplanState::Replanned { count } = task.replan_state {
        if count > 0 {
            segment["count"] = json!(count.min(MAX_COUNT));
            segment["count_label"] = json!("replans");
        }
    }
    segment
}

/// The remaining-work estimate: a span, and the confidence in that span.
///
/// Both facts live on one segment on purpose. The confidence is a
/// confidence *in this estimate*, so sitting it beside the number is what
/// makes `preflight confidence medium` unambiguous — a separate segment
/// would put the qualifier a degradation step away from the thing it
/// qualifies, which is how `pf:high` came to read as a risk rating.
///
/// The span is sent as seconds plus a noun, never as a formatted string.
/// The host owns formatting (`statusline_render.format_duration`); a
/// product that rendered its own `5d4h` would be one more place for two
/// products to disagree about what a day is.
///
/// With no `duration_p90_secs` there is no estimate, so no confidence is
/// emitted either: a confidence in a number that is absent reads as a
/// judgement about the task. The reason is taken from `Estimate::cold_start`,
/// which is authoritative and structural — the type's own docs promise a
/// cold start is *never* inferred from `confidence` alone, and the
/// estimator's free-text `reason` is never rendered.
///
/// The two labels differ, and that is the point rather than a cosmetic
/// choice. Clear keeps the state marker, the label and the duration and
/// drops the reason, so a no-estimate segment labelled `Remaining work`
/// would render in Clear as exactly `Remaining work` — a heading with
/// nothing under it, indistinguishable at a glance from an estimate the
/// reader simply failed to read. `Remaining work not estimated` says the
/// missing half in the one field Clear is guaranteed to show, and the
/// reason still distinguishes *why* in Detail.
///
/// `posture`, in both shapes. For a product whose whole subject is whether
/// work will finish, the remaining span is the posture — and an estimate
/// that does not exist is a posture too, which is why the role does not
/// move when the number does.
fn estimate_segment(task: &TaskSummary) -> Value {
    let estimate = &task.remaining_estimate;
    let Some(p90) = estimate.duration_p90_secs else {
        return json!({
            "key": "estimate",
            "state": "unknown",
            "label": "Remaining work not estimated",
            "reason_code": if estimate.cold_start {
                "no_local_history_yet"
            } else {
                "estimate_has_no_duration_bound"
            },
            "explain_key": "libra.estimate",
            "order_hint": 20,
            "clear_role": "posture",
        });
    };
    json!({
        "key": "estimate",
        "state": "neutral",
        "label": "Remaining work",
        "duration_seconds": p90.min(MAX_DURATION_SECONDS),
        "duration_label": "P90",
        "confidence": confidence_token(task.confidence),
        "confidence_of": "preflight_estimate",
        "explain_key": "libra.estimate",
        "order_hint": 20,
        "clear_role": "posture",
    })
}

/// The contract's spelling of a [`Confidence`].
fn confidence_token(confidence: Confidence) -> &'static str {
    match confidence {
        Confidence::Low => "low",
        Confidence::Medium => "medium",
        Confidence::High => "high",
    }
}

/// The share of the task's resource envelope still available, as a whole
/// percentage in `1..=100`.
///
/// Truncated rather than rounded, and floored at 1. Both bounds exist to
/// keep the three ways a budget can be gone distinguishable from a budget
/// that is merely nearly gone: rounding `0.004` to `0%` would print "0%
/// budget left" for a task the ledger will still serve, and rounding
/// `0.999` up to `100%` would claim nothing had been spent when something
/// had. Truncation never over-reports what is left, which is the direction
/// a budget figure should err in.
///
/// `None` for a share that is not a finite number in `0.0..=1.0`. The value
/// arrives over the wire from the daemon, so this function cannot assume it
/// is well-formed, and a percentage computed from a NaN is worse than no
/// percentage — the caller reports it as an unreadable budget rather than
/// printing arithmetic.
fn percent_left(fraction_left: f64) -> Option<u32> {
    if !fraction_left.is_finite() || fraction_left <= 0.0 || fraction_left > 1.0 {
        return None;
    }
    Some(((fraction_left * 100.0).floor() as u32).max(1))
}

/// The share to display, preferring the one the transported *amounts*
/// imply over the one the posture carries (HORO-1709).
///
/// Both come from the same `BudgetSnapshot` in the same ledger
/// transaction, so they agree in the daemon by construction. They can
/// still disagree by one ULP *after the wire*: `serde_json`'s default
/// float parser is approximate — exact `f64` round-tripping lives behind
/// its `float_roundtrip` feature, which is not enabled — while the
/// integral amounts round-trip exactly. A share sitting on an integer
/// boundary can therefore come back one ULP low and render a whole point
/// lower than the amounts beside it, because [`percent_left`] floors.
/// Recomputing from the amounts is what makes "percentage and amount
/// cannot contradict each other" survive the transport as well as the
/// query.
///
/// Falls back to the posture's own share when no snapshot came with it —
/// a v11 daemon always sends both for a `Remaining` posture, so this is
/// the path for a future peer that sends only the classification.
fn share_to_display(fraction_left: f64, amounts: Option<&BudgetSnapshot>) -> f64 {
    amounts
        .and_then(BudgetSnapshot::fraction_left)
        .unwrap_or(fraction_left)
}

/// The noun the host may print beside a budget count, per resource kind
/// (HORO-1709).
///
/// `None` suppresses amounts entirely, which is the honest answer for a
/// quota envelope: its amounts *are* percentages, so "38 of 100 quota
/// percent" restates the share as though it were a second fact.
///
/// USD is reported in cents and says so. The host's label allowlist has
/// no `$`, and the contract's `count`/`total` are integers, so `$12.40`
/// is not expressible — and inventing a formatter here would put a
/// currency renderer in a product, which is exactly what the contract
/// keeps in the host. Cents with the unit named is unambiguous today;
/// a currency-aware host span is HORO-1719's business.
///
/// Nothing here guesses: the kind is the envelope's own, fixed at
/// admission from the policy's resource target, so a unit is never
/// inferred from the fact that a number exists.
fn unit_noun(kind: ResourceKind) -> Option<&'static str> {
    match kind {
        ResourceKind::Tokens => Some("tokens"),
        ResourceKind::Usd => Some("USD cents"),
        ResourceKind::QuotaPercent => None,
    }
}

/// One amount as a contract `count`, or `None` when it cannot be one.
///
/// Floored rather than rounded, for the same reason [`percent_left`]
/// truncates: a budget figure should never over-report what is left.
/// `None` for a non-finite value, a negative one (the contract has no
/// signed count, and silently clamping an overrun to `0` would turn a
/// deficit into a boundary), or one above the host's ceiling — an
/// envelope larger than a billion units renders as a share with no
/// amounts rather than as a clamped number that is simply wrong.
fn count_value(amount: f64) -> Option<u32> {
    if !amount.is_finite() || amount < 0.0 || amount > f64::from(MAX_COUNT) {
        return None;
    }
    Some(amount.floor() as u32)
}

/// A run of decimal digits with `,` every three, deterministically and
/// without consulting a locale — the host formats its own `count`/`total`
/// span, but `used` and `reserved` have no contract field yet, so those
/// two reach the user through the label and need a grouping of their own.
///
/// Takes the digits as text rather than a number so that the one rule
/// lives in one place regardless of how wide the figure is: a four-digit
/// count and a ledger `f64` past four billion are grouped identically,
/// and an unfamiliar magnitude never silently loses its separators at the
/// exact width where they matter most.
fn group_digits(digits: &str) -> String {
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A ledger figure as it may appear inside a label, or `None` when it has
/// no honest rendering there (HORO-1709).
///
/// Deliberately not routed through [`count_value`]: that function enforces
/// the *host's* ceiling on its `count` field, which is a rule about a
/// number the host will format and has nothing to say about a word in a
/// sentence. The only ceiling that applies here is the label's own, and
/// [`budget_label`] checks it on the assembled phrase.
///
/// A negative figure is declined rather than rendered. Settled spend and
/// held capacity cannot be negative in a well-formed snapshot, so one
/// arriving here is a peer sending something this build does not
/// understand, and the compact phrase is the wrong place to work out what
/// it meant.
fn span_figure(amount: f64) -> Option<String> {
    (amount.is_finite() && amount >= 0.0).then(|| grouped_f64(amount))
}

/// The compact budget phrase, at the most detail the preference allows
/// that still fits the host's label ceiling (HORO-1709).
///
/// The percentage is always present and always first, because it is the
/// one figure that fits in every envelope's worth of digits and the one
/// the pre-ticket line carried. `used` and `reserved` are appended only
/// when asked for, and only when they fit — the contract has no field
/// for either, so the label is the only route, and a label over
/// [`MAX_LABEL_CHARS`] is refused by the host at the cost of the *whole
/// document*. Dropping back to a shorter phrase is therefore the only
/// acceptable failure mode: the preference is a ceiling on detail, never
/// a promise of it, and the figures that did not fit are still in the
/// structured fields and in `explain`.
///
/// "held" rather than "reserved" for the reserved figure: it is four
/// characters shorter in a 48-character budget, and "reserved" beside
/// "used" invites reading the two as a sum, which the ticket forbids.
fn budget_label(percent: u32, amounts: Option<&BudgetSnapshot>, display: BudgetDisplay) -> String {
    let base = format!("{percent}% budget left");
    let (Some(amounts), true) = (
        amounts,
        display == BudgetDisplay::UsedRemainingAndTotal || display == BudgetDisplay::Full,
    ) else {
        return base;
    };
    // A quota envelope's amounts *are* percentages, so "38% left, 42 used"
    // would put two percentages side by side with only one of them marked
    // as one. The share is the honest rendering of that envelope and it is
    // already here.
    if unit_noun(amounts.kind()).is_none() || !amounts.is_observed() {
        return base;
    }
    let Some(used) = span_figure(amounts.used().as_f64()) else {
        return base;
    };
    let mut candidates = Vec::new();
    if display == BudgetDisplay::Full {
        if let Some(held) = span_figure(amounts.reserved().as_f64()) {
            candidates.push(format!("{percent}% left, {used} used, {held} held"));
        }
    }
    candidates.push(format!("{percent}% left, {used} used"));
    // Bytes, not chars, because that is what the host measures and these
    // candidates are pure ASCII — digits, commas, spaces and two English
    // words — so the two counts are equal and the byte one cannot be the
    // looser of them.
    candidates
        .into_iter()
        .find(|candidate| candidate.len() <= MAX_LABEL_CHARS)
        .unwrap_or(base)
}

/// One structured ledger figure, written only if it is a number at all.
///
/// A non-finite amount is omitted rather than serialized: `serde_json`
/// cannot represent `NaN` or an infinity and would turn it into `null`,
/// which a reader would have to tell apart from an absent field anyway —
/// so the absent field is the clearer of the two identical outcomes.
fn amount_field(segment: &mut Value, key: &str, amount: f64) {
    if amount.is_finite() {
        segment[key] = json!(amount);
    }
}

/// The structured economic fields the shared renderer will read
/// (HORO-1709), attached to a segment whatever the wording preference
/// says.
///
/// Emitted unconditionally when the ledger has authoritative figures,
/// because the preference governs the *compact phrase* and nothing else:
/// Clear stays as terse as the user asked for while Detail and
/// HORO-1719's palette still have the whole envelope to work from. The
/// contract ignores fields it does not know, so these render nothing
/// today and cost nothing — HORO-1719 is where they acquire a
/// presentation.
///
/// `budget_scope` is on every one of them. Four envelopes can produce a
/// number here — a task's, a configured default, a host cap and an
/// organisation's — and the ticket's standing requirement is that they
/// are never conflated. A figure that cannot say which one it is a
/// figure of is not economic truth.
fn budget_amount_fields(segment: &mut Value, amounts: &BudgetSnapshot) {
    segment["budget_scope"] = json!("active_task");
    if let Some(unit) = unit_noun(amounts.kind()) {
        segment["budget_unit"] = json!(unit);
    }
    // The ceiling is authoritative even when nothing has drawn against
    // it, so it is reported either way. The *consumption* figures are
    // not: an unobserved envelope has no measured spend, and emitting
    // `0` for it would be the "100% budget left" claim in a new field.
    //
    // These are the ledger's own figures, not the host's `count` field, so
    // [`count_value`]'s ceiling does not apply to them and they are
    // reported signed and at full width. A figure a renderer declines to
    // show is a presentation decision; a figure withheld here would be a
    // missing fact.
    amount_field(segment, "budget_total", amounts.total().as_f64());
    segment["budget_observed"] = json!(amounts.is_observed());
    if !amounts.is_observed() {
        return;
    }
    amount_field(segment, "budget_used", amounts.used().as_f64());
    amount_field(segment, "budget_reserved", amounts.reserved().as_f64());
    // Signed on purpose, and the field here most likely to be negative: an
    // overrun is a real authoritative state and the contract's unsigned
    // `count` cannot hold it, so this is where it survives.
    amount_field(segment, "budget_remaining", amounts.remaining().value);
}

/// The contract-native amount span: `count`, `total` and the noun that
/// says what they count (HORO-1709).
///
/// These three the host *does* render today, which is why they are the
/// one part of this gated on the preference. The default is
/// percentage-only, so a fresh install's Clear line is unchanged by this
/// ticket; a user who asks for amounts gets them in the host's own
/// formatting rather than in a span assembled here.
///
/// Nothing is emitted for an unobserved envelope. `count` would have to
/// be the whole ceiling, and "150,000 of 150,000 tokens" is the
/// measured-full claim HORO-1708 removed, restated as a pair of numbers.
fn budget_count_fields(segment: &mut Value, amounts: &BudgetSnapshot, display: BudgetDisplay) {
    if display == BudgetDisplay::Percent || !amounts.is_observed() {
        return;
    }
    let Some(unit) = unit_noun(amounts.kind()) else {
        return;
    };
    let Some(remaining) = count_value(amounts.remaining().value) else {
        return;
    };
    segment["count"] = json!(remaining);
    segment["count_label"] = json!(unit);
    if display == BudgetDisplay::Remaining {
        return;
    }
    if let Some(total) = count_value(amounts.total().as_f64()) {
        segment["total"] = json!(total);
    }
}

/// What is left of the task's resource envelope (HORO-1634), optionally
/// with the amounts behind the share (HORO-1709).
///
/// The share is always present and the axis is always a literal in the
/// format string rather than something assembled from a variable. The host
/// refuses a label containing a percentage with no word saying what it
/// measures — `62%` alone reads as used *and* as left, which are opposite
/// answers — and a refused label costs the whole document, not the one
/// segment. Writing the axis as part of the template is what makes the rule
/// satisfied by construction: there is no code path that formats the number
/// without it.
///
/// HORO-1709 adds the figures the share is a share *of*. Three routes, and
/// which one a figure takes is not a style choice:
///
/// - `count`/`total`/`count_label` for remaining and total, because the
///   contract has fields for them and is explicit that the *host* formats
///   the numbers. Gated on the user's preference, so a fresh install's line
///   is byte-identical to the pre-ticket one.
/// - the label for used and reserved, because the contract has no field for
///   either and the label is the only surface left. Gated on the preference
///   too, and dropped when it would not fit.
/// - `budget_*` structured fields for everything, unconditionally, because
///   the preference governs the compact phrase and not what Detail,
///   `explain` and HORO-1719's palette are allowed to know.
///
/// The share rendered is recomputed from the amounts when they are present
/// — see [`share_to_display`] — so the percentage and the numbers beside it
/// cannot contradict each other even by one ULP of transport error.
///
/// `vital`, except when the envelope is spent. A budget share is the one
/// reading that qualifies a schedule — `Replans now need approval · 38%
/// budget left` says what the decision costs, which a bare approval cannot
/// — and the host only pairs a declared vital with an exception, never an
/// inferred one. Exhaustion is the exception: a limit that has been reached
/// is not a posture about how the work is going, it is the reason the next
/// reservation will be refused.
///
/// The four non-numeric outcomes stay distinct instead of collapsing to
/// one "no budget" phrase. A task that was never admitted to an envelope, an
/// envelope nothing has drawn against, an envelope that is spent, and a
/// ledger that would not read are four different things to be told, and
/// only the spent one is about the work.
///
/// `Uncommitted` is worded as a statement about *usage* rather than about
/// the envelope (HORO-1708), and that is the whole point of the label. The
/// state it describes is "nothing has been attributed to this task", whose
/// natural phrasings — untouched, unspent, full — all read as "none of it
/// is gone", which is the "100% budget left" claim this variant exists to
/// stop making. Naming the unknown thing instead is the only phrasing that
/// cannot be mistaken for the measurement it lacks.
fn budget_segment(
    posture: BudgetPosture,
    amounts: Option<&BudgetSnapshot>,
    display: BudgetDisplay,
) -> Value {
    let (state, label, reason_code, clear_role) = match posture {
        BudgetPosture::Remaining { fraction_left } => {
            match percent_left(share_to_display(fraction_left, amounts)) {
                Some(percent) => (
                    "neutral",
                    budget_label(percent, amounts, display),
                    None,
                    "vital",
                ),
                // "not usable" rather than "not a number", because
                // `percent_left` rejects three different malformations and
                // only one of them is a NaN: a share of exactly zero and a
                // share above one are both perfectly good numbers that
                // cannot be a share of a live envelope. A label naming the
                // narrowest cause would be false in two of the three cases,
                // which is the one thing a provider whose whole job is
                // reporting state may not be.
                None => (
                    "warn",
                    "Budget share not usable".to_string(),
                    Some("budget_share_not_usable"),
                    "vital",
                ),
            }
        }
        // `warn`, matching the escalation and for the same reason: hooks are
        // advisory-only, so an exhausted envelope means the ledger will
        // refuse the next reservation, not that work has stopped. `critical`
        // would claim the latter.
        BudgetPosture::Exhausted => (
            "warn",
            "Budget exhausted".to_string(),
            Some("budget_hard_limit_reached"),
            "exception",
        ),
        // `unknown`, not `neutral`: this is the absence of a reading, and
        // `neutral` is how a reading that happens to be comfortable is
        // reported. Sharing the state with `NotEstablished` is right —
        // neither surface has a share to show — and the `reason_code` is
        // what separates "no envelope" from "no draw against one".
        BudgetPosture::Uncommitted => (
            "unknown",
            "Budget usage unknown".to_string(),
            Some("no_commitment_against_envelope"),
            "vital",
        ),
        BudgetPosture::NotEstablished => (
            "unknown",
            "Budget not established".to_string(),
            Some("task_admitted_without_a_budget"),
            "vital",
        ),
        BudgetPosture::Unreadable => (
            "warn",
            "Budget unreadable".to_string(),
            Some("budget_ledger_unreadable"),
            "vital",
        ),
    };
    let mut segment = json!({
        "key": "budget",
        "state": state,
        "label": label,
        "explain_key": "libra.budget",
        "order_hint": 25,
        "clear_role": clear_role,
    });
    if let Some(reason_code) = reason_code {
        segment["reason_code"] = json!(reason_code);
    }
    if let Some(amounts) = amounts {
        budget_amount_fields(&mut segment, amounts);
        budget_count_fields(&mut segment, amounts, display);
    }
    segment
}

/// The four presets `Policy` exposes: the config token, spelled exactly as
/// `crates/domain/src/policy.rs` spells it, paired with the prose a
/// statusline may carry.
///
/// Three of the four tokens contain an underscore, and the host's label
/// allowlist is ASCII alphanumerics plus `. , ' - — ( ) % + ? ! ≤ ≥` — no
/// `_`, deliberately, because that is where `key=value` and shell
/// expansions live. A label of `Running policy strict_budget` is therefore
/// not merely ugly: the host raises on it and refuses the whole document,
/// so Libra would render as *nothing* at precisely the moment it had drift
/// to report. Hence a table rather than the raw name, and the prose reads
/// better beside a user's own statusline anyway.
const KNOWN_PRESETS: [(&str, &str); 4] = [
    ("balanced", "balanced"),
    ("deadline_first", "deadline first"),
    ("cost_first", "cost first"),
    ("strict_budget", "strict budget"),
];

/// The config token for a preset, if it is one this build recognises.
///
/// `Policy::name` is a `String` whose docs permit "a caller-chosen name for
/// a custom policy", so it is not a bounded value and must not be rendered
/// verbatim. Today its only production writer is `config_file::resolve_policy`,
/// which matches a closed four-name set and rejects anything else — so this
/// is a structural guarantee rather than a fix for an observed leak, and it
/// is the difference between "no path exists today" and "no path can
/// exist". A statusline is a place arbitrary product text must not be able
/// to reach.
///
/// Returns the token rather than the prose because its caller is `explain`,
/// where the useful string is the one a user would type into `config.json`.
///
/// An unrecognised name yields `None`, which suppresses the profile segment
/// entirely rather than rendering a placeholder: `Running policy custom`
/// would be a claim about a policy this build cannot describe.
fn preset_label(name: &str) -> Option<&'static str> {
    KNOWN_PRESETS
        .iter()
        .find(|(token, _)| *token == name)
        .map(|(token, _)| *token)
}

/// The same lookup, returning the prose form a segment label may carry.
///
/// Separate from [`preset_label`] rather than replacing it: the two surfaces
/// want different strings for the same fact, and collapsing them would
/// either push an underscore into a label the host refuses or print prose
/// where a user needs the exact config token.
fn preset_display(name: &str) -> Option<&'static str> {
    KNOWN_PRESETS
        .iter()
        .find(|(token, _)| *token == name)
        .map(|(_, display)| *display)
}

/// The policy preset the daemon is *running*, when disk no longer agrees.
///
/// This is the contract's "report running truth, never the configured value
/// as if it were live" rule, and it is why the segment names the running
/// preset in its label and puts the disagreement in the reason clause
/// rather than the other way round. A user who has edited `config.json`
/// wants to know two things: that the edit has not taken effect, and what
/// is in effect instead.
///
/// Returns `None` only when the two agree, which is the ordinary case. The
/// alternative — always showing `Running policy balanced` — spends a column
/// on a constant, and `explain` reports the preset unconditionally for the
/// user who wants it.
///
/// A preset this build cannot name costs the *name*, never the warning. The
/// label falls back to the half of the fact that does not need a bounded
/// value, because the drift is the actionable half: a user whose edit has
/// not taken effect needs to know that whether or not this binary can
/// describe what it is running instead, and `explain` says which case it is.
/// Suppressing the segment there would make the one condition worth
/// reporting the one condition reported as silence.
fn profile_segment(doctor: &DoctorResult) -> Option<Value> {
    if doctor.running_config_matches_disk {
        return None;
    }
    let label = match preset_display(&doctor.policy_preset) {
        Some(preset) => format!("Running policy {preset}"),
        None => "Policy edit not in effect".to_string(),
    };
    Some(json!({
        "key": "profile",
        "state": "warn",
        "label": label,
        "reason_code": "config_edited_restart_required",
        "explain_key": "libra.profile",
        "order_hint": 40,
        // `vital`, not `exception`. A policy edit that has not taken effect
        // changes how every reading above it should be understood, which is
        // what a vital signal is for — but nothing is broken, nothing is
        // unavailable, and nobody is being waited on, so it is not the top
        // rung. Declaring the role also pins behaviour that the host used to
        // infer from `warn` being an emphatic state; an inferred vital is
        // never paired with an exception, and a declared one is.
        "clear_role": "vital",
    }))
}

/// `YYYY-MM-DDTHH:MM:SSZ`, which is what the host's `observed_at` accepts.
///
/// Built by hand rather than via `time`'s `Rfc3339`, which renders a UTC
/// offset as `+00:00`. The contract requires a literal `Z` and refuses an
/// offset form outright, because an age computed from a local offset is
/// ambiguous across machines. Whole seconds, which is all a statusline
/// could have displayed.
fn rfc3339_utc(now: OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
    )
}

/// Build the provider document from a successful probe.
///
/// `observed_at` is `now` and there is no `age_seconds`, because unlike a
/// stored finding this reading *is* live — the daemon holds `current_task`
/// in memory and answered just now. Claiming an age would invent a
/// staleness that does not exist.
///
/// No `cache_ttl_seconds` either. The state can change on any tool call,
/// and a host reusing a cached answer across that would render a superseded
/// plan as current. The founder's wrapper caches this at its own layer,
/// which is its choice to make; the provider does not authorise it.
///
/// At most four segments, which is the host's whole per-provider allowance:
/// the task, its estimate, its budget share, and a policy-drift warning.
/// Nothing is appended conditionally beyond those four, and the escalation
/// shares the task's segment rather than claiming a fifth — see
/// [`task_segment`]. A document with five segments is refused whole, so
/// Libra would render as nothing at the moment it had the most to say.
///
/// The wording preference is a parameter rather than something read in
/// here, deliberately. A function that reached for the user's state
/// directory could not be asserted on without the suite depending on
/// whatever preference the developer running it happens to have recorded,
/// and the alternative — a wrapper that reads the file and forwards it —
/// is a branch no unit test can reach, which is the same gap wearing a
/// function signature. So the read happens once, at the process edge in
/// [`run_provider`], beside the socket call and the clock.
pub fn reading(
    status: &StatusResult,
    doctor: Option<&DoctorResult>,
    now: OffsetDateTime,
    display: BudgetDisplay,
) -> Value {
    let mut segments = vec![task_segment(status.current_task.as_ref())];
    if let Some(task) = status.current_task.as_ref() {
        segments.push(estimate_segment(task));
        // Read from the snapshot rather than derived from it: the daemon
        // computed this share while answering the same `Status` request, so
        // Clear and Detail cannot disagree about it and no second round trip
        // was spent. A task without a budget field yields no segment rather
        // than a guessed one.
        segments.extend(
            status.task_budget.map(|posture| {
                budget_segment(posture, status.task_budget_amounts.as_ref(), display)
            }),
        );
    }
    segments.extend(doctor.and_then(profile_segment));

    json!({
        "contract_version": CONTRACT_VERSION,
        "provider": PROVIDER_ID,
        "provider_version": env!("CARGO_PKG_VERSION"),
        "scope": SCOPE,
        "availability": "available",
        "order_hint": ORDER_HINT,
        "clear_authority": CLEAR_AUTHORITY,
        "observed_at": rfc3339_utc(now),
        "segments": segments,
    })
}

/// Ask the daemon one question inside `deadline`.
///
/// One request per connection is the protocol (see
/// `libra-governor-protocol`'s transport docs), so each call reconnects.
/// The remaining budget is recomputed per call so the two round trips share
/// one deadline rather than getting one each.
fn ask(request: Request, deadline: Instant) -> Result<Response, NoReading> {
    let socket_path =
        libra_governor_daemon::paths::socket_path().map_err(|_| NoReading::SocketPathUnresolved)?;
    let remaining = deadline.saturating_duration_since(Instant::now());
    let stream = client::connect_only_with_budget(&socket_path, remaining)
        .map_err(|e| NoReading::from(&e))?;
    client::roundtrip(&stream, request).map_err(|e| NoReading::from(&e))
}

/// The live state, as far as it could be established inside `deadline`.
///
/// The `Doctor` half is deliberately not allowed to fail the whole probe:
/// its outcome is discarded to `None`, leaving the profile segment absent.
/// A diagnostic that could not be obtained must not cost the user the state
/// that could.
fn probe(deadline: Instant) -> Result<(StatusResult, Option<DoctorResult>), NoReading> {
    let status = match ask(Request::Status, deadline)? {
        Response::Status(status) => *status,
        _ => return Err(NoReading::ResponseNotUnderstood),
    };
    let doctor = match ask(Request::Doctor, deadline) {
        Ok(Response::Doctor(doctor)) => Some(*doctor),
        _ => None,
    };
    Ok((status, doctor))
}

/// `libra-governor statusline provider`.
///
/// Exits 0 in every case, including every failure. A non-zero exit would
/// make a stopped daemon indistinguishable from a broken provider, and the
/// document already says which it is — with a bounded reason code the host
/// can render and `doctor` can act on.
pub fn run_provider() {
    let payload = match probe(Instant::now() + HOT_PATH_BUDGET) {
        Ok((status, doctor)) => reading(
            &status,
            doctor.as_ref(),
            OffsetDateTime::now_utc(),
            presentation::load().budget_display(),
        ),
        Err(kind) => no_reading(kind),
    };
    println!("{payload}");
}

/// Prose for a [`ReplanState`], spelled out.
///
/// The escalated case is the reason this surface exists. Four lines of
/// explanation is the right price for a state whose name
/// (`EscalatedAwaitingApproval`) misdescribes it, and the statusline has
/// room for none of them.
fn replan_state_prose(state: &ReplanState) -> String {
    match state {
        ReplanState::Stable => "stable, no automatic replan yet".to_string(),
        ReplanState::Replanned { count } => {
            format!("automatically replanned {count} time(s) so far")
        }
        ReplanState::EscalatedAwaitingApproval => "automatic-replan budget spent".to_string(),
    }
}

/// Prose for a [`BudgetPosture`], spelled out at `explain` length.
///
/// The percentage carries its axis here too. The host's label rule does not
/// reach this surface — it is plain text, not a contract field — but a bare
/// `38%` is ambiguous wherever it is read, and a user comparing the
/// statusline against `explain` should not have to work out whether the two
/// are even measuring the same direction.
///
/// `None` only reaches here if a daemon answered with a task and no budget
/// field, which the current daemon does not do; it is reported as
/// unestablished rather than silently omitted, because a missing line reads
/// as "there is no budget" and that is a claim this function cannot make.
fn budget_posture_prose(posture: Option<BudgetPosture>) -> String {
    match posture {
        Some(BudgetPosture::Remaining { fraction_left }) => match percent_left(fraction_left) {
            Some(percent) => format!(
                "{percent}% budget left — a share of the task's limit, not an\n\
                 \x20                  amount; the limit itself and what has been spent are\n\
                 \x20                  resource figures and stay off every rendering surface"
            ),
            None => "not reported — the daemon's share was not a usable number".to_string(),
        },
        // The one line here that explains an *omission*, so it says what
        // would otherwise be assumed: the share is withheld deliberately,
        // and the envelope being intact is not the same fact as the work
        // having been measured.
        Some(BudgetPosture::Uncommitted) => {
            "unknown — this task holds an envelope but nothing has been \
             committed\n\x20                  against it, so there is no measured share to \
             report; an\n\x20                  untouched envelope is not the same as a \
             measured full one"
                .to_string()
        }
        Some(BudgetPosture::Exhausted) => {
            "exhausted — the next reservation will be refused; the task is \
             not\n\x20                  stopped, because Libra's hooks are advisory"
                .to_string()
        }
        Some(BudgetPosture::NotEstablished) | None => {
            "none — this task was admitted without a resource envelope, which\n\
             \x20                  is not the same as having spent one"
                .to_string()
        }
        Some(BudgetPosture::Unreadable) => {
            "unknown — the local reservation ledger could not be read, so this\n\
             \x20                  is a missing reading rather than an absent budget"
                .to_string()
        }
    }
}

/// A possibly-fractional figure, grouped, for the surfaces that carry a
/// ledger amount as text rather than as a contract field (HORO-1709): the
/// compact label, via [`span_figure`], and `explain`.
///
/// The contract's `count` is a `u32`, so [`count_value`] can decline a
/// figure outright. A ledger figure is an `f64` and three things can be
/// true of it that no
/// `count` can express — a fraction, a negative, and a magnitude past four
/// billion — and `explain` has both the room and the obligation to show
/// all three rather than withhold the figure. Fractions are printed to one
/// place and only when there is one, so a token count does not acquire a
/// `.0` it never had.
///
/// Past 2^53 an `f64`'s integers are no longer consecutive, so the digits
/// printed here are the exact value of the number held rather than the
/// figure anyone intended. That is still the honest rendering of what the
/// ledger returned, and no envelope anywhere near that width exists; the
/// alternative — rounding to something tidier — would print a figure the
/// ledger does not hold.
fn grouped_f64(value: f64) -> String {
    if !value.is_finite() {
        return "not a usable number".to_string();
    }
    let sign = if value < 0.0 { "-" } else { "" };
    let magnitude = value.abs();
    let whole = magnitude.trunc();
    let fraction = magnitude - whole;
    let whole = group_digits(&format!("{whole:.0}"));
    if fraction < 0.05 {
        format!("{sign}{whole}")
    } else {
        format!("{sign}{whole}.{:.0}", fraction * 10.0)
    }
}

/// The read-only long form of the same state, for the shared `explain`
/// surface.
///
/// Given more room than a statusline, this says everything bounded that the
/// daemon holds — and says plainly what it does *not* hold, which is the
/// question a user arrives here with. Spans are printed as raw seconds
/// rather than as `12m`: the host owns span formatting for the contract, and
/// a second formatter in this file is a second place for the two to
/// disagree about what a minute is.
///
/// What it never prints: prompts, task or tool content, the estimator's
/// free-text `reason`, resource or cost figures of any kind, a credential,
/// or a path. The ids it does print are local, ephemeral and the only
/// handle a reader has on the work — unlike Fornax's claim and session ids,
/// which answer no question this surface is asked.
pub fn explain_text(status: &StatusResult, doctor: Option<&DoctorResult>) -> String {
    let mut out = String::from("Libra — the task being governed on this machine (host-wide)\n\n");

    match status.current_task.as_ref() {
        None => {
            out.push_str("  No task is being governed right now. The daemon is running\n");
            out.push_str("  and has admitted nothing, which is not a judgement about\n");
            out.push_str("  anything it has admitted before.\n");
        }
        Some(task) => {
            let estimate = &task.remaining_estimate;
            out.push_str(&format!("  task             {}\n", task.task_id));
            out.push_str(&format!("  plan             {}\n", task.plan_id.0));
            match estimate.duration_p90_secs {
                Some(p90) => out.push_str(&format!("  remaining P90    {p90}s\n")),
                None if estimate.cold_start => {
                    out.push_str("  remaining P90    none — no local history yet\n")
                }
                None => out.push_str("  remaining P90    none — no duration bound computed\n"),
            }
            if let Some(p50) = estimate.duration_p50_secs {
                out.push_str(&format!("  remaining P50    {p50}s\n"));
            }
            out.push_str(&format!(
                "  confidence       {} — confidence in the remaining-work estimate,\n\
                 \x20                  not a risk or priority rating for the task\n",
                confidence_token(task.confidence)
            ));
            // The existing shared helper, not a third description of the
            // same taxonomy: a preflight, the receipt that finalizes it and
            // this surface must describe an estimate's evidentiary basis
            // identically, or a user comparing them sees a discrepancy that
            // is purely in the prose.
            out.push_str(&format!(
                "  estimate basis   {}\n",
                crate::bucket_prose::describe_bucket_tier(
                    estimate.bucket_tier,
                    estimate.sample_count
                )
            ));
            out.push_str(&format!(
                "  estimator        {}\n",
                estimate.estimator_version
            ));
            out.push_str(&format!(
                "  recon cost       {:.1}s\n",
                task.recon_cost_seconds
            ));
            out.push_str(&format!(
                "  replan state     {}\n",
                replan_state_prose(&task.replan_state)
            ));
            out.push_str(&format!(
                "  budget           {}\n",
                budget_posture_prose(status.task_budget)
            ));
        }
    }

    if status
        .current_task
        .as_ref()
        .is_some_and(|task| task.replan_state == ReplanState::EscalatedAwaitingApproval)
    {
        out.push_str("\n  This task has used up its automatic-replan budget, so the next\n");
        out.push_str("  material deviation will not be silently replanned again. Nothing\n");
        out.push_str("  is blocked and nothing is waiting on an answer from you — Libra's\n");
        out.push_str("  hooks are advisory, and the task is still running. What has\n");
        out.push_str("  changed is that the plan and estimate above will no longer be\n");
        out.push_str("  corrected automatically, so they are worth re-reading yourself.\n");
    }

    out.push('\n');
    match doctor {
        None => out.push_str(
            "  running policy   not established — the daemon did not answer the\n\
                              \x20                  diagnostic request\n",
        ),
        Some(doctor) => {
            match preset_label(&doctor.policy_preset) {
                Some(preset) => out.push_str(&format!("  running policy   {preset}\n")),
                None => out.push_str("  running policy   a preset this build does not recognise\n"),
            }
            if doctor.running_config_matches_disk {
                out.push_str("  config.json      agrees with the running policy\n");
            } else {
                out.push_str("  config.json      has been edited since the daemon started; the\n");
                out.push_str(
                    "                   policy above is what is actually in force, and a\n",
                );
                out.push_str("                   daemon restart is what would apply the edit\n");
            }
        }
    }

    out.push_str("\n  Not shown here: prompts, task or tool content, the estimator's\n");
    out.push_str("  free-text reasoning, and its resource/cost quantiles. Libra keeps\n");
    out.push_str("  that material local and off every rendering surface.\n");
    out
}

/// `libra-governor statusline explain`.
///
/// Exits 0 in every case, for the same reason [`run_provider`] does: a
/// non-zero exit would make a stopped daemon indistinguishable from a
/// broken command, and the text already says which it is.
pub fn run_explain() {
    match probe(Instant::now() + EXPLAIN_BUDGET) {
        Ok((status, doctor)) => print!("{}", explain_text(&status, doctor.as_ref())),
        Err(kind) => {
            println!("Libra — the task being governed on this machine (host-wide)\n");
            println!("  no reading       {}", kind.label());
            println!("  reason           {}", kind.reason_code());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`super::reading`] with the wording preference pinned to the
    /// pre-ticket default, shadowing the real one for the whole test
    /// module so that the tests predating HORO-1709 keep reading as
    /// three-argument calls about documents rather than about a
    /// preference none of them is concerned with.
    fn reading(status: &StatusResult, doctor: Option<&DoctorResult>, now: OffsetDateTime) -> Value {
        super::reading(status, doctor, now, BudgetDisplay::Percent)
    }
    use libra_governor_domain::{
        BucketTier, BudgetSnapshot, Estimate, PlanId, ResourceAmount, ResourceKind, TaskId,
    };
    use libra_governor_protocol::ConfiguredBudget;

    // ---------------------------------------------------------------- fixtures

    fn estimate_with(p90: Option<u64>, cold_start: bool, samples: usize) -> Estimate {
        Estimate {
            duration_p50_secs: p90.map(|s| s / 2),
            duration_p80_secs: p90.map(|s| s * 4 / 5),
            duration_p90_secs: p90,
            // Present in the source state on purpose: the assertions below
            // prove these never reach a rendering surface, and a fixture
            // that left them `None` would prove nothing.
            resource_p50: Some(ResourceAmount::UsdCents(1234)),
            resource_p80: Some(ResourceAmount::UsdCents(2345)),
            resource_p90: Some(ResourceAmount::UsdCents(3456)),
            confidence: Confidence::Medium,
            sample_count: samples,
            cold_start,
            estimator_version: "v3-tiered-confidence".to_string(),
            reason: Some(ESTIMATOR_FREE_TEXT.to_string()),
            feature_schema_version: "fs-v1".to_string(),
            bucket_tier: BucketTier::Repo,
            regime: Default::default(),
        }
    }

    /// Stands in for whatever the estimator actually wrote. Free text by
    /// type, so the only safe treatment is to never render it — which is
    /// what a search for this sentinel proves.
    const ESTIMATOR_FREE_TEXT: &str =
        "the repo looked like /Users/someone/secret-project with token sk-live-AbC123";

    fn task(replan_state: ReplanState) -> TaskSummary {
        TaskSummary {
            task_id: TaskId::new(),
            confidence: Confidence::Medium,
            recon_cost_seconds: 2.1,
            plan_id: PlanId::new(),
            remaining_estimate: estimate_with(Some(600), false, 7),
            replan_state,
        }
    }

    fn status_with(replan_state: ReplanState) -> StatusResult {
        StatusResult {
            current_task: Some(task(replan_state)),
            // The ordinary active shape: an envelope with room left in it.
            // 0.38 rather than a round fraction so a truncation or rounding
            // bug shows up as a wrong digit instead of as the right one by
            // luck.
            task_budget: Some(BUDGET_LEFT),
            task_budget_amounts: Some(BUDGET_LEFT_AMOUNTS),
            configured_budget: Some(CONFIGURED),
        }
    }

    /// The share the fixtures use for "a healthy active task".
    const BUDGET_LEFT: BudgetPosture = BudgetPosture::Remaining {
        fraction_left: 0.38,
    };

    /// The amounts that *produce* [`BUDGET_LEFT`]: a 150,000-token
    /// envelope with 18,000 settled and 75,000 held, so 57,000 remain and
    /// `57_000 / 150_000` is exactly `0.38`.
    ///
    /// Tokens, not currency, because every envelope a live Libra daemon
    /// writes is denominated in tokens (HORO-1725/HORO-1727) — a dollar
    /// fixture here would be testing a shape the product does not
    /// produce. `budget_fixtures_classify_to_the_posture_they_are_paired_with`
    /// below pins this against [`BudgetPosture::from_snapshot`] so the
    /// pair cannot drift into describing two different envelopes.
    const BUDGET_LEFT_AMOUNTS: BudgetSnapshot = BudgetSnapshot::new(
        ResourceKind::Tokens,
        150_000.0,
        30_000.0,
        18_000.0,
        75_000.0,
        2,
    );

    /// The daemon's running ceiling — a different scope from the active
    /// task's envelope, and deliberately a different number from
    /// [`BUDGET_LEFT_AMOUNTS`]'s ceiling so a test cannot pass by
    /// accidentally reading one for the other.
    const CONFIGURED: ConfiguredBudget = ConfiguredBudget {
        ceiling: ResourceAmount::Tokens(200_000),
    };

    /// The amounts behind a posture, for the fixtures that vary it.
    ///
    /// `None` for the two postures that have no snapshot behind them by
    /// construction: `NotEstablished` means there is no budget row to
    /// read, `Unreadable` means the read failed. Both are the daemon's
    /// `(posture, None)` shape, not a snapshot that happens to look
    /// empty.
    fn amounts_for(posture: BudgetPosture) -> Option<BudgetSnapshot> {
        Some(match posture {
            BudgetPosture::Remaining { .. } => BUDGET_LEFT_AMOUNTS,
            // An envelope with no reservation row in any state: the
            // HORO-1708 case.
            BudgetPosture::Uncommitted => {
                BudgetSnapshot::new(ResourceKind::Tokens, 150_000.0, 30_000.0, 0.0, 0.0, 0)
            }
            BudgetPosture::Exhausted => {
                BudgetSnapshot::new(ResourceKind::Tokens, 150_000.0, 30_000.0, 150_000.0, 0.0, 4)
            }
            BudgetPosture::NotEstablished | BudgetPosture::Unreadable => return None,
        })
    }

    /// Sets the budget half of a fixture reply the way the daemon sets
    /// it: the posture and the amounts together, from one snapshot, never
    /// one without the other (HORO-1709).
    ///
    /// A test that assigned only `task_budget` would be describing a wire
    /// message the daemon cannot send — and, worse, would quietly stop
    /// exercising the amounts path while still looking like it covered
    /// that posture.
    fn set_budget(status: &mut StatusResult, posture: Option<BudgetPosture>) {
        status.task_budget_amounts = posture.and_then(amounts_for);
        status.task_budget = posture;
    }

    /// Every budget shape the daemon can report, plus the absence of the
    /// field.
    ///
    /// `None` is not reachable from the current daemon — it computes a
    /// posture for every task it holds — but the provider parses a wire
    /// message rather than calling a function, so a peer that omitted the
    /// field must still produce a truthful document, and the all-documents
    /// properties below have to cover that.
    const ALL_BUDGETS: [(&str, Option<BudgetPosture>); 6] = [
        ("absent", None),
        ("remaining", Some(BUDGET_LEFT)),
        ("uncommitted", Some(BudgetPosture::Uncommitted)),
        ("exhausted", Some(BudgetPosture::Exhausted)),
        ("unestablished", Some(BudgetPosture::NotEstablished)),
        ("unreadable", Some(BudgetPosture::Unreadable)),
    ];

    /// The idle shape: no task, and so no share and no amounts of a
    /// budget that was never admitted.
    ///
    /// `configured_budget` is nonetheless `Some`, because that is what an
    /// idle daemon really sends (HORO-1709): the ceiling the *next* task
    /// would get is a fact it knows authoritatively even with nothing
    /// running. Keeping it populated here is what makes the idle
    /// assertions below load-bearing — they prove the provider does not
    /// turn a configured setting into an active-task reading, rather than
    /// proving it has nothing to turn.
    fn idle() -> StatusResult {
        StatusResult {
            current_task: None,
            task_budget: None,
            task_budget_amounts: None,
            configured_budget: Some(CONFIGURED),
        }
    }

    /// Fixture-drift guard. Each `(posture, amounts)` pair above must
    /// describe one envelope: the amounts, classified by the daemon's own
    /// rule, must come back as the posture they are paired with. Without
    /// this, a fixture could pair "38% left" with an exhausted snapshot
    /// and every assertion built on it would be testing a state the
    /// product cannot reach.
    #[test]
    fn budget_fixtures_classify_to_the_posture_they_are_paired_with() {
        for posture in [
            BUDGET_LEFT,
            BudgetPosture::Uncommitted,
            BudgetPosture::Exhausted,
        ] {
            let amounts = amounts_for(posture).expect("these three have amounts behind them");
            assert_eq!(
                BudgetPosture::from_snapshot(&amounts),
                posture,
                "fixture amounts do not classify to {posture:?}"
            );
        }
        for posture in [BudgetPosture::NotEstablished, BudgetPosture::Unreadable] {
            assert!(
                amounts_for(posture).is_none(),
                "{posture:?} has no snapshot behind it by construction"
            );
        }
    }

    fn doctor_with(preset: &str, matches_disk: bool) -> DoctorResult {
        DoctorResult {
            daemon_version: "0.0.2".to_string(),
            protocol_version: 8,
            schema_version_applied: 9,
            schema_version_known: 9,
            schema_ahead_of_binary: false,
            policy_preset: preset.to_string(),
            config_file_present: true,
            config_file_valid: true,
            config_file_error: None,
            running_config_matches_disk: matches_disk,
            gateway_configured: false,
            gateway_running: false,
            gateway_disabled_reason: None,
            gateway_capabilities: None,
            gateway_credential_configured: false,
            telemetry_enabled: false,
            extension_business_context_configured: false,
            extension_policy_webhook_configured: false,
            extension_events_configured: false,
            extension_events_pending: 0,
            extension_config_error: None,
        }
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_770_000_000).unwrap()
    }

    const ALL_NO_READINGS: [NoReading; 6] = [
        NoReading::SocketPathUnresolved,
        NoReading::DaemonUnreachable,
        NoReading::DaemonTooSlow,
        NoReading::DaemonProtocolMismatch,
        NoReading::DaemonReportedError,
        NoReading::ResponseNotUnderstood,
    ];

    fn segments(document: &Value) -> &Vec<Value> {
        document["segments"].as_array().unwrap()
    }

    /// Every document this provider can emit, for the properties that must
    /// hold in *all* of them rather than in a chosen one.
    fn every_document() -> Vec<(String, Value)> {
        let mut out: Vec<(String, Value)> = ALL_NO_READINGS
            .iter()
            .map(|kind| {
                (
                    format!("no_reading/{}", kind.reason_code()),
                    no_reading(*kind),
                )
            })
            .collect();
        let states = [
            ("stable", ReplanState::Stable),
            ("replanned", ReplanState::Replanned { count: 3 }),
            ("escalated", ReplanState::EscalatedAwaitingApproval),
        ];
        let doctors = [
            ("no-doctor", None),
            ("agrees", Some(doctor_with("balanced", true))),
            ("drifted", Some(doctor_with("strict_budget", false))),
            ("unknown-preset", Some(doctor_with("custom-thing", false))),
        ];
        for (state_name, state) in states {
            for (doctor_name, doctor) in &doctors {
                for (estimate_name, p90) in [("with-p90", Some(600u64)), ("no-p90", None)] {
                    for (budget_name, budget) in ALL_BUDGETS {
                        let mut status = status_with(state);
                        status.current_task.as_mut().unwrap().remaining_estimate =
                            estimate_with(p90, p90.is_none(), 7);
                        set_budget(&mut status, budget);
                        out.push((
                            format!(
                                "reading/{state_name}/{doctor_name}/{estimate_name}/{budget_name}"
                            ),
                            reading(&status, doctor.as_ref(), now()),
                        ));
                    }
                }
            }
        }
        out.push((
            "reading/idle".to_string(),
            reading(&idle(), Some(&doctor_with("balanced", true)), now()),
        ));
        out
    }

    fn walk_strings(value: &Value, path: &str, out: &mut Vec<(String, String)>) {
        match value {
            Value::String(s) => out.push((path.to_string(), s.clone())),
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    walk_strings(item, &format!("{path}[{i}]"), out);
                }
            }
            Value::Object(map) => {
                for (key, item) in map {
                    let child = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    walk_strings(item, &child, out);
                }
            }
            _ => {}
        }
    }

    fn all_strings(document: &Value) -> Vec<(String, String)> {
        let mut out = Vec::new();
        walk_strings(document, "", &mut out);
        out
    }

    // ------------------------------------------------------------- identity

    #[test]
    fn the_document_states_the_registered_identity_on_every_answer() {
        // Including the failures: a host that cannot tell which provider or
        // which contract version produced a document cannot render it, and
        // "the failing one" is exactly when it most needs to say whose
        // failure it is.
        for (name, document) in every_document() {
            assert_eq!(
                document["contract_version"],
                json!(CONTRACT_VERSION),
                "{name}"
            );
            assert_eq!(document["provider"], json!("libra"), "{name}");
            assert_eq!(document["scope"], json!("host"), "{name}");
            assert_eq!(document["order_hint"], json!(ORDER_HINT), "{name}");
            assert!(
                document["provider_version"].is_string(),
                "{name}: a version the host can report in doctor"
            );
        }
    }

    // ----------------------------------------------------------- no reading

    #[test]
    fn a_failed_probe_is_never_silence_and_never_a_pass() {
        // The two failure modes the contract exists to prevent, in one
        // assertion each: an empty segment list renders as nothing, and
        // nothing on a statusline reads as all-clear; an `ok` state on a
        // probe that established nothing is a health claim from a provider
        // that measured no health.
        for kind in ALL_NO_READINGS {
            let document = no_reading(kind);
            let code = kind.reason_code();
            assert_ne!(document["availability"], json!("available"), "{code}");
            assert_eq!(segments(&document).len(), 1, "{code}");
            let segment = &segments(&document)[0];
            assert_ne!(segment["state"], json!("ok"), "{code}");
            assert_eq!(segment["reason_code"], json!(code));
            assert!(
                segment["label"].as_str().is_some_and(|l| !l.is_empty()),
                "{code}: something must be rendered"
            );
        }
    }

    #[test]
    fn a_stopped_daemon_a_slow_one_and_a_broken_one_are_three_facts() {
        // `UNKNOWN != UNAVAILABLE`. Reporting a slow daemon as not running
        // sends the user to start one that is already up; reporting a
        // version mismatch as not running hides the restart that would fix
        // it.
        assert_eq!(
            no_reading(NoReading::DaemonUnreachable)["availability"],
            json!("unavailable")
        );
        assert_eq!(
            no_reading(NoReading::DaemonTooSlow)["availability"],
            json!("unknown")
        );
        assert_eq!(
            no_reading(NoReading::DaemonProtocolMismatch)["availability"],
            json!("error")
        );

        let codes: Vec<&str> = ALL_NO_READINGS.iter().map(|k| k.reason_code()).collect();
        let mut unique = codes.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            unique.len(),
            codes.len(),
            "each outcome needs its own reason code, or doctor cannot tell them apart"
        );
    }

    #[test]
    fn a_failed_probe_reports_no_number_at_all() {
        // `UNAVAILABLE != ZERO`, and its neighbours: a count of 0, a
        // duration of 0 or a confidence attached to a reading that does not
        // exist all read as measurements. There were none.
        for kind in ALL_NO_READINGS {
            for segment in segments(&no_reading(kind)) {
                for field in ["count", "duration_seconds", "confidence", "confidence_of"] {
                    assert!(
                        segment.get(field).is_none(),
                        "{}: {field} must be absent, not zero",
                        kind.reason_code()
                    );
                }
            }
        }
    }

    #[test]
    fn a_client_error_never_carries_its_socket_path_into_the_label() {
        // `ClientError::DaemonUnavailable` embeds the socket path and its
        // `Display` renders it. That string is one `format!` away from a
        // statusline, so the classification deliberately drops it and the
        // label is a fixed literal.
        let error = ClientError::DaemonUnavailable(
            std::path::PathBuf::from("/Users/someone/.local/state/libra/daemon.sock"),
            "No such file or directory".to_string(),
        );
        assert!(error.to_string().contains("/Users/someone"), "premise");

        let kind = NoReading::from(&error);
        assert_eq!(kind, NoReading::DaemonUnreachable);
        for (path, text) in all_strings(&no_reading(kind)) {
            assert!(!text.contains('/'), "{path} = {text:?}");
            assert!(!text.contains("someone"), "{path} = {text:?}");
        }
    }

    #[test]
    fn a_socket_timeout_is_classified_as_slow_not_as_unreadable() {
        // Both kinds are observed in practice — which one a socket timeout
        // surfaces as depends on the platform and on whether the read or
        // the write side expired.
        for kind in [std::io::ErrorKind::WouldBlock, std::io::ErrorKind::TimedOut] {
            let error = ClientError::Io(std::io::Error::from(kind));
            assert_eq!(
                NoReading::from(&error),
                NoReading::DaemonTooSlow,
                "{kind:?}"
            );
        }
        let other = ClientError::Io(std::io::Error::from(std::io::ErrorKind::InvalidData));
        assert_eq!(NoReading::from(&other), NoReading::ResponseNotUnderstood);
    }

    // --------------------------------------------------------------- task

    #[test]
    fn a_governed_task_is_reported_as_a_state_not_as_a_verdict() {
        let document = reading(&status_with(ReplanState::Stable), None, now());
        let task = &segments(&document)[0];
        assert_eq!(task["key"], json!("task"));
        assert_eq!(
            task["state"],
            json!("neutral"),
            "`ok` would assert the work is going well, which Libra has not measured"
        );
        assert!(task["label"].as_str().unwrap().starts_with("Task "));
    }

    #[test]
    fn no_task_is_reported_explicitly_rather_than_by_omission() {
        let document = reading(&idle(), None, now());
        assert_eq!(document["availability"], json!("available"));
        assert_eq!(segments(&document).len(), 1);
        assert_eq!(
            segments(&document)[0]["label"],
            json!("No task being governed")
        );
        assert_eq!(segments(&document)[0]["state"], json!("neutral"));
    }

    #[test]
    fn the_replan_count_rides_the_task_it_describes_and_zero_is_omitted() {
        let replanned = reading(
            &status_with(ReplanState::Replanned { count: 3 }),
            None,
            now(),
        );
        let task = &segments(&replanned)[0];
        assert_eq!(task["count"], json!(3));
        assert_eq!(
            task["count_label"],
            json!("replans"),
            "a bare 3 beside a task id means nothing"
        );

        for state in [ReplanState::Stable, ReplanState::Replanned { count: 0 }] {
            let document = reading(&status_with(state), None, now());
            assert!(
                segments(&document)[0].get("count").is_none(),
                "{state:?}: 0 replans spends columns to say nothing"
            );
        }
    }

    #[test]
    fn the_task_segment_carries_one_identifier_not_two() {
        // The legacy line carried the plan id as well. Two opaque
        // eight-character identifiers on a line shared with the user's own
        // statusline and every other product is what `explain` is for.
        let status = status_with(ReplanState::Stable);
        let plan = status.current_task.as_ref().unwrap().plan_id.0.to_string();
        let document = reading(&status, None, now());
        for (path, text) in all_strings(&document) {
            assert!(!text.contains(&plan[..8]), "{path} = {text:?}");
        }
    }

    // ----------------------------------------------------------- estimate

    #[test]
    fn a_span_is_sent_as_seconds_and_a_noun_never_as_rendered_text() {
        let document = reading(&status_with(ReplanState::Stable), None, now());
        let estimate = &segments(&document)[1];
        assert_eq!(estimate["key"], json!("estimate"));
        assert_eq!(estimate["duration_seconds"], json!(600));
        assert_eq!(estimate["duration_label"], json!("P90"));
        assert_eq!(estimate["label"], json!("Remaining work"));
        for (path, text) in all_strings(document.get("segments").unwrap()) {
            assert!(
                !text.contains("10m") && !text.contains("600s"),
                "{path} = {text:?}: the host owns span formatting"
            );
        }
    }

    #[test]
    fn a_confidence_always_says_what_it_is_a_confidence_in() {
        // The `pf:high` regression, structurally. A bare `high` beside a
        // task reads as a risk or priority rating; the host will only render
        // the qualifying prose if the pair is present.
        for (name, document) in every_document() {
            for segment in segments(&document) {
                if segment.get("confidence").is_some() {
                    assert_eq!(
                        segment["confidence_of"],
                        json!("preflight_estimate"),
                        "{name}"
                    );
                }
                assert!(
                    segment.get("confidence_of").is_none() || segment.get("confidence").is_some(),
                    "{name}: a qualifier with nothing to qualify"
                );
            }
        }
    }

    #[test]
    fn a_confidence_rides_the_estimate_it_qualifies() {
        // One segment, not two. A separate segment would put the qualifier
        // a degradation step away from the number it qualifies, which is
        // how the abbreviation came to be read as a rating in the first
        // place.
        let document = reading(&status_with(ReplanState::Stable), None, now());
        let estimate = &segments(&document)[1];
        assert_eq!(estimate["confidence"], json!("medium"));
        assert!(estimate.get("duration_seconds").is_some());
    }

    #[test]
    fn an_estimate_with_no_span_carries_no_confidence_in_that_span() {
        let mut status = status_with(ReplanState::Stable);
        status.current_task.as_mut().unwrap().remaining_estimate = estimate_with(None, true, 0);
        let document = reading(&status, None, now());
        let estimate = &segments(&document)[1];
        assert_eq!(estimate["state"], json!("unknown"));
        assert!(estimate.get("duration_seconds").is_none());
        assert!(
            estimate.get("confidence").is_none(),
            "a confidence in an absent number reads as a judgement about the task"
        );
    }

    #[test]
    fn a_missing_span_is_explained_from_cold_start_not_from_a_sample_count() {
        // The contract's "do not infer a reason" rule. `Estimate::cold_start`
        // is authoritative and its own docs promise it is never inferred
        // from confidence; a sample count is not a reason.
        let reason = |cold_start: bool, samples: usize| {
            let mut status = status_with(ReplanState::Stable);
            status.current_task.as_mut().unwrap().remaining_estimate =
                estimate_with(None, cold_start, samples);
            segments(&reading(&status, None, now()))[1]["reason_code"].clone()
        };

        assert_eq!(reason(true, 0), json!("no_local_history_yet"));
        assert_eq!(
            reason(true, 40),
            json!("no_local_history_yet"),
            "the count must not override the authoritative field"
        );
        assert_eq!(reason(false, 0), json!("estimate_has_no_duration_bound"));
        assert_ne!(
            reason(true, 7),
            reason(false, 7),
            "same count, different fact: the reason must come from cold_start"
        );
    }

    // ------------------------------------------------------------- budget

    /// The one label shape a budget share may ever take.
    fn budget_label(document: &Value) -> Option<String> {
        segments(document)
            .iter()
            .find(|s| s["key"] == json!("budget"))
            .map(|s| s["label"].as_str().unwrap().to_string())
    }

    fn with_budget(posture: Option<BudgetPosture>) -> Value {
        let mut status = status_with(ReplanState::Stable);
        set_budget(&mut status, posture);
        reading(&status, None, now())
    }

    #[test]
    fn a_budget_share_is_reported_as_a_share_with_its_axis_named() {
        let document = with_budget(Some(BudgetPosture::Remaining {
            fraction_left: 0.38,
        }));
        let budget = segments(&document)
            .iter()
            .find(|s| s["key"] == json!("budget"))
            .expect("an active task's envelope is a reading, not a detail");
        assert_eq!(budget["label"], json!("38% budget left"));
        assert_eq!(budget["state"], json!("neutral"));
        assert!(
            budget.get("count").is_none() && budget.get("total").is_none(),
            "a share is not a counter; counters are dropped at Clear depth"
        );
    }

    #[test]
    fn a_share_that_cannot_be_a_share_is_named_truthfully() {
        // `percent_left` rejects three different malformations, and the one
        // label it produces has to be true of all three: a NaN, a share of
        // exactly zero and a share above one are different defects, and only
        // the first is "not a number". None of these can reach the wire from
        // this build's daemon — `budget_posture` decides exhaustion on the
        // headroom before it ever divides — which is precisely why the branch
        // is asserted here. A guard no test executes is a guard nobody has
        // read, and this one's label is the only thing the user would see.
        for fraction in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -0.5, 1.5] {
            // Deliberately the share-only shape, with no amounts beside
            // it. A malformed share *next to* a well-formed snapshot is
            // not this state: the snapshot is the authority and the
            // provider recomputes the share from it, so pairing the two
            // would be testing which of two contradictory numbers wins
            // rather than how an unusable share is worded.
            let mut status = status_with(ReplanState::Stable);
            status.task_budget = Some(BudgetPosture::Remaining {
                fraction_left: fraction,
            });
            status.task_budget_amounts = None;
            let document = reading(&status, None, now());
            let budget = segments(&document)
                .iter()
                .find(|s| s["key"] == json!("budget"))
                .expect("a malformed share is still a reading, never silence")
                .clone();
            assert_eq!(
                budget["label"],
                json!("Budget share not usable"),
                "{fraction} earned a label that narrows the cause untruthfully"
            );
            assert_eq!(
                budget["reason_code"],
                json!("budget_share_not_usable"),
                "{fraction}"
            );
            assert_eq!(budget["state"], json!("warn"), "{fraction}");
            // Still a vital, not an exception: a share this provider cannot
            // use is a reading it is missing, not a limit the task has hit.
            assert_eq!(budget["clear_role"], json!("vital"), "{fraction}");
            assert!(
                !budget["label"].as_str().unwrap().contains('%'),
                "{fraction} printed arithmetic instead of declining to"
            );
        }
    }

    #[test]
    fn no_percentage_this_provider_can_emit_lacks_an_axis() {
        // The AC is "satisfied by construction, not by luck", so this sweeps
        // the whole input domain rather than the fixture: every fraction that
        // produces a percentage must produce one with its direction named.
        // The axis lives in the format template, so there is no code path
        // that can print the number without it — this proves the claim over
        // every number the template can be handed.
        //
        // Share-only, so the sweep's fraction is the one rendered: with
        // amounts present the snapshot is the authority and all 1001
        // iterations would render the fixture's 38%, which would sweep
        // nothing.
        let mut seen_percent = 0;
        for permille in 0..=1000u32 {
            let fraction = f64::from(permille) / 1000.0;
            let mut status = status_with(ReplanState::Stable);
            status.task_budget = Some(BudgetPosture::Remaining {
                fraction_left: fraction,
            });
            status.task_budget_amounts = None;
            let document = reading(&status, None, now());
            let label = budget_label(&document).expect("a budget is always reported");
            if !label.contains('%') {
                // A fraction outside `0.0..=1.0` (only 0.0 here) is reported
                // as an unusable share rather than as a percentage — see
                // `a_share_that_cannot_be_a_share_is_named_truthfully`.
                assert_eq!(fraction, 0.0, "{fraction}: {label}");
                continue;
            }
            seen_percent += 1;
            assert!(
                label.ends_with("% budget left"),
                "{fraction} rendered {label:?} with no axis"
            );
        }
        assert_eq!(
            seen_percent, 1000,
            "the sweep must actually have produced percentages"
        );

        // And across every document the provider can emit, including the
        // no-reading and idle shapes, so a future field cannot reintroduce a
        // bare number somewhere else.
        for (name, document) in every_document() {
            for (path, text) in all_strings(&document) {
                if !text.contains('%') {
                    continue;
                }
                assert!(
                    text.split_whitespace()
                        .filter(
                            |word| word.chars().filter(|c| c.is_ascii_alphabetic()).count() >= 2
                        )
                        .count()
                        >= 1,
                    "{name}: {path} = {text:?} is a percentage with no noun"
                );
            }
        }
    }

    #[test]
    fn a_share_is_truncated_floored_and_never_fabricated() {
        // Truncation never over-reports what is left, which is the direction
        // a budget figure has to err in. The floor at 1 keeps `0%` reserved
        // for an envelope that is actually spent.
        assert_eq!(percent_left(1.0), Some(100));
        assert_eq!(
            percent_left(0.999),
            Some(99),
            "rounding up would claim 100%"
        );
        assert_eq!(percent_left(0.38), Some(38));
        assert_eq!(
            percent_left(0.004),
            Some(1),
            "a served task must not read as 0% left"
        );
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.5, 1.5, 0.0] {
            assert_eq!(percent_left(bad), None, "{bad}");
        }
    }

    #[test]
    fn an_exhausted_envelope_is_an_exception_and_still_not_an_emergency() {
        let document = with_budget(Some(BudgetPosture::Exhausted));
        let budget = segments(&document)
            .iter()
            .find(|s| s["key"] == json!("budget"))
            .unwrap();
        assert_eq!(budget["label"], json!("Budget exhausted"));
        assert_eq!(
            budget["clear_role"],
            json!("exception"),
            "a limit that has been reached is not a posture about the work"
        );
        assert_eq!(
            budget["state"],
            json!("warn"),
            "`critical` would claim work has stopped; Libra's hooks are advisory"
        );
        assert_eq!(budget["reason_code"], json!("budget_hard_limit_reached"));
    }

    #[test]
    fn an_uncommitted_envelope_is_reported_as_an_unknown_and_never_as_a_full_one() {
        // HORO-1708. The daemon used to send `Remaining { 1.0 }` here and
        // this surface faithfully rendered "100% budget left". The label is
        // asserted against that specific string rather than only against
        // the absence of a `%`, because "100% budget left" is the exact
        // output a reverted fix produces and the one a reader of this test
        // needs to see named.
        let document = with_budget(Some(BudgetPosture::Uncommitted));
        let budget = segments(&document)
            .iter()
            .find(|s| s["key"] == json!("budget"))
            .unwrap()
            .clone();
        assert_eq!(budget["label"], json!("Budget usage unknown"));
        assert_ne!(budget["label"], json!("100% budget left"));
        assert_eq!(budget["state"], json!("unknown"));
        assert_eq!(
            budget["reason_code"],
            json!("no_commitment_against_envelope")
        );
        assert_eq!(budget["clear_role"], json!("vital"));

        let label = budget["label"].as_str().unwrap();
        assert!(!label.contains('%'), "there is no share to report: {label}");
        // The phrasings that would reintroduce the defect in words rather
        // than in arithmetic. Each of these reads as "none of it is gone",
        // which is the claim this posture exists to withhold.
        for forbidden in ["full", "untouched", "unspent", "intact", "all"] {
            assert!(
                !label.to_lowercase().contains(forbidden),
                "{label:?} implies a measured full envelope via {forbidden:?}"
            );
        }
    }

    #[test]
    fn the_uncommitted_explain_prose_says_why_the_share_is_withheld() {
        // The statusline has room only to name the unknown; `explain` is
        // where a user finds out that the omission is deliberate rather
        // than a missing reading.
        let prose = budget_posture_prose(Some(BudgetPosture::Uncommitted));
        assert!(prose.starts_with("unknown — "), "{prose}");
        assert!(prose.contains("nothing has been"), "{prose}");
        assert!(!prose.contains('%'), "{prose}");
        // Distinct from the neighbouring absences, so `explain` cannot be
        // read as saying the task has no envelope at all.
        assert_ne!(
            prose,
            budget_posture_prose(Some(BudgetPosture::NotEstablished))
        );
        assert_ne!(prose, budget_posture_prose(Some(BudgetPosture::Unreadable)));
    }

    #[test]
    fn the_four_ways_a_share_can_be_missing_stay_four_facts() {
        // Collapsing them loses the one a user could act on: a spent
        // envelope is not an unadmitted task, an envelope nothing has drawn
        // against is neither, and none of the three is a ledger that would
        // not read.
        let shapes = [
            BudgetPosture::Uncommitted,
            BudgetPosture::Exhausted,
            BudgetPosture::NotEstablished,
            BudgetPosture::Unreadable,
        ];
        let mut labels = Vec::new();
        let mut reasons = Vec::new();
        for shape in shapes {
            let document = with_budget(Some(shape));
            let budget = segments(&document)
                .iter()
                .find(|s| s["key"] == json!("budget"))
                .unwrap()
                .clone();
            assert!(
                !budget["label"].as_str().unwrap().contains('%'),
                "{shape:?} has no share to report"
            );
            labels.push(budget["label"].as_str().unwrap().to_string());
            reasons.push(budget["reason_code"].as_str().unwrap().to_string());
        }
        let mut unique_labels = labels.clone();
        unique_labels.sort();
        unique_labels.dedup();
        assert_eq!(unique_labels.len(), 4, "{labels:?}");
        let mut unique_reasons = reasons.clone();
        unique_reasons.sort();
        unique_reasons.dedup();
        assert_eq!(unique_reasons.len(), 4, "{reasons:?}");
    }

    #[test]
    fn an_absent_budget_field_yields_no_segment_rather_than_a_guess() {
        let document = with_budget(None);
        assert!(budget_label(&document).is_none());
        assert_eq!(segments(&document).len(), 2, "task and estimate only");
    }

    #[test]
    fn an_idle_daemon_reports_no_budget_at_all() {
        // A share of an envelope that was never admitted is not zero, it is
        // absent — and the idle document must not grow a reading for it even
        // if a peer sent one.
        let mut status = idle();
        set_budget(&mut status, Some(BudgetPosture::Exhausted));
        let document = reading(&status, None, now());
        assert!(budget_label(&document).is_none());
        assert_eq!(segments(&document).len(), 1);
    }

    #[test]
    fn a_budget_reading_needs_neither_the_doctor_half_nor_a_third_request() {
        // The whole probe is budgeted at 200 ms across two round trips, and a
        // budget figure that cost a third would not be worth a statusline.
        // The share therefore rides the `Status` reply: it is present with no
        // `Doctor` answer at all, which is the shape a failure-isolated or
        // timed-out second round trip produces.
        let document = reading(&status_with(ReplanState::Stable), None, now());
        assert_eq!(budget_label(&document).as_deref(), Some("38% budget left"));
    }

    // -------------------------------------------------------- escalation

    #[test]
    fn escalation_is_reported_without_claiming_anything_awaits_the_user() {
        // The second correction to the legacy rendering. Nothing is blocked
        // and nothing is waiting on an answer: the daemon's
        // `EscalateApprovalNeeded` arm logs, records the state and returns
        // `Ok(())`, and hooks are advisory-only. "Awaiting approval" invites
        // the reader to go and approve something that does not exist.
        let document = reading(
            &status_with(ReplanState::EscalatedAwaitingApproval),
            None,
            now(),
        );
        // On the task segment, not one of its own: the host allows four
        // segments per provider and the estimate, the budget and a
        // policy-drift warning claim the other three.
        let escalation = segments(&document)
            .iter()
            .find(|s| s["key"] == json!("task"))
            .expect("the state must be visible at all");

        assert_eq!(escalation["state"], json!("warn"));
        assert_eq!(escalation["label"], json!("Replans now need approval"));
        assert_eq!(
            escalation["reason_code"],
            json!("next_replan_needs_human_approval"),
            "the *next* replan, not this moment"
        );
        assert!(
            segments(&document)
                .iter()
                .all(|s| s["key"] != json!("escalation")),
            "a fifth segment would make the host refuse the whole document"
        );
        for (path, text) in all_strings(&document) {
            let lower = text.to_lowercase();
            assert!(!lower.contains("awaiting"), "{path} = {text:?}");
            assert!(!lower.contains("blocked"), "{path} = {text:?}");
        }
    }

    #[test]
    fn the_replan_state_is_one_fact_about_one_thing_in_all_three_shapes() {
        // Anti-vacuity for the fold: every variant has to be visible
        // *somewhere*, and all three have to be visible on the same segment,
        // or the fold has quietly lost one.
        let labels = |state| {
            segments(&reading(&status_with(state), None, now()))[0]["label"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert!(labels(ReplanState::Stable).starts_with("Task "));
        assert!(labels(ReplanState::Replanned { count: 3 }).starts_with("Task "));
        assert_eq!(
            labels(ReplanState::EscalatedAwaitingApproval),
            "Replans now need approval"
        );
    }

    #[test]
    fn escalation_is_never_rendered_as_an_emergency_or_as_an_affordance() {
        let document = reading(
            &status_with(ReplanState::EscalatedAwaitingApproval),
            None,
            now(),
        );
        for segment in segments(&document) {
            assert_ne!(
                segment["state"],
                json!("critical"),
                "the task is still running"
            );
            // A statusline is not an approval interface, so the document
            // must carry nothing a host could turn into a control.
            for field in ["action", "command", "approve_url", "href"] {
                assert!(segment.get(field).is_none(), "{field}");
            }
        }
    }

    #[test]
    fn an_ordinary_state_renders_three_quiet_segments() {
        // The line the founder sees almost always. Anything that warns
        // here is a warning that means nothing, and a statusline whose
        // warnings mean nothing is a statusline nobody reads.
        let document = reading(
            &status_with(ReplanState::Stable),
            Some(&doctor_with("balanced", true)),
            now(),
        );
        assert_eq!(segments(&document).len(), 3);
        for segment in segments(&document) {
            assert!(
                segment["state"] == json!("neutral"),
                "{:?} is not quiet",
                segment["key"]
            );
        }
    }

    // ----------------------------------------------------------- profile

    #[test]
    fn the_profile_segment_is_silent_while_disk_and_runtime_agree() {
        let document = reading(
            &status_with(ReplanState::Stable),
            Some(&doctor_with("balanced", true)),
            now(),
        );
        assert!(segments(&document)
            .iter()
            .all(|s| s["key"] != json!("profile")));
    }

    #[test]
    fn the_profile_segment_names_what_is_running_not_what_was_edited() {
        // The "never present the configured value as if it were live" rule.
        // A user who has edited `config.json` needs both facts: that the
        // edit has not taken effect, and what is in effect instead.
        let document = reading(
            &status_with(ReplanState::Stable),
            Some(&doctor_with("strict_budget", false)),
            now(),
        );
        let profile = segments(&document)
            .iter()
            .find(|s| s["key"] == json!("profile"))
            .expect("drift must be visible");
        assert_eq!(profile["state"], json!("warn"));
        assert_eq!(
            profile["label"],
            json!("Running policy strict budget"),
            "the token's underscore is a character the host's label allowlist refuses"
        );
        assert_eq!(
            profile["reason_code"],
            json!("config_edited_restart_required")
        );
    }

    #[test]
    fn a_preset_name_this_build_does_not_know_cannot_reach_the_payload() {
        // `Policy::name` is a `String` whose docs permit a caller-chosen
        // name for a custom policy. Today's only writer rejects anything
        // outside the four presets, so this is the difference between "no
        // path exists" and "no path can exist".
        let hostile = "balanced\u{1b}[31m sk-live-AbC123XyZ456 /Users/someone";
        let document = reading(
            &status_with(ReplanState::Stable),
            Some(&doctor_with(hostile, false)),
            now(),
        );
        let profile = segments(&document)
            .iter()
            .find(|s| s["key"] == json!("profile"))
            .expect("an unnameable preset costs the name, never the warning")
            .clone();
        assert_eq!(
            profile["label"],
            json!("Policy edit not in effect"),
            "the drift is the actionable half and needs no bounded value to state"
        );
        assert_eq!(
            profile["reason_code"],
            json!("config_edited_restart_required"),
            "the same fact, whether or not the preset can be named"
        );
        for (path, text) in all_strings(&document) {
            assert!(!text.contains("sk-live"), "{path} = {text:?}");
            assert!(!text.contains('\u{1b}'), "{path} = {text:?}");
        }

        for (token, display) in KNOWN_PRESETS {
            assert_eq!(preset_label(token), Some(token));
            assert_eq!(preset_display(token), Some(display));
            assert!(
                !display.contains('_'),
                "{display:?} would be refused by the host's label allowlist"
            );
        }
        assert_eq!(preset_label("balanced-ish"), None);
        assert_eq!(preset_display("balanced-ish"), None);
    }

    // --------------------------------------------------- clear projection

    /// Role of a segment by key, or `None` if the document has no such
    /// segment.
    fn role_of(document: &Value, key: &str) -> Option<String> {
        segments(document)
            .iter()
            .find(|s| s["key"] == json!(key))
            .map(|s| s["clear_role"].as_str().unwrap().to_string())
    }

    const CLEAR_ROLES: [&str; 4] = ["exception", "posture", "vital", "supporting"];

    #[test]
    fn every_document_declares_its_own_clear_projection() {
        for (name, document) in every_document() {
            assert_eq!(
                document["clear_authority"],
                json!("provider"),
                "{name}: an undeclared document is re-inferred by the host"
            );
            for segment in segments(&document) {
                let role = segment["clear_role"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{name}: {} has no clear_role", segment["key"]));
                assert!(CLEAR_ROLES.contains(&role), "{name}: unknown role {role:?}");
            }
        }
    }

    #[test]
    fn every_document_satisfies_the_hosts_declaration_rules() {
        // Mirrors `statusline_contract.ProviderStatus._validate_clear_authority`.
        // A declaring payload that breaks one of these is refused *whole*, so
        // Libra would render as nothing — the failure mode is silence, which
        // is why this is checked over every document rather than sampled.
        for (name, document) in every_document() {
            let segments = segments(&document);
            assert!(!segments.is_empty(), "{name}");
            let roles: Vec<&str> = segments
                .iter()
                .map(|s| s["clear_role"].as_str().unwrap())
                .collect();
            assert!(
                roles.iter().filter(|r| **r == "posture").count() <= 1,
                "{name}: more than one posture {roles:?}"
            );
            assert!(
                roles.iter().any(|r| *r == "posture" || *r == "exception"),
                "{name}: no posture and no exception {roles:?}"
            );
        }
    }

    #[test]
    fn an_active_task_leads_with_its_schedule_and_its_budget() {
        // The ticket's normal case: schedule expectation plus budget posture,
        // and the task id explicitly *not* competing with either.
        let document = reading(
            &status_with(ReplanState::Stable),
            Some(&doctor_with("balanced", true)),
            now(),
        );
        assert_eq!(role_of(&document, "estimate").as_deref(), Some("posture"));
        assert_eq!(role_of(&document, "budget").as_deref(), Some("vital"));
        assert_eq!(
            role_of(&document, "task").as_deref(),
            Some("supporting"),
            "an opaque id is a handle on the work, not a reason to look"
        );
    }

    #[test]
    fn an_action_required_state_outranks_the_routine_metrics() {
        // Approval and exhaustion are the two exceptions, and each must be
        // the *only* exception in its own shape so the host's ladder has an
        // unambiguous primary.
        let escalated = reading(
            &status_with(ReplanState::EscalatedAwaitingApproval),
            None,
            now(),
        );
        assert_eq!(role_of(&escalated, "task").as_deref(), Some("exception"));
        assert_eq!(
            role_of(&escalated, "budget").as_deref(),
            Some("vital"),
            "the host pairs an exception only with a *declared* vital, which \
             is how `Replans now need approval . 38% budget left` gets to say \
             what the decision costs"
        );

        let mut exhausted = status_with(ReplanState::Stable);
        set_budget(&mut exhausted, Some(BudgetPosture::Exhausted));
        let exhausted = reading(&exhausted, None, now());
        assert_eq!(role_of(&exhausted, "budget").as_deref(), Some("exception"));
        assert_eq!(role_of(&exhausted, "task").as_deref(), Some("supporting"));
    }

    #[test]
    fn an_idle_daemon_shows_no_schedule_expectation() {
        // "A P90 with nothing being governed is a number about nothing." The
        // guarantee is structural rather than a formatting rule: with no task
        // there is no estimate segment for a span to live on.
        let document = reading(&idle(), Some(&doctor_with("balanced", true)), now());
        assert_eq!(role_of(&document, "task").as_deref(), Some("posture"));
        for segment in segments(&document) {
            assert!(
                segment.get("duration_seconds").is_none()
                    && segment.get("duration_label").is_none(),
                "{segment:?}"
            );
        }
    }

    #[test]
    fn a_task_with_no_estimate_says_so_in_the_field_clear_shows() {
        // Clear keeps the state marker, the label and the duration, and drops
        // the reason. A no-estimate segment labelled `Remaining work` would
        // render as a heading with nothing under it.
        let mut status = status_with(ReplanState::Stable);
        status.current_task.as_mut().unwrap().remaining_estimate = estimate_with(None, true, 0);
        let document = reading(&status, None, now());
        let estimate = &segments(&document)[1];
        assert_eq!(estimate["label"], json!("Remaining work not estimated"));
        assert_eq!(estimate["clear_role"], json!("posture"));

        let estimated = reading(&status_with(ReplanState::Stable), None, now());
        assert_ne!(
            segments(&estimated)[1]["label"],
            estimate["label"],
            "the two shapes must not read identically once the reason is gone"
        );
    }

    #[test]
    fn a_preflight_confidence_can_never_become_the_primary_clear_signal() {
        // Confidence stays Detail information. The host drops it at Clear
        // depth, and this pins the other half: it is never attached to a
        // segment the ladder could promote on its own declaration, so it
        // cannot ride into Clear beside an exception either.
        for (name, document) in every_document() {
            for segment in segments(&document) {
                if segment.get("confidence").is_none() {
                    continue;
                }
                assert_eq!(
                    segment["key"],
                    json!("estimate"),
                    "{name}: a confidence must qualify the estimate it is about"
                );
                assert_eq!(
                    segment["clear_role"],
                    json!("posture"),
                    "{name}: a confidence on an exception or a vital would be \
                     promoted as if it were the reading"
                );
            }
        }
    }

    #[test]
    fn the_clear_declaration_is_not_vacuous() {
        // Guarding the guard: the properties above would all pass over a
        // document set that never exercised more than one role, so prove the
        // set spans every role and every budget shape.
        let mut roles: Vec<String> = Vec::new();
        let mut budget_labels: Vec<String> = Vec::new();
        for (_, document) in every_document() {
            for segment in segments(&document) {
                roles.push(segment["clear_role"].as_str().unwrap().to_string());
            }
            if let Some(label) = budget_label(&document) {
                budget_labels.push(label);
            }
        }
        for role in CLEAR_ROLES {
            assert!(
                roles.iter().any(|r| r == role),
                "no document exercises the {role:?} role"
            );
        }
        budget_labels.sort();
        budget_labels.dedup();
        assert!(
            budget_labels.len() >= 4,
            "the budget shapes are not all covered: {budget_labels:?}"
        );
    }

    // ------------------------------------------------------- the document

    #[test]
    fn a_doctor_that_did_not_answer_does_not_cost_the_user_the_task() {
        // Failure isolation inside one provider: the diagnostic half is
        // discarded to `None`, and what could be established still renders.
        let document = reading(&status_with(ReplanState::Stable), None, now());
        assert_eq!(document["availability"], json!("available"));
        let keys: Vec<&str> = segments(&document)
            .iter()
            .map(|s| s["key"].as_str().unwrap())
            .collect();
        assert_eq!(
            keys,
            ["task", "estimate", "budget"],
            "only the diagnostic half is lost; everything the `Status` reply \
             carried still renders"
        );
    }

    #[test]
    fn the_worst_case_document_fits_the_hosts_per_provider_segment_budget() {
        // Four is the host's `MAX_SEGMENTS_PER_PROVIDER`, and a document
        // that exceeds it is refused whole — so Libra would render as
        // nothing at precisely the moment it had the most to say. The
        // allowance is now fully spent: task, estimate, budget, profile.
        // This is why the escalation folded onto the task segment rather
        // than claiming a fifth.
        let document = reading(
            &status_with(ReplanState::EscalatedAwaitingApproval),
            Some(&doctor_with("strict_budget", false)),
            now(),
        );
        assert_eq!(segments(&document).len(), 4);
        let keys: Vec<&str> = segments(&document)
            .iter()
            .map(|s| s["key"].as_str().unwrap())
            .collect();
        assert_eq!(keys, ["task", "estimate", "budget", "profile"]);
        for (name, document) in every_document() {
            assert!(segments(&document).len() <= 4, "{name}");
        }
    }

    #[test]
    fn segment_keys_are_unique_and_ordered_within_the_provider() {
        for (name, document) in every_document() {
            let mut keys: Vec<&str> = segments(&document)
                .iter()
                .map(|s| s["key"].as_str().unwrap())
                .collect();
            let count = keys.len();
            keys.sort_unstable();
            keys.dedup();
            assert_eq!(keys.len(), count, "{name}: duplicate segment key");
        }
    }

    #[test]
    fn a_live_reading_claims_no_age_and_authorises_no_cache() {
        // The daemon answered just now, so an age would invent a staleness
        // that does not exist — and the state can change on any tool call,
        // so a TTL would let a host render a superseded plan as current.
        let document = reading(&status_with(ReplanState::Stable), None, now());
        assert_eq!(document["observed_at"], json!("2026-02-02T02:40:00Z"));
        assert!(document.get("age_seconds").is_none());
        assert!(document.get("cache_ttl_seconds").is_none());
        for segment in segments(&document) {
            assert!(segment.get("age_seconds").is_none());
        }
    }

    #[test]
    fn observed_at_is_utc_with_a_literal_z() {
        // `time`'s own Rfc3339 renders UTC as `+00:00`, which the contract
        // refuses: an age computed against a local offset is ambiguous
        // across machines.
        let stamp = rfc3339_utc(now());
        assert!(stamp.ends_with('Z'), "{stamp}");
        assert!(!stamp.contains('+'), "{stamp}");
        assert_eq!(stamp.len(), 20, "{stamp}");
    }

    // ----------------------------------------------------------- privacy

    const SECRET_PREFIXES: [&str; 8] = [
        "sk-",
        "sk_live_",
        "sk_test_",
        "ghp_",
        "gho_",
        "github_pat_",
        "xoxb-",
        "AKIA",
    ];

    /// A run of 20+ token characters mixing cases and digits — the shape a
    /// key has and prose does not.
    fn looks_high_entropy(text: &str) -> bool {
        text.split(|c: char| !(c.is_ascii_alphanumeric() || "_+/=-".contains(c)))
            .any(|run| {
                run.len() >= 20
                    && run.chars().any(|c| c.is_ascii_lowercase())
                    && run.chars().any(|c| c.is_ascii_uppercase())
                    && run.chars().any(|c| c.is_ascii_digit())
            })
    }

    #[test]
    fn no_string_in_any_document_is_secret_shaped() {
        // The permanent guard the contract requires. It runs over every
        // document the provider can produce, not a chosen one, so a new
        // field cannot be added without passing through here.
        for (name, document) in every_document() {
            for (path, text) in all_strings(&document) {
                for prefix in SECRET_PREFIXES {
                    assert!(
                        !text.contains(prefix),
                        "{name}: {path} carries a {prefix}-shaped value"
                    );
                }
                assert!(
                    !looks_high_entropy(&text),
                    "{name}: {path} = {text:?} has the shape of a key"
                );
            }
        }
    }

    #[test]
    fn the_high_entropy_check_is_not_vacuous() {
        // Guarding the guard: a check that never fires proves nothing about
        // the documents it passed.
        assert!(looks_high_entropy("sk-live-AbCd1234EfGh5678IjKl"));
        assert!(!looks_high_entropy("Replans now need approval"));
        assert!(
            !looks_high_entropy("Task 3f2a1b9c"),
            "a short lowercase-hex id must stay renderable"
        );
    }

    #[test]
    fn no_document_carries_a_filesystem_path_or_an_escape_sequence() {
        // Paths are the contract's "avoid unless there is an explicit safe
        // UX requirement"; there is none here. Escape sequences are worse
        // than a leak — a statusline is written straight to a terminal.
        for (name, document) in every_document() {
            for (path, text) in all_strings(&document) {
                assert!(!text.contains('/'), "{name}: {path} = {text:?}");
                assert!(!text.contains('\\'), "{name}: {path} = {text:?}");
                assert!(!text.contains('\u{1b}'), "{name}: {path}");
                assert!(!text.contains('\n'), "{name}: {path} = {text:?}");
            }
        }
    }

    #[test]
    fn human_labels_use_only_the_characters_the_contract_allows() {
        // The host refuses a document whose label falls outside its ASCII
        // allowlist, and a refused document renders as nothing.
        const HUMAN_FIELDS: [&str; 4] = ["label", "count_label", "duration_label", "reason_label"];
        for (name, document) in every_document() {
            for segment in segments(&document) {
                for field in HUMAN_FIELDS {
                    let Some(text) = segment.get(field).and_then(Value::as_str) else {
                        continue;
                    };
                    assert!(
                        text.chars()
                            .all(|c| c.is_ascii_alphanumeric() || " .,'-—()%+?!≤≥".contains(c)),
                        "{name}: {field} = {text:?}"
                    );
                    assert!(text.len() <= 48, "{name}: {field} = {text:?} is too long");
                }
            }
        }
    }

    #[test]
    fn the_estimators_free_text_never_reaches_a_rendering_surface() {
        // `Estimate::reason` is free text by type. The fixture puts a path
        // and a key-shaped token in it precisely so a leak here is loud.
        let status = status_with(ReplanState::Stable);
        let document = reading(&status, Some(&doctor_with("balanced", true)), now());
        assert!(
            status
                .current_task
                .as_ref()
                .unwrap()
                .remaining_estimate
                .reason
                .is_some(),
            "premise: the state being rendered from does carry free text"
        );
        for (path, text) in all_strings(&document) {
            assert!(!text.contains("secret-project"), "{path}");
        }
        assert!(
            !explain_text(&status, Some(&doctor_with("balanced", true))).contains("secret-project")
        );
    }

    // ----------------------------------------------------------- explain

    #[test]
    fn explain_answers_the_question_the_statusline_could_not() {
        let status = status_with(ReplanState::Stable);
        let text = explain_text(&status, Some(&doctor_with("balanced", true)));
        let task = status.current_task.as_ref().unwrap();
        assert!(text.contains(&task.task_id.to_string()), "the full task id");
        assert!(
            text.contains(&task.plan_id.0.to_string()),
            "the full plan id"
        );
        assert!(text.contains("remaining P90    600s"));
        assert!(text.contains("running policy   balanced"));
        assert!(
            text.contains("basis: this repo"),
            "the same words the preflight and receipt renderers use"
        );
    }

    #[test]
    fn explain_says_a_confidence_is_about_the_estimate_not_about_the_task() {
        let text = explain_text(&status_with(ReplanState::Stable), None);
        assert!(text.contains("confidence in the remaining-work estimate"));
        assert!(text.contains("not a risk or priority rating for the task"));
    }

    #[test]
    fn explain_states_plainly_that_escalation_is_not_waiting_on_anyone() {
        let text = explain_text(&status_with(ReplanState::EscalatedAwaitingApproval), None);
        assert!(text.contains("automatic-replan budget"));
        assert!(text.contains("Nothing"));
        assert!(text.contains("waiting on an answer from you"));
        assert!(
            !text.contains("approve this"),
            "explain is not an approval interface either"
        );
    }

    #[test]
    fn explain_reports_the_budget_share_with_its_axis_too() {
        // The privacy rule bars amounts, not ratios, and `explain` is where
        // a ratio is allowed to be a sentence rather than a segment. The
        // axis travels with it: `explain` is read out of context of the
        // line that produced it.
        let text = explain_text(&status_with(ReplanState::Stable), None);
        assert!(text.contains("38% budget left"), "{text}");
        for rendered in ["1234", "2345", "3456"] {
            assert!(
                !text.contains(rendered),
                "{rendered} is an amount, not a share"
            );
        }
    }

    #[test]
    fn explain_distinguishes_a_spent_budget_from_one_that_never_existed() {
        // The statusline has room for a label; `explain` has room for the
        // difference, which is the whole reason the posture is four variants.
        let mut spent = status_with(ReplanState::Stable);
        set_budget(&mut spent, Some(BudgetPosture::Exhausted));
        let spent = explain_text(&spent, None);

        let mut never = status_with(ReplanState::Stable);
        set_budget(&mut never, Some(BudgetPosture::NotEstablished));
        let never = explain_text(&never, None);

        let mut unreadable = status_with(ReplanState::Stable);
        set_budget(&mut unreadable, Some(BudgetPosture::Unreadable));
        let unreadable = explain_text(&unreadable, None);

        let mut lines: Vec<String> = Vec::new();
        for text in [&spent, &never, &unreadable] {
            let line = text
                .lines()
                .find(|line| line.trim_start().starts_with("budget"))
                .expect("explain always has a budget line")
                .to_string();
            assert!(!line.contains('%'), "{line}: there is no share to report");
            lines.push(line);
        }
        let mut unique = lines.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 3, "{lines:?}");

        // The one deliberate collapse: an absent field reads as an absent
        // envelope, because with a task present the daemon always answers
        // and `None` only ever means "nothing was governed".
        let mut absent = status_with(ReplanState::Stable);
        set_budget(&mut absent, None);
        let absent = explain_text(&absent, None);
        assert!(absent.contains("admitted without a resource envelope"));
    }

    #[test]
    fn explain_never_prints_a_resource_quantile() {
        // Cost history is the founder's, and the contract's privacy rule
        // names it. The fixture sets all three quantiles so their absence
        // here is a real observation.
        let text = explain_text(&status_with(ReplanState::Stable), None);
        for rendered in ["1234", "2345", "3456"] {
            assert!(
                !text.contains(rendered),
                "{rendered} is a resource quantile"
            );
        }
    }

    #[test]
    fn explain_reports_an_unestablished_diagnostic_rather_than_guessing() {
        let text = explain_text(&status_with(ReplanState::Stable), None);
        assert!(text.contains("not established"));
        assert!(
            !text.contains("agrees with the running policy"),
            "silence about drift must not read as agreement"
        );
    }

    #[test]
    fn explain_reports_config_drift_and_which_side_is_in_force() {
        let text = explain_text(
            &status_with(ReplanState::Stable),
            Some(&doctor_with("strict_budget", false)),
        );
        assert!(text.contains("running policy   strict_budget"));
        assert!(text.contains("has been edited since the daemon started"));
        assert!(text.contains("daemon restart is what would apply the edit"));
    }

    #[test]
    fn explain_declines_to_name_a_preset_it_cannot_describe() {
        let text = explain_text(
            &status_with(ReplanState::Stable),
            Some(&doctor_with("custom-thing", true)),
        );
        assert!(text.contains("a preset this build does not recognise"));
        assert!(!text.contains("custom-thing"));
    }

    #[test]
    fn explain_reports_no_task_without_implying_a_verdict() {
        let text = explain_text(&idle(), None);
        assert!(text.contains("No task is being governed right now"));
        assert!(text.contains("not a judgement"));
    }
}
