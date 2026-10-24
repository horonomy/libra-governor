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

use libra_governor_protocol::{
    Request, Response, SharedBudgetCheckState, SharedBudgetDenyReason, SharedBudgetOutcome,
    SharedBudgetRefusal, SharedBudgetRequest, SoftLimitScope, SoftLimitWarning,
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

/// Cheap stat: does a budget file exist? "Not found", and "the state
/// directory is not a directory" (nothing can exist beneath it), count as
/// absent; any other stat error (permissions, I/O) is treated as present so
/// the daemon, not a failed stat, decides.
pub fn budget_file_present() -> bool {
    match budget_file_path() {
        Some(path) => match std::fs::symlink_metadata(&path) {
            Ok(_) => true,
            Err(e) => {
                e.kind() != std::io::ErrorKind::NotFound && e.raw_os_error() != Some(libc::ENOTDIR)
            }
        },
        None => false,
    }
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

fn denied_reason(
    reason: SharedBudgetDenyReason,
    remaining: Option<i64>,
    period_end: Option<&str>,
    detail: Option<&str>,
) -> String {
    match reason {
        SharedBudgetDenyReason::CapExhausted => {
            let left = match remaining {
                Some(r) if r >= 0 => format!("{} remaining", usd(r)),
                Some(r) => format!("over the cap by {}", usd(-r)),
                None => "no budget remaining".to_string(),
            };
            let end = period_end
                .map(|e| format!("; the period ends {e}"))
                .unwrap_or_default();
            format!("Libra shared monthly budget exhausted: {left}{end} ({LABEL}).")
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

fn refusal_name(reason: SharedBudgetRefusal) -> &'static str {
    match reason {
        SharedBudgetRefusal::EmptySessionId => "empty session id",
        SharedBudgetRefusal::ReservedSessionScope => "reserved session scope",
        SharedBudgetRefusal::InvalidAgentId => "invalid agent id",
        SharedBudgetRefusal::InvalidAmount => "invalid amount",
        SharedBudgetRefusal::AmountTooLarge => "amount too large",
        SharedBudgetRefusal::InvalidIdempotencyKey => "invalid idempotency key",
        SharedBudgetRefusal::InvalidEventTime => "invalid event time",
        SharedBudgetRefusal::KeyConflict => "key conflict",
        SharedBudgetRefusal::PoolUnitMismatch => "pool unit mismatch",
    }
}

fn unreachable_reason() -> String {
    format!(
        "Libra shared budget: budget enforcement unavailable: daemon unreachable, so this \
         prompt cannot be admitted while shared-budget.json exists ({LABEL})."
    )
}

fn unusable_reason() -> String {
    format!(
        "Libra shared budget: budget enforcement unavailable: the daemon gave an unusable \
         answer, so this prompt cannot be admitted while shared-budget.json exists ({LABEL})."
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
            ),
        },
        SharedBudgetOutcome::Refused { reason } => Admission::Block {
            reason: format!(
                "Libra shared budget refused this session identity ({}) ({LABEL}).",
                refusal_name(*reason)
            ),
        },
        // Not an answer to Admit: the budget file exists, so fail closed.
        _ => Admission::Block {
            reason: unusable_reason(),
        },
    }
}

/// Maps the daemon's answer to a `Check` to a PreToolUse denial reason.
/// `None` (allow) for everything except exhausted / denied.
pub fn decide_check(outcome: &SharedBudgetOutcome) -> Option<String> {
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
        )),
        _ => None,
    }
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
            Ok(Response::SharedBudget(outcome)) => return decide_admit(&outcome),
            Ok(_) => {
                log("shared budget: unexpected response kind for Admit");
                return Admission::Block {
                    reason: unusable_reason(),
                };
            }
            Err(ClientError::DaemonError(message)) if attempt == 0 => {
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
    let stream = match client::connect_only_with_budget(socket_path, CHECK_BUDGET) {
        Ok(stream) => stream,
        Err(e) => {
            log(&format!("pre-tool-use: shared budget check skipped: {e}"));
            return None;
        }
    };
    let request = Request::SharedBudget {
        shared_budget: SharedBudgetRequest::Check {
            session_id: session_id.to_string(),
            agent_id: agent_id.map(str::to_owned),
        },
    };
    match client::roundtrip(&stream, request) {
        Ok(Response::SharedBudget(outcome)) => decide_check(&outcome),
        Ok(_) => {
            log("pre-tool-use: unexpected response kind for Check, allowing");
            None
        }
        Err(e) => {
            log(&format!(
                "pre-tool-use: shared budget check failed, allowing: {e}"
            ));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libra_governor_domain::{ExecutorKind, SharedPoolReservationId};

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
        let Admission::Block { reason } = decide_admit(&SharedBudgetOutcome::Refused {
            reason: SharedBudgetRefusal::EmptySessionId,
        }) else {
            panic!("refusal must block")
        };
        assert!(reason.contains("empty session id"));
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
