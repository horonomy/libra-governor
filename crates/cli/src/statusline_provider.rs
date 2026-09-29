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
//!    So this provider reports the budget being spent and says plainly, in
//!    the reason clause, that the *next* replan is what would need a human.
//!
//! # Why the profile costs a second round trip
//!
//! The provider answers a `Status` request and then, separately, a
//! `Doctor` one, because the running policy preset and whether
//! `config.json` still agrees with it live on [`DoctorResult`] and nowhere
//! else. The alternative considered and rejected was widening
//! [`StatusResult`] to carry them: this repository's convention is to bump
//! `PROTOCOL_VERSION` for any response-shape change (see its docs — eight
//! bumps, one per shape change), and a bump forces every long-lived daemon
//! to be restarted before it will answer again. A second local round trip
//! inside the same deadline is the cheaper price, and it keeps one
//! implementation of "does the running config match disk" rather than two.
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
    Confidence, DoctorResult, ReplanState, Request, Response, StatusResult, TaskSummary,
};
use serde_json::{json, Value};
use time::OffsetDateTime;

use crate::client::{self, ClientError};

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
pub fn no_reading(kind: NoReading) -> Value {
    let availability = kind.availability();
    json!({
        "contract_version": CONTRACT_VERSION,
        "provider": PROVIDER_ID,
        "provider_version": env!("CARGO_PKG_VERSION"),
        "scope": SCOPE,
        "availability": availability,
        "order_hint": ORDER_HINT,
        "segments": [{
            "key": "availability",
            "state": state_for(availability),
            "label": kind.label(),
            "reason_code": kind.reason_code(),
            "explain_key": "libra.availability",
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
/// The replan count rides here as a labelled `count` rather than as its own
/// segment, because "this task, replanned twice" is one fact about one
/// thing. It is omitted when zero: `0 replans` spends columns to say
/// nothing, and the legacy line's `stable` said the same thing at more
/// length.
///
/// The plan id the legacy line also carried is deliberately dropped. Two
/// opaque eight-character identifiers on a line shared with the user's own
/// statusline and every other product is what progressive disclosure is
/// for; the task id stays because it is the one handle a reader has on the
/// work, and both appear in full on the `explain` surface.
fn task_segment(task: Option<&TaskSummary>) -> Value {
    let Some(task) = task else {
        return json!({
            "key": "task",
            "state": "neutral",
            "label": "No task being governed",
            "explain_key": "libra.task",
            "order_hint": 10,
        });
    };
    let mut segment = json!({
        "key": "task",
        "state": "neutral",
        "label": format!("Task {}", crate::statusline::short_task_id(&task.task_id.to_string())),
        "explain_key": "libra.task",
        "order_hint": 10,
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
fn estimate_segment(task: &TaskSummary) -> Value {
    let estimate = &task.remaining_estimate;
    let Some(p90) = estimate.duration_p90_secs else {
        return json!({
            "key": "estimate",
            "state": "unknown",
            "label": "Remaining work",
            "reason_code": if estimate.cold_start {
                "no_local_history_yet"
            } else {
                "estimate_has_no_duration_bound"
            },
            "explain_key": "libra.estimate",
            "order_hint": 20,
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

/// The automatic-replan budget having been spent, when it has been.
///
/// `warn`, not `critical`, and the label says what is true rather than what
/// the variant is named. Nothing is blocked: `server.rs`'s
/// `EscalateApprovalNeeded` arm logs, records the state and returns
/// `Ok(())`, and hooks are advisory-only, so the task keeps running. What
/// has changed is that the plan on screen will no longer be silently
/// corrected, which is worth acting on but is not an emergency and is
/// certainly not a request the user can answer from here.
///
/// Returns `None` for the other two states. `Stable` has nothing to report
/// and `Replanned` is already reported as the task segment's count, so a
/// segment for either would be a column spent on "normal".
fn escalation_segment(task: &TaskSummary) -> Option<Value> {
    match task.replan_state {
        ReplanState::Stable | ReplanState::Replanned { .. } => None,
        ReplanState::EscalatedAwaitingApproval => Some(json!({
            "key": "escalation",
            "state": "warn",
            "label": "Replan budget spent",
            "reason_code": "next_replan_needs_human_approval",
            "explain_key": "libra.escalation",
            "order_hint": 30,
        })),
    }
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
/// Returns `None` when the two agree, which is the ordinary case. The
/// alternative — always showing `Running policy balanced` — spends a column
/// on a constant, and `explain` reports the preset unconditionally for the
/// user who wants it. `None` also covers a preset this build cannot name;
/// see [`preset_display`].
fn profile_segment(doctor: &DoctorResult) -> Option<Value> {
    if doctor.running_config_matches_disk {
        return None;
    }
    let preset = preset_display(&doctor.policy_preset)?;
    Some(json!({
        "key": "profile",
        "state": "warn",
        "label": format!("Running policy {preset}"),
        "reason_code": "config_edited_restart_required",
        "explain_key": "libra.profile",
        "order_hint": 40,
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
pub fn reading(status: &StatusResult, doctor: Option<&DoctorResult>, now: OffsetDateTime) -> Value {
    let mut segments = vec![task_segment(status.current_task.as_ref())];
    if let Some(task) = status.current_task.as_ref() {
        segments.push(estimate_segment(task));
        segments.extend(escalation_segment(task));
    }
    segments.extend(doctor.and_then(profile_segment));

    json!({
        "contract_version": CONTRACT_VERSION,
        "provider": PROVIDER_ID,
        "provider_version": env!("CARGO_PKG_VERSION"),
        "scope": SCOPE,
        "availability": "available",
        "order_hint": ORDER_HINT,
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
        Ok((status, doctor)) => reading(&status, doctor.as_ref(), OffsetDateTime::now_utc()),
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
/// free-text `reason`, the resource/cost quantiles, a credential, or a
/// path. The ids it does print are local, ephemeral and the only handle a
/// reader has on the work — unlike Fornax's claim and session ids, which
/// answer no question this surface is asked.
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
            if task.replan_state == ReplanState::EscalatedAwaitingApproval {
                out.push_str(
                    "\n  This task has used up its automatic-replan budget, so the next\n",
                );
                out.push_str(
                    "  material deviation will not be silently replanned again. Nothing\n",
                );
                out.push_str(
                    "  is blocked and nothing is waiting on an answer from you — Libra's\n",
                );
                out.push_str("  hooks are advisory, and the task is still running. What has\n");
                out.push_str("  changed is that the plan and estimate above will no longer be\n");
                out.push_str("  corrected automatically, so they are worth re-reading yourself.\n");
            }
        }
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
    out.push_str("  free-text reasoning, and resource/cost quantiles. Libra keeps that\n");
    out.push_str("  material local and off every rendering surface.\n");
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
    use libra_governor_domain::{BucketTier, Estimate, PlanId, ResourceAmount, TaskId};

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
                    let mut status = status_with(state);
                    status.current_task.as_mut().unwrap().remaining_estimate =
                        estimate_with(p90, p90.is_none(), 7);
                    out.push((
                        format!("reading/{state_name}/{doctor_name}/{estimate_name}"),
                        reading(&status, doctor.as_ref(), now()),
                    ));
                }
            }
        }
        out.push((
            "reading/idle".to_string(),
            reading(
                &StatusResult { current_task: None },
                Some(&doctor_with("balanced", true)),
                now(),
            ),
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
        let document = reading(&StatusResult { current_task: None }, None, now());
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
        let escalation = segments(&document)
            .iter()
            .find(|s| s["key"] == json!("escalation"))
            .expect("the state must be visible at all");

        assert_eq!(escalation["state"], json!("warn"));
        assert_eq!(escalation["label"], json!("Replan budget spent"));
        assert_eq!(
            escalation["reason_code"],
            json!("next_replan_needs_human_approval"),
            "the *next* replan, not this moment"
        );
        for (path, text) in all_strings(&document) {
            let lower = text.to_lowercase();
            assert!(!lower.contains("awaiting"), "{path} = {text:?}");
            assert!(!lower.contains("blocked"), "{path} = {text:?}");
        }
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
    fn an_ordinary_state_renders_two_quiet_segments() {
        // The line the founder sees almost always. Anything that warns
        // here is a warning that means nothing, and a statusline whose
        // warnings mean nothing is a statusline nobody reads.
        let document = reading(
            &status_with(ReplanState::Stable),
            Some(&doctor_with("balanced", true)),
            now(),
        );
        assert_eq!(segments(&document).len(), 2);
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
        assert!(
            segments(&document)
                .iter()
                .all(|s| s["key"] != json!("profile")),
            "an unnameable policy is suppressed, not rendered as a placeholder"
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

    // ------------------------------------------------------- the document

    #[test]
    fn a_doctor_that_did_not_answer_does_not_cost_the_user_the_task() {
        // Failure isolation inside one provider: the diagnostic half is
        // discarded to `None`, and what could be established still renders.
        let document = reading(&status_with(ReplanState::Stable), None, now());
        assert_eq!(document["availability"], json!("available"));
        assert_eq!(segments(&document).len(), 2);
        assert_eq!(segments(&document)[0]["key"], json!("task"));
    }

    #[test]
    fn the_worst_case_document_fits_the_hosts_per_provider_segment_budget() {
        // Four is the host's `MAX_SEGMENTS_PER_PROVIDER`, and a document
        // that exceeds it is refused whole — so Libra would render as
        // nothing at precisely the moment it had the most to say.
        let document = reading(
            &status_with(ReplanState::EscalatedAwaitingApproval),
            Some(&doctor_with("strict_budget", false)),
            now(),
        );
        assert_eq!(segments(&document).len(), 4);
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
        assert!(!looks_high_entropy("Replan budget spent"));
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
        let text = explain_text(&StatusResult { current_task: None }, None);
        assert!(text.contains("No task is being governed right now"));
        assert!(text.contains("not a judgement"));
    }
}
