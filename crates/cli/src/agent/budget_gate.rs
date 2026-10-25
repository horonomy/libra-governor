//! Hook-side runtime of the local monthly shared budget (HORO-2215,
//! ADR-0026 D1/D3).
//!
//! This module holds NO policy. Every allow/deny decision is the daemon's
//! (`SharedBudget::Admit` / `Check`); the hook only (a) asks the daemon,
//! (b) renders the answer in Claude Code's blocking vocabulary, and (c)
//! applies the ADR's safe-failure rows for the one situation the daemon
//! cannot answer for itself -- it is unreachable -- where the only fact the
//! hook consults is whether `<state_dir>/shared-budget.json` exists.
//!
//! Guarantee label (must not be strengthened): amounts are
//! Libra-accounted USD, admission-bounded (cooperative), never provider
//! quota. Texts built here contain no prompt content.
//!
//! | Hook | Budget file | Daemon answer | Result |
//! |---|---|---|---|
//! | UserPromptSubmit | absent | (not asked) | unchanged output |
//! | UserPromptSubmit | present | `not_configured` | unchanged output |
//! | UserPromptSubmit | present | `allowed` | unchanged output (+ one warning line) |
//! | UserPromptSubmit | present | denied / refused / unusable / unreachable | `{"decision":"block",..}` |
//! | PreToolUse | absent | (not asked, no connect) | empty output |
//! | PreToolUse | present | exhausted / denied | deny JSON |
//! | PreToolUse | present | anything else, incl. timeout | empty output |

use std::path::{Path, PathBuf};
use std::time::Duration;

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use libra_governor_domain::{format_local_rfc3339, IanaTimeZone};
use libra_governor_protocol::{
    Request, Response, SharedBudgetCheckState, SharedBudgetDenyReason, SharedBudgetOutcome,
    SharedBudgetRequest, SoftLimitScope, SoftLimitWarning,
};

use crate::client::{self, ClientError};

/// File name under the daemon state directory (the daemon owns the format).
const BUDGET_FILE_NAME: &str = libra_governor_daemon::shared_budget_file::SHARED_BUDGET_FILE_NAME;

/// Hard wall-clock budget for the PreToolUse check round trip. A tool call
/// must never stall behind the budget.
pub const CHECK_BUDGET: Duration = Duration::from_millis(500);

/// What a denial must always say about its own units.
const LABEL: &str = "Libra-accounted USD, admission-bounded (cooperative), not provider quota";

/// The prompt hook's decision.
#[derive(Debug, PartialEq, Eq)]
pub enum Admission {
    /// Continue to Preflight exactly as before; `warning` is one short line
    /// to append to `additionalContext`.
    Proceed { warning: Option<String> },
    /// Emit the blocking output with this reason; do not call Preflight.
    Block { reason: String },
}

/// `<state_dir>/shared-budget.json`, resolved like the daemon client's
/// socket path. `None` only when the state directory cannot be resolved.
fn budget_file_path() -> Option<PathBuf> {
    libra_governor_daemon::paths::state_dir()
        .ok()
        .map(|dir| dir.join(BUDGET_FILE_NAME))
}

/// Can this process search (traverse) `dir`? Without that, nothing beneath
/// it can be stat'ed, and the daemon cannot run there either.
fn dir_searchable(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    match std::ffi::CString::new(dir.as_os_str().as_bytes()) {
        // SAFETY: `c` is a valid NUL-terminated path for the call's duration.
        Ok(c) => unsafe { libc::access(c.as_ptr(), libc::X_OK) == 0 },
        Err(_) => false,
    }
}

/// Cheap stat: does a budget file exist? Absent means the file is not there
/// or cannot exist/be reached through its parent (state dir missing, not a
/// directory, or not searchable): that is not a budget decision, so the hook
/// fails open as it always did. Only an error on the file entry itself, in
/// a searchable state dir, counts as present so the daemon decides.
pub fn budget_file_present() -> bool {
    let Some(path) = budget_file_path() else {
        return false;
    };
    match std::fs::symlink_metadata(&path) {
        Ok(_) => true,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::ENOTDIR) {
                return false;
            }
            path.parent().map(dir_searchable).unwrap_or(false)
        }
    }
}

/// Session ids the daemon would refuse purely because of what the host
/// sent. These never block (fail open), and are not sent as `Admit`.
pub fn session_id_unusable(session_id: &str) -> bool {
    // Mirrors the daemon's reserved execution-owner scope prefix.
    session_id.trim().is_empty()
        || session_id.starts_with("execution-owner-v1:")
        || libra_governor_daemon::legacy_executor::is_reserved_session(session_id)
}

fn usd(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.unsigned_abs();
    format!("{sign}${}.{:02}", abs / 100, abs % 100)
}

fn usd_u(cents: u64) -> String {
    format!("${}.{:02}", cents / 100, cents % 100)
}

/// Single-line, bounded rendering of daemon-supplied free text.
fn one_line(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.trim();
    if flat.chars().count() > 160 {
        let cut: String = flat.chars().take(160).collect();
        format!("{cut}...")
    } else {
        flat.to_string()
    }
}

/// What a denial message needs from `shared-budget.json` beyond the daemon's
/// answer: where the file is (for the remedy), the configured zone (the
/// period end is shown in it) and the per-turn hold (a turn needs that much
/// left, which is why a positive remainder can still be "exhausted").
///
/// Read from the file at message time, only on the denial path, so the
/// allow path and the wire protocol are untouched. When the file cannot be
/// read, every field is `None` and the message degrades to the daemon's
/// own facts (period end in UTC, no reservation figure).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BudgetContext {
    pub file_path: Option<PathBuf>,
    pub time_zone: Option<IanaTimeZone>,
    pub turn_hold_usd_cents: Option<u64>,
}

impl BudgetContext {
    pub fn load() -> Self {
        use libra_governor_daemon::shared_budget_file::LoadedSharedBudget;
        let Ok(dir) = libra_governor_daemon::paths::state_dir() else {
            return Self::default();
        };
        let loaded = LoadedSharedBudget::load(&dir);
        Self {
            time_zone: loaded.config().map(|c| c.time_zone.clone()),
            turn_hold_usd_cents: loaded.config().map(|c| c.turn_hold_usd_cents),
            file_path: loaded.path,
        }
    }

    /// `end` (an RFC 3339 instant from the daemon) in the configured zone,
    /// with the zone name; unchanged when there is no zone or `end` does not
    /// parse.
    fn period_end_text(&self, end: &str) -> String {
        let local = self.time_zone.as_ref().and_then(|tz| {
            let instant = OffsetDateTime::parse(end, &Rfc3339).ok()?;
            Some(format!(
                "{} ({})",
                format_local_rfc3339(instant, tz),
                tz.as_str()
            ))
        });
        local.unwrap_or_else(|| end.to_string())
    }
}

fn denied_reason(
    reason: SharedBudgetDenyReason,
    remaining: Option<i64>,
    period_end: Option<&str>,
    detail: Option<&str>,
    ctx: &BudgetContext,
) -> String {
    match reason {
        SharedBudgetDenyReason::CapExhausted => {
            let left = match (remaining, ctx.turn_hold_usd_cents) {
                (Some(r), Some(hold)) if r >= 0 && (r as u64) < hold => {
                    format!(
                        "{} remaining is less than the {} a turn reserves",
                        usd(r),
                        usd_u(hold)
                    )
                }
                (Some(r), _) if r >= 0 => format!("{} remaining", usd(r)),
                (Some(r), _) => format!("over the cap by {}", usd(-r)),
                (None, _) => "no budget remaining".to_string(),
            };
            let end = period_end
                .map(|e| format!("; the period ends {}", ctx.period_end_text(e)))
                .unwrap_or_default();
            let file = ctx
                .file_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "shared-budget.json".to_string());
            // The remainder is net of other sessions' live holds and the
            // denial does not say how much is held, so waiting for the
            // period to end is not claimed as the way out.
            format!(
                "Libra shared monthly budget exhausted: {left}{end}. To continue, raise \
                 monthly_cap_usd_cents in {file} (applies to the next prompt) or finish or stop \
                 other active sessions whose in-flight turns hold part of the budget ({LABEL})."
            )
        }
        SharedBudgetDenyReason::ConfigInvalid => {
            let why = detail
                .map(|d| format!(" ({})", one_line(d)))
                .unwrap_or_default();
            format!(
                "Libra shared budget file is invalid{why}; fix or remove shared-budget.json \
                 to continue ({LABEL})."
            )
        }
        SharedBudgetDenyReason::ClockAnomaly => format!(
            "Libra shared budget paused: the system clock is earlier than a recorded \
             budget period (clock anomaly); correct the clock to continue ({LABEL})."
        ),
    }
}

fn unreachable_reason() -> String {
    format!(
        "Libra shared budget: budget enforcement unavailable: daemon unreachable, so this \
         prompt cannot be admitted while shared-budget.json exists. Start the Libra daemon \
         (libra-governor daemon run), or remove/fix shared-budget.json, or unset \
         LIBRA_GOVERNOR_NO_AUTOSPAWN ({LABEL})."
    )
}

fn unusable_reason() -> String {
    format!(
        "Libra shared budget: budget enforcement unavailable: the daemon gave an unusable \
         answer, so this prompt cannot be admitted while shared-budget.json exists. Restart \
         the Libra daemon (libra-governor daemon run) or remove/fix shared-budget.json \
         ({LABEL})."
    )
}

/// One short, deterministic line for soft-limit warnings. Carries no
/// session/agent ids and no prompt content. `None` when there is nothing
/// to say.
fn warning_line(warnings: &[SoftLimitWarning], unavailable: bool) -> Option<String> {
    if warnings.is_empty() && !unavailable {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    for scope in [SoftLimitScope::PerSession, SoftLimitScope::PerAgent] {
        let worst = warnings
            .iter()
            .filter(|w| w.scope == scope)
            .max_by_key(|w| w.used_plus_reserved_usd_cents);
        if let Some(w) = worst {
            let name = match scope {
                SoftLimitScope::PerSession => "per-session",
                SoftLimitScope::PerAgent => "per-agent",
            };
            parts.push(format!(
                "{name} {} of {}",
                usd_u(w.used_plus_reserved_usd_cents),
                usd_u(w.threshold_usd_cents)
            ));
        }
    }
    if unavailable {
        parts.push("soft-limit status unavailable".to_string());
    }
    Some(format!(
        "[libra-governor] budget soft limit: {} (warn only; Libra-accounted USD, \
         admission-bounded).",
        parts.join(", ")
    ))
}

/// Maps the daemon's answer to an `Admit` to the prompt hook's decision.
pub fn decide_admit(outcome: &SharedBudgetOutcome) -> Admission {
    decide_admit_in(outcome, &BudgetContext::load())
}

fn decide_admit_in(outcome: &SharedBudgetOutcome, ctx: &BudgetContext) -> Admission {
    match outcome {
        SharedBudgetOutcome::NotConfigured => Admission::Proceed { warning: None },
        SharedBudgetOutcome::Allowed {
            warnings,
            warnings_unavailable,
            ..
        } => Admission::Proceed {
            warning: warning_line(warnings, *warnings_unavailable),
        },
        SharedBudgetOutcome::Denied {
            reason,
            remaining_usd_cents,
            period_end,
            detail,
        } => Admission::Block {
            reason: denied_reason(
                *reason,
                *remaining_usd_cents,
                period_end.as_deref(),
                detail.as_deref(),
                ctx,
            ),
        },
        // A refusal is driven purely by host-supplied identity, not by the
        // budget: never block the user's prompt for it.
        SharedBudgetOutcome::Refused { .. } => Admission::Proceed { warning: None },
        // Not an answer to Admit: the budget file exists, so fail closed.
        _ => Admission::Block {
            reason: unusable_reason(),
        },
    }
}

/// Maps the daemon's answer to a `Check` to a PreToolUse denial reason.
/// `None` (allow) for everything except exhausted / denied.
pub fn decide_check(outcome: &SharedBudgetOutcome) -> Option<String> {
    decide_check_in(outcome, &BudgetContext::load())
}

fn decide_check_in(outcome: &SharedBudgetOutcome, ctx: &BudgetContext) -> Option<String> {
    match outcome {
        SharedBudgetOutcome::Checked {
            state: SharedBudgetCheckState::Exhausted,
            remaining_usd_cents,
            period_end,
        } => Some(denied_reason(
            SharedBudgetDenyReason::CapExhausted,
            Some(*remaining_usd_cents),
            Some(period_end),
            None,
            ctx,
        )),
        SharedBudgetOutcome::Denied {
            reason,
            remaining_usd_cents,
            period_end,
            detail,
        } => Some(denied_reason(
            *reason,
            *remaining_usd_cents,
            period_end.as_deref(),
            detail.as_deref(),
            ctx,
        )),
        _ => None,
    }
}

/// Only errors a stale daemon gives *before* handling the request justify a
/// resend. Any other error may follow a committed hold, and an `Admit`
/// without a `prompt_id` is not idempotent, so resending could reserve twice.
fn stale_daemon_error(message: &str) -> bool {
    message.starts_with("protocol version mismatch") || message == "malformed request"
}

/// Asks the daemon to admit one prompt. Only called when the budget file
/// exists. Connecting uses the same autospawn path as Preflight (so
/// `LIBRA_GOVERNOR_NO_AUTOSPAWN` is honoured); a daemon that cannot be
/// reached, or does not answer within the client's request timeout, blocks.
pub fn admit(
    socket_path: &Path,
    session_id: &str,
    prompt_id: Option<String>,
    log: &dyn Fn(&str),
) -> Admission {
    let request = || Request::SharedBudget {
        shared_budget: SharedBudgetRequest::Admit {
            session_id: session_id.to_string(),
            agent_id: None,
            prompt_id: prompt_id.clone(),
        },
    };
    // Two attempts: a daemon from an older release answers once with an
    // error and then shuts down so the next connection spawns a current one.
    for attempt in 0..2 {
        let stream = match client::ensure_daemon_connection(socket_path) {
            Ok(stream) => stream,
            Err(e) => {
                log(&format!("shared budget: daemon unreachable: {e}"));
                return Admission::Block {
                    reason: unreachable_reason(),
                };
            }
        };
        match client::roundtrip(&stream, request()) {
            Ok(Response::SharedBudget(outcome)) => {
                if let SharedBudgetOutcome::Refused { reason } = &*outcome {
                    log(&format!(
                        "shared budget: admit refused ({reason:?}); failing open"
                    ));
                }
                return decide_admit(&outcome);
            }
            Ok(_) => {
                log("shared budget: unexpected response kind for Admit");
                return Admission::Block {
                    reason: unusable_reason(),
                };
            }
            Err(ClientError::DaemonError(message))
                if attempt == 0 && stale_daemon_error(&message) =>
            {
                log(&format!(
                    "shared budget: daemon error, retrying once: {message}"
                ));
            }
            Err(e) => {
                log(&format!("shared budget: admit request failed: {e}"));
                return Admission::Block {
                    reason: unreachable_reason(),
                };
            }
        }
    }
    Admission::Block {
        reason: unusable_reason(),
    }
}

/// Asks the running daemon whether this tool call may proceed. Never
/// spawns a daemon, and every failure (no daemon, timeout, error, odd
/// answer) is `None` = allow. Bounded by [`CHECK_BUDGET`].
pub fn check_denial(
    socket_path: &Path,
    session_id: &str,
    agent_id: Option<&str>,
    log: &dyn Fn(&str),
) -> Option<String> {
    let socket = socket_path.to_path_buf();
    let request = Request::SharedBudget {
        shared_budget: SharedBudgetRequest::Check {
            session_id: session_id.to_string(),
            agent_id: agent_id.map(str::to_owned),
        },
    };
    // One overall deadline around connect + write + read: a daemon that
    // trickles bytes defeats per-read socket timeouts. The worker is
    // abandoned on timeout; the hook process exits right after.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let outcome = client::connect_only_with_budget(&socket, CHECK_BUDGET)
            .and_then(|stream| client::roundtrip(&stream, request));
        let _ = tx.send(outcome);
    });
    match rx.recv_timeout(CHECK_BUDGET) {
        Ok(Ok(Response::SharedBudget(outcome))) => decide_check(&outcome),
        Ok(Ok(_)) => {
            log("pre-tool-use: unexpected response kind for Check, allowing");
            None
        }
        Ok(Err(e)) => {
            log(&format!(
                "pre-tool-use: shared budget check failed, allowing: {e}"
            ));
            None
        }
        Err(_) => {
            log("pre-tool-use: shared budget check timed out, allowing");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libra_governor_domain::{ExecutorKind, SharedPoolReservationId};
    use libra_governor_protocol::SharedBudgetRefusal;

    // The pure tests below see no budget file: they must not read the
    // developer's real state dir.
    fn decide_admit(outcome: &SharedBudgetOutcome) -> Admission {
        decide_admit_in(outcome, &BudgetContext::default())
    }

    fn decide_check(outcome: &SharedBudgetOutcome) -> Option<String> {
        decide_check_in(outcome, &BudgetContext::default())
    }

    fn taipei_ctx() -> BudgetContext {
        BudgetContext {
            file_path: Some(PathBuf::from("/state/shared-budget.json")),
            time_zone: Some(IanaTimeZone::new("Asia/Taipei").unwrap()),
            turn_hold_usd_cents: Some(50),
        }
    }

    fn cap_denial(remaining: i64) -> SharedBudgetOutcome {
        SharedBudgetOutcome::Denied {
            reason: SharedBudgetDenyReason::CapExhausted,
            remaining_usd_cents: Some(remaining),
            period_end: Some("2026-10-31T16:00:00Z".into()),
            detail: None,
        }
    }

    #[test]
    fn the_block_message_shows_the_period_end_in_the_configured_zone() {
        let Admission::Block { reason } = decide_admit_in(&cap_denial(49), &taipei_ctx()) else {
            panic!("must block")
        };
        assert!(
            reason.contains("2026-11-01T00:00:00+08:00 (Asia/Taipei)"),
            "{reason}"
        );
        assert!(!reason.contains("2026-10-31T16:00:00Z"), "{reason}");
    }

    #[test]
    fn the_block_message_explains_why_a_positive_remainder_is_exhausted() {
        let Admission::Block { reason } = decide_admit_in(&cap_denial(49), &taipei_ctx()) else {
            panic!("must block")
        };
        assert!(
            reason.contains("$0.49 remaining is less than the $0.50 a turn reserves"),
            "{reason}"
        );
    }

    #[test]
    fn the_block_message_names_the_remedy_and_keeps_the_label() {
        for outcome in [cap_denial(49), cap_denial(-3)] {
            let Admission::Block { reason } = decide_admit_in(&outcome, &taipei_ctx()) else {
                panic!("must block")
            };
            assert!(
                reason.contains(
                    "raise monthly_cap_usd_cents in /state/shared-budget.json (applies to the \
                     next prompt) or finish or stop other active sessions"
                ),
                "{reason}"
            );
            assert!(reason.contains("Libra-accounted USD, admission-bounded (cooperative)"));
            assert!(!reason.contains('\n'), "a single paragraph: {reason}");
        }
    }

    #[test]
    fn pre_tool_use_denial_uses_the_same_zone_and_reservation_wording() {
        let reason = decide_check_in(
            &SharedBudgetOutcome::Checked {
                state: SharedBudgetCheckState::Exhausted,
                remaining_usd_cents: 49,
                period_end: "2026-10-31T16:00:00Z".into(),
            },
            &taipei_ctx(),
        )
        .expect("exhausted denies");
        assert!(reason.contains("+08:00 (Asia/Taipei)"), "{reason}");
        assert!(reason.contains("a turn reserves"), "{reason}");
    }

    #[test]
    fn without_a_readable_file_the_message_falls_back_to_the_daemons_own_facts() {
        let Admission::Block { reason } =
            decide_admit_in(&cap_denial(49), &BudgetContext::default())
        else {
            panic!("must block")
        };
        assert!(reason.contains("2026-10-31T16:00:00Z"), "{reason}");
        assert!(reason.contains("$0.49 remaining"), "{reason}");
        assert!(reason.contains("raise monthly_cap_usd_cents in shared-budget.json"));
    }

    fn allowed(warnings: Vec<SoftLimitWarning>, unavailable: bool) -> SharedBudgetOutcome {
        SharedBudgetOutcome::Allowed {
            reservation_id: SharedPoolReservationId::new(),
            pool_id: "p".into(),
            hold_usd_cents: 1000,
            hold_reused: false,
            executor_kind: ExecutorKind::AgentAbsent,
            remaining_usd_cents: 500,
            period_end: "2027-02-01T00:00:00Z".into(),
            warnings,
            warnings_unavailable: unavailable,
        }
    }

    fn warn(scope: SoftLimitScope, used: u64, threshold: u64) -> SoftLimitWarning {
        SoftLimitWarning {
            scope,
            session_id: "SECRET-SESSION".into(),
            agent_id: Some("SECRET-AGENT".into()),
            used_plus_reserved_usd_cents: used,
            threshold_usd_cents: threshold,
        }
    }

    #[test]
    fn not_configured_and_clean_allowed_proceed_without_a_warning() {
        assert_eq!(
            decide_admit(&SharedBudgetOutcome::NotConfigured),
            Admission::Proceed { warning: None }
        );
        assert_eq!(
            decide_admit(&allowed(vec![], false)),
            Admission::Proceed { warning: None }
        );
    }

    #[test]
    fn soft_limit_warning_is_one_deterministic_line_without_identifiers() {
        let outcome = allowed(
            vec![
                warn(SoftLimitScope::PerSession, 1000, 500),
                warn(SoftLimitScope::PerAgent, 1250, 1000),
            ],
            false,
        );
        let Admission::Proceed { warning: Some(w) } = decide_admit(&outcome) else {
            panic!("expected a warning");
        };
        assert_eq!(
            w,
            "[libra-governor] budget soft limit: per-session $10.00 of $5.00, per-agent \
             $12.50 of $10.00 (warn only; Libra-accounted USD, admission-bounded)."
        );
        assert!(!w.contains('\n') && !w.contains("SECRET"));
        assert_eq!(decide_admit(&outcome), decide_admit(&outcome));
    }

    #[test]
    fn warnings_unavailable_is_said_once() {
        let Admission::Proceed { warning: Some(w) } = decide_admit(&allowed(vec![], true)) else {
            panic!("expected a warning");
        };
        assert!(w.contains("soft-limit status unavailable"));
    }

    #[test]
    fn every_denial_blocks_with_the_label_and_the_given_facts() {
        let cap = decide_admit(&SharedBudgetOutcome::Denied {
            reason: SharedBudgetDenyReason::CapExhausted,
            remaining_usd_cents: Some(500),
            period_end: Some("2027-02-01T00:00:00Z".into()),
            detail: None,
        });
        let Admission::Block { reason } = cap else {
            panic!("cap must block")
        };
        assert!(reason.contains("$5.00 remaining"), "{reason}");
        assert!(reason.contains("2027-02-01T00:00:00Z"), "{reason}");
        assert!(reason.contains("Libra-accounted USD, admission-bounded"));

        for deny in [
            SharedBudgetDenyReason::ConfigInvalid,
            SharedBudgetDenyReason::ClockAnomaly,
        ] {
            let Admission::Block { reason } = decide_admit(&SharedBudgetOutcome::Denied {
                reason: deny,
                remaining_usd_cents: None,
                period_end: None,
                detail: Some("bad\nthing".into()),
            }) else {
                panic!("{deny:?} must block")
            };
            assert!(reason.contains("Libra-accounted USD, admission-bounded"));
            assert!(!reason.contains('\n'));
        }
        // Host-input refusals never block.
        assert_eq!(
            decide_admit(&SharedBudgetOutcome::Refused {
                reason: SharedBudgetRefusal::EmptySessionId,
            }),
            Admission::Proceed { warning: None }
        );
    }

    #[test]
    fn host_driven_session_ids_are_unusable_but_real_ones_are_not() {
        for bad in [
            "",
            "   ",
            "\t\n",
            "execution-owner-v1:abc",
            "legacy-executor-v1:[\"agent\",null,\"s\",\"a\"]",
        ] {
            assert!(session_id_unusable(bad), "{bad:?}");
        }
        assert!(!session_id_unusable("sess-1"));
    }

    #[test]
    fn only_known_stale_daemon_errors_are_retried() {
        assert!(stale_daemon_error(
            "protocol version mismatch: daemon speaks 20, client sent 21"
        ));
        assert!(stale_daemon_error("malformed request"));
        assert!(!stale_daemon_error("ledger busy"));
    }

    #[test]
    fn an_overdrawn_cap_says_so_instead_of_a_negative_remainder() {
        let Admission::Block { reason } = decide_admit(&SharedBudgetOutcome::Denied {
            reason: SharedBudgetDenyReason::CapExhausted,
            remaining_usd_cents: Some(-3),
            period_end: None,
            detail: None,
        }) else {
            panic!()
        };
        assert!(reason.contains("over the cap by $0.03"), "{reason}");
    }

    #[test]
    fn a_non_admit_answer_with_a_budget_file_fails_closed() {
        assert!(matches!(
            decide_admit(&SharedBudgetOutcome::Checked {
                state: SharedBudgetCheckState::Available,
                remaining_usd_cents: 1,
                period_end: "x".into(),
            }),
            Admission::Block { .. }
        ));
    }

    #[test]
    fn check_denies_only_exhausted_and_denied() {
        assert!(decide_check(&SharedBudgetOutcome::Checked {
            state: SharedBudgetCheckState::Exhausted,
            remaining_usd_cents: 0,
            period_end: "2027-02-01T00:00:00Z".into(),
        })
        .is_some());
        assert!(decide_check(&SharedBudgetOutcome::Denied {
            reason: SharedBudgetDenyReason::ClockAnomaly,
            remaining_usd_cents: None,
            period_end: None,
            detail: None,
        })
        .is_some());
        for state in [
            SharedBudgetCheckState::Available,
            SharedBudgetCheckState::Admitted,
        ] {
            assert!(decide_check(&SharedBudgetOutcome::Checked {
                state,
                remaining_usd_cents: 9,
                period_end: "x".into(),
            })
            .is_none());
        }
        assert!(decide_check(&SharedBudgetOutcome::NotConfigured).is_none());
        assert!(decide_check(&SharedBudgetOutcome::Refused {
            reason: SharedBudgetRefusal::EmptySessionId
        })
        .is_none());
    }
}
