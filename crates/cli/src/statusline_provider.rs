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
//! a credential, or a path. Every string it emits is a fixed literal in this
//! file.

use std::time::{Duration, Instant};

use libra_governor_protocol::{ReplanState, Request, Response, StatusResult, TaskSummary};
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
pub fn reading(status: &StatusResult, now: OffsetDateTime) -> Value {
    let segments = vec![task_segment(status.current_task.as_ref())];

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
fn probe(deadline: Instant) -> Result<StatusResult, NoReading> {
    match ask(Request::Status, deadline)? {
        Response::Status(status) => Ok(*status),
        _ => Err(NoReading::ResponseNotUnderstood),
    }
}

/// `libra-governor statusline provider`.
///
/// Exits 0 in every case, including every failure. A non-zero exit would
/// make a stopped daemon indistinguishable from a broken provider, and the
/// document already says which it is — with a bounded reason code the host
/// can render and `doctor` can act on.
pub fn run_provider() {
    let payload = match probe(Instant::now() + HOT_PATH_BUDGET) {
        Ok(status) => reading(&status, OffsetDateTime::now_utc()),
        Err(kind) => no_reading(kind),
    };
    println!("{payload}");
}
