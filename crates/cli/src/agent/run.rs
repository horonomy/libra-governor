//! [`run_prompt_submit`]/[`run_tool_completed`]/[`run_turn_completed`] —
//! the single shared implementation both Claude Code's and Codex's hook
//! entry points call (HORO-1157).
//!
//! Preserves the exact existing stdout/stderr discipline,
//! daemon-spawn-on-preflight-only behavior, and never-block /
//! never-panic / never-nonzero-exit contract of the pre-HORO-1157
//! `hook.rs`/`hook_post_tool_use.rs`/`hook_stop.rs` — see
//! `crates/cli/tests/agent_contract.rs` for the byte-exact regression
//! proof against Claude Code. `crates/cli/src/hook.rs`,
//! `hook_post_tool_use.rs`, and `hook_stop.rs` are now thin callers into
//! this module; `crates/cli/src/codex_hook.rs` calls the exact same
//! functions.

use std::io::Read;
use std::path::PathBuf;

use libra_governor_domain::{
    AssociationUnavailable, ExecutionIdentity, EXECUTION_ASSOCIATION_VERSION,
};
use libra_governor_protocol::{
    ExecutionEffect, ExecutionOperation, ExecutionOwnerOutcome, ExecutionOwnerRequest,
    FinalizeOutcome, NativeExecutionContext, Request, Response,
};

use super::event::NormalizedEvent;
use super::normalize::{normalize, EntryPoint, NormalizeError};
use super::render;
use super::AgentKind;
use crate::client;

/// HORO-1714 decision gate (2026-10-10): the Codex ExecutionOwner route is
/// off by default. The founder's authorization for decisions A/B/C
/// explicitly excludes "activation of unverified economic effects" --
/// building the route is authorized, switching it on for real traffic is a
/// separate decision requiring real-host Codex verification first. Claude
/// Code never uses this route regardless of the flag: its byte-exact
/// contract with the legacy path (`crates/cli/tests/agent_contract.rs`)
/// stays untouched.
fn codex_owner_route_enabled() -> bool {
    std::env::var("LIBRA_GOVERNOR_CODEX_EXECUTION_OWNER")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// A short, honest description of why the owner route produced no
/// economic effect -- never a claim of success, never silently dropped.
fn no_effect_text(reason: AssociationUnavailable) -> &'static str {
    match reason {
        AssociationUnavailable::Missing => "no bound task exists yet for this lane",
        AssociationUnavailable::Ambiguous => "association is ambiguous; refusing to guess",
        AssociationUnavailable::Stale => "this turn has been superseded",
        AssociationUnavailable::Unsupported => "this event's identity shape is unsupported",
        AssociationUnavailable::ReplayConflict => "native reference was reused across turns",
    }
}

/// Bounded retries for the owner round trip only -- never for the legacy
/// path, which already has its own best-effort failure handling. A lost
/// response or storage error is retried a small, fixed number of times
/// within the hook's own timeout; this is not an unbounded loop and never
/// retries a result that was actually returned (only transport failure).
const OWNER_ROUTE_MAX_ATTEMPTS: u32 = 3;

/// Logs `message` prefixed with `agent`'s label — the only place
/// [`AgentKind`] is observable in this module's behavior; it never
/// affects the stdout/stderr hook contract itself (see [`print_context`]
/// and [`render`]), only which agent a log line names.
fn log(
    log_path: &Result<PathBuf, libra_governor_daemon::paths::PathsError>,
    agent: AgentKind,
    message: &str,
) {
    if let Ok(path) = log_path {
        libra_governor_daemon::log::append_line(
            path,
            &format!("[{}] {message}", super::label(agent)),
        );
    }
}

fn print_context(message: &str) {
    println!("{}", render::render_context_envelope(message));
}

/// Builds this event's [`ExecutionIdentity`] (HORO-1599) and logs a
/// redacted summary line. Never fails the hook: a host_id or validation
/// error here is logged and swallowed, exactly like every other
/// best-effort failure in this module. Returns the captured identity so
/// the Codex owner route (HORO-1714) can reuse the exact same capture for
/// both identity slots a request needs -- two separate captures would
/// disagree on `observed_at` and the owner would refuse them as
/// `Ambiguous`.
fn capture_and_log_identity(
    log_path: &Result<PathBuf, libra_governor_daemon::paths::PathsError>,
    agent: AgentKind,
    entry_label: &str,
    session_id: &str,
    agent_id: Option<&str>,
    turn_id: Option<&str>,
) -> Option<ExecutionIdentity> {
    let host_id = match libra_governor_daemon::paths::ensure_state_dir()
        .map_err(|e| e.to_string())
        .and_then(|dir| super::identity::resolve_host_id(&dir).map_err(|e| e.to_string()))
    {
        Ok(host_id) => host_id,
        Err(e) => {
            log(
                log_path,
                agent,
                &format!("{entry_label}: could not resolve host_id: {e}"),
            );
            return None;
        }
    };

    match super::identity::capture(
        &host_id,
        agent,
        session_id,
        agent_id,
        turn_id,
        time::OffsetDateTime::now_utc(),
    ) {
        Ok(identity) => {
            log(
                log_path,
                agent,
                &format!(
                    "{entry_label}: execution identity host={} session={} agent={} lineage={:?}",
                    identity.display_id("host_id", identity.host_id()),
                    identity.display_id("provider_session_id", session_id),
                    agent_id
                        .map(|a| identity.display_id("agent_id", a))
                        .unwrap_or_else(|| "none".to_string()),
                    identity.lineage_status(),
                ),
            );
            Some(identity)
        }
        Err(e) => {
            log(
                log_path,
                agent,
                &format!("{entry_label}: execution identity invalid, not captured: {e}"),
            );
            None
        }
    }
}

fn read_stdin(
    log_path: &Result<PathBuf, libra_governor_daemon::paths::PathsError>,
    agent: AgentKind,
) -> Option<String> {
    let mut raw = String::new();
    if std::io::stdin().read_to_string(&mut raw).is_err() {
        log(log_path, agent, "failed to read hook payload from stdin");
        return None;
    }
    Some(raw)
}

/// Reads stdin, talks to the daemon (starting it if absent), and prints
/// a `hookSpecificOutput.additionalContext` JSON object to stdout. Never
/// panics, never exits nonzero.
pub fn run_prompt_submit(agent: AgentKind) {
    let log_path = libra_governor_daemon::paths::log_path();
    let Some(raw) = read_stdin(&log_path, agent) else {
        print_context("[libra-governor] preflight skipped: could not read hook payload.");
        return;
    };

    run_prompt_submit_with_input(agent, &raw);
}

/// The installed callback uses the same consumer after its bounded input gate.
pub(crate) fn run_prompt_submit_with_input(agent: AgentKind, raw: &str) {
    let log_path = libra_governor_daemon::paths::log_path();

    let event = match normalize(EntryPoint::PromptSubmit, raw) {
        Ok(event) => event,
        Err(NormalizeError::Malformed(e)) => {
            log(&log_path, agent, &format!("malformed hook payload: {e}"));
            print_context("[libra-governor] preflight skipped: malformed hook payload.");
            return;
        }
    };

    let (session_id, cwd, prompt, turn_id, agent_id) = match event {
        NormalizedEvent::PromptSubmitted {
            session_id,
            cwd,
            prompt,
            turn_id,
            agent_id,
        } => (session_id, cwd, prompt, turn_id, agent_id),
        NormalizedEvent::RecognizedUnwired { hook_event_name } => {
            log(
                &log_path,
                agent,
                &format!("preflight: recognized-but-unwired hook event {hook_event_name}, no-op"),
            );
            print_context("[libra-governor] preflight skipped: unwired hook event.");
            return;
        }
        NormalizedEvent::Unrecognized { hook_event_name } => {
            log(
                &log_path,
                agent,
                &format!("preflight: unrecognized hook event {hook_event_name}, no-op"),
            );
            print_context("[libra-governor] preflight skipped: unrecognized hook event.");
            return;
        }
        NormalizedEvent::ToolCompleted { .. } | NormalizedEvent::TurnCompleted { .. } => {
            unreachable!("normalize(EntryPoint::PromptSubmit, ..) never returns these variants")
        }
    };

    let identity = capture_and_log_identity(
        &log_path,
        agent,
        "preflight",
        &session_id,
        agent_id.as_deref(),
        turn_id.as_deref(),
    );

    let socket_path = match libra_governor_daemon::paths::socket_path() {
        Ok(p) => p,
        Err(e) => {
            log(
                &log_path,
                agent,
                &format!("could not resolve state dir: {e}"),
            );
            print_context("[libra-governor] preflight skipped: daemon state dir unavailable.");
            return;
        }
    };

    let stream = match client::ensure_daemon_connection(&socket_path) {
        Ok(stream) => stream,
        Err(e) => {
            log(&log_path, agent, &format!("daemon unavailable: {e}"));
            print_context(
                "[libra-governor] preflight unavailable: daemon unreachable. Proceeding \
                 without governance.",
            );
            return;
        }
    };

    // HORO-1714: Codex, and only behind the default-off gate, uses the
    // ExecutionOwner route exclusively -- never falling back to the legacy
    // Preflight path below once this branch is taken, even on a no-effect
    // outcome. Claude Code never reaches this branch regardless of the
    // flag.
    if agent == AgentKind::Codex && codex_owner_route_enabled() {
        let Some(identity) = identity else {
            print_context("[libra-governor] no effect: execution identity unavailable.");
            return;
        };
        let request = Request::ExecutionOwner {
            event: Box::new(ExecutionOwnerRequest {
                association_version: EXECUTION_ASSOCIATION_VERSION,
                identity: identity.clone(),
                native_context: NativeExecutionContext {
                    identity,
                    operation: ExecutionOperation::Prompt {
                        task_hint: prompt,
                        cwd,
                        // Codex exposes no native predecessor field; the
                        // owner's own finalized-predecessor succession
                        // (decision B) is the only legal path to a second
                        // turn on an existing lane.
                        supersedes_turn: None,
                    },
                },
            }),
        };
        match client::roundtrip(&stream, request) {
            Ok(Response::ExecutionOwner(outcome)) => match *outcome {
                ExecutionOwnerOutcome::Applied {
                    effect: boxed_effect,
                    ..
                } => match *boxed_effect {
                    ExecutionEffect::Prompt { result } => {
                        print_context(&render::format_additional_context(&result))
                    }
                    other => {
                        log(
                            &log_path,
                            agent,
                            &format!("preflight: unexpected effect for Prompt: {other:?}"),
                        );
                        print_context(
                            "[libra-governor] preflight skipped: unexpected daemon response.",
                        );
                    }
                },
                ExecutionOwnerOutcome::Duplicate { .. } => print_context(
                    "[libra-governor] no effect: this prompt was already applied (replay).",
                ),
                ExecutionOwnerOutcome::Unavailable { reason } => print_context(&format!(
                    "[libra-governor] no effect: {}.",
                    no_effect_text(reason)
                )),
                ExecutionOwnerOutcome::Resolved { .. } => {
                    log(
                        &log_path,
                        agent,
                        "preflight: Resolved is not a Prompt outcome",
                    );
                    print_context(
                        "[libra-governor] preflight skipped: unexpected daemon response.",
                    );
                }
            },
            Ok(other) => {
                log(
                    &log_path,
                    agent,
                    &format!("unexpected response kind for ExecutionOwner: {other:?}"),
                );
                print_context("[libra-governor] preflight skipped: unexpected daemon response.");
            }
            Err(e) => {
                log(
                    &log_path,
                    agent,
                    &format!("execution owner request failed: {e}"),
                );
                print_context(
                    "[libra-governor] preflight unavailable: request to daemon failed. \
                     Proceeding without governance.",
                );
            }
        }
        return;
    }

    let request = Request::Preflight {
        task_hint: prompt,
        cwd,
        session_id,
    };

    match client::roundtrip(&stream, request) {
        Ok(Response::Preflight(result)) => {
            print_context(&render::format_additional_context(&result))
        }
        Ok(other) => {
            log(
                &log_path,
                agent,
                &format!("unexpected response kind for Preflight: {other:?}"),
            );
            print_context("[libra-governor] preflight skipped: unexpected daemon response.");
        }
        Err(e) => {
            log(&log_path, agent, &format!("preflight request failed: {e}"));
            print_context(
                "[libra-governor] preflight unavailable: request to daemon failed. \
                 Proceeding without governance.",
            );
        }
    }
}

/// Reads stdin and fires a cheap, fire-and-forget `ToolInvoked`
/// notification at the daemon. Never spawns the daemon, never blocks on
/// a response, never panics, never exits nonzero, prints nothing to
/// stdout.
pub fn run_tool_completed(agent: AgentKind) {
    let log_path = libra_governor_daemon::paths::log_path();
    let Some(raw) = read_stdin(&log_path, agent) else {
        return;
    };

    run_tool_completed_with_input(agent, &raw);
}

/// The installed callback uses the same consumer after its bounded input gate.
pub(crate) fn run_tool_completed_with_input(agent: AgentKind, raw: &str) {
    let log_path = libra_governor_daemon::paths::log_path();

    let event = match normalize(EntryPoint::ToolCompleted, raw) {
        Ok(event) => event,
        Err(NormalizeError::Malformed(e)) => {
            log(
                &log_path,
                agent,
                &format!("post-tool-use: malformed hook payload: {e}"),
            );
            return;
        }
    };

    let (session_id, tool_name, native_call_id, turn_id, agent_id) = match event {
        NormalizedEvent::ToolCompleted {
            session_id,
            tool_name,
            native_call_id,
            turn_id,
            agent_id,
        } => (session_id, tool_name, native_call_id, turn_id, agent_id),
        NormalizedEvent::RecognizedUnwired { hook_event_name } => {
            log(
                &log_path,
                agent,
                &format!(
                    "post-tool-use: recognized-but-unwired hook event {hook_event_name}, no-op"
                ),
            );
            return;
        }
        NormalizedEvent::Unrecognized { hook_event_name } => {
            log(
                &log_path,
                agent,
                &format!("post-tool-use: unrecognized hook event {hook_event_name}, no-op"),
            );
            return;
        }
        NormalizedEvent::PromptSubmitted { .. } | NormalizedEvent::TurnCompleted { .. } => {
            unreachable!("normalize(EntryPoint::ToolCompleted, ..) never returns these variants")
        }
    };

    let identity = capture_and_log_identity(
        &log_path,
        agent,
        "post-tool-use",
        &session_id,
        agent_id.as_deref(),
        turn_id.as_deref(),
    );

    let socket_path = match libra_governor_daemon::paths::socket_path() {
        Ok(p) => p,
        Err(e) => {
            log(
                &log_path,
                agent,
                &format!("post-tool-use: could not resolve state dir: {e}"),
            );
            return;
        }
    };

    if agent == AgentKind::Codex && codex_owner_route_enabled() {
        let Some(identity) = identity else {
            log(
                &log_path,
                agent,
                "post-tool-use: no effect: execution identity unavailable",
            );
            return;
        };
        let Some(native_call_id) = native_call_id else {
            log(
                &log_path,
                agent,
                "post-tool-use: no effect: no native tool_use_id reported for this call",
            );
            return;
        };
        let request = Request::ExecutionOwner {
            event: Box::new(ExecutionOwnerRequest {
                association_version: EXECUTION_ASSOCIATION_VERSION,
                identity: identity.clone(),
                native_context: NativeExecutionContext {
                    identity,
                    operation: ExecutionOperation::Tool {
                        native_call_id,
                        tool_name,
                    },
                },
            }),
        };
        // A missing/unreachable response is retried a small bounded
        // number of times (transport failure only -- never a result that
        // was actually returned); each attempt opens its own connection
        // since the stream from a failed attempt cannot be trusted.
        let mut last_error = None;
        for _ in 0..OWNER_ROUTE_MAX_ATTEMPTS {
            let stream = match client::connect_only(&socket_path) {
                Ok(stream) => stream,
                Err(e) => {
                    last_error = Some(e.to_string());
                    continue;
                }
            };
            match client::roundtrip(&stream, request.clone()) {
                Ok(Response::ExecutionOwner(outcome)) => {
                    if let ExecutionOwnerOutcome::Unavailable { reason } = *outcome {
                        log(
                            &log_path,
                            agent,
                            &format!("post-tool-use: no effect: {}", no_effect_text(reason)),
                        );
                    }
                    last_error = None;
                    break;
                }
                Ok(other) => {
                    log(
                        &log_path,
                        agent,
                        &format!(
                            "post-tool-use: unexpected response kind for ExecutionOwner: {other:?}"
                        ),
                    );
                    last_error = None;
                    break;
                }
                Err(e) => {
                    last_error = Some(e.to_string());
                }
            }
        }
        if let Some(e) = last_error {
            log(
                &log_path,
                agent,
                &format!("post-tool-use: execution owner request failed after retries: {e}"),
            );
        }
        return;
    }

    let request = Request::ToolInvoked {
        session_id,
        tool_name,
    };

    // Best-effort: a daemon-unavailable or write error here is not
    // actionable by the user and must never surface as hook output —
    // just log it and move on.
    if let Err(e) = client::fire_and_forget(&socket_path, request) {
        log(
            &log_path,
            agent,
            &format!("post-tool-use: fire_and_forget failed: {e}"),
        );
    }
}

/// Reads stdin and asks the daemon to finalize the task bound to this
/// session. Prints a concise human-readable summary to stderr — never
/// stdout. Never spawns the daemon, never panics, never exits nonzero.
pub fn run_turn_completed(agent: AgentKind) {
    let log_path = libra_governor_daemon::paths::log_path();
    let Some(raw) = read_stdin(&log_path, agent) else {
        return;
    };

    run_turn_completed_with_input(agent, &raw);
}

/// The installed callback uses the same consumer after its bounded input gate.
pub(crate) fn run_turn_completed_with_input(agent: AgentKind, raw: &str) {
    let log_path = libra_governor_daemon::paths::log_path();

    let event = match normalize(EntryPoint::TurnCompleted, raw) {
        Ok(event) => event,
        Err(NormalizeError::Malformed(e)) => {
            log(
                &log_path,
                agent,
                &format!("stop: malformed hook payload: {e}"),
            );
            return;
        }
    };

    let (session_id, model, turn_id, agent_id, transcript_path) = match event {
        NormalizedEvent::TurnCompleted {
            session_id,
            model,
            turn_id,
            agent_id,
            transcript_path,
        } => (session_id, model, turn_id, agent_id, transcript_path),
        NormalizedEvent::RecognizedUnwired { hook_event_name } => {
            log(
                &log_path,
                agent,
                &format!("stop: recognized-but-unwired hook event {hook_event_name}, no-op"),
            );
            return;
        }
        NormalizedEvent::Unrecognized { hook_event_name } => {
            log(
                &log_path,
                agent,
                &format!("stop: unrecognized hook event {hook_event_name}, no-op"),
            );
            return;
        }
        NormalizedEvent::PromptSubmitted { .. } | NormalizedEvent::ToolCompleted { .. } => {
            unreachable!("normalize(EntryPoint::TurnCompleted, ..) never returns these variants")
        }
    };

    let identity = capture_and_log_identity(
        &log_path,
        agent,
        "stop",
        &session_id,
        agent_id.as_deref(),
        turn_id.as_deref(),
    );

    let socket_path = match libra_governor_daemon::paths::socket_path() {
        Ok(p) => p,
        Err(e) => {
            log(
                &log_path,
                agent,
                &format!("stop: could not resolve state dir: {e}"),
            );
            return;
        }
    };

    let stream = match client::connect_only(&socket_path) {
        Ok(stream) => stream,
        Err(e) => {
            // No daemon reachable: nothing to finalize against. A safe,
            // silent no-op, not an error condition worth surfacing.
            log(
                &log_path,
                agent,
                &format!("stop: daemon unreachable, skipping finalize: {e}"),
            );
            return;
        }
    };

    if agent == AgentKind::Codex && codex_owner_route_enabled() {
        let Some(identity) = identity else {
            log(
                &log_path,
                agent,
                "stop: no effect: execution identity unavailable",
            );
            return;
        };
        let request = Request::ExecutionOwner {
            event: Box::new(ExecutionOwnerRequest {
                association_version: EXECUTION_ASSOCIATION_VERSION,
                identity: identity.clone(),
                native_context: NativeExecutionContext {
                    identity,
                    operation: ExecutionOperation::Stop {
                        model,
                        // The owner route's reader measures a session
                        // window, not an agent/turn window, and would
                        // cross-associate sibling usage -- the daemon
                        // itself refuses any Some() here (Unsupported).
                        // Never opened by this crate regardless.
                        transcript_path: None,
                    },
                },
            }),
        };
        match client::roundtrip(&stream, request) {
            Ok(Response::ExecutionOwner(outcome)) => match *outcome {
                ExecutionOwnerOutcome::Applied {
                    effect: boxed_effect,
                    ..
                } => match *boxed_effect {
                    ExecutionEffect::Stop { result } => match *result {
                        FinalizeOutcome::Finalized(result) => {
                            eprintln!("{}", render::format_receipt_summary(&result))
                        }
                        FinalizeOutcome::NoActiveTask => log(
                            &log_path,
                            agent,
                            "stop: no active task for this session, safe no-op",
                        ),
                    },
                    other => log(
                        &log_path,
                        agent,
                        &format!("stop: unexpected effect for Stop: {other:?}"),
                    ),
                },
                ExecutionOwnerOutcome::Duplicate { .. } => log(
                    &log_path,
                    agent,
                    "stop: duplicate, already finalized (replay)",
                ),
                ExecutionOwnerOutcome::Unavailable { reason } => log(
                    &log_path,
                    agent,
                    &format!("stop: no effect: {}", no_effect_text(reason)),
                ),
                ExecutionOwnerOutcome::Resolved { .. } => {
                    log(&log_path, agent, "stop: Resolved is not a Stop outcome")
                }
            },
            Ok(other) => log(
                &log_path,
                agent,
                &format!("stop: unexpected response kind for ExecutionOwner: {other:?}"),
            ),
            Err(e) => log(
                &log_path,
                agent,
                &format!("stop: execution owner request failed: {e}"),
            ),
        }
        return;
    }

    let request = Request::Finalize {
        session_id,
        model,
        provider: Some(super::label(agent).to_string()),
        // Relayed verbatim, never opened here (HORO-1725): a lossy
        // conversion is still a conversion, and this crate has no
        // business reading the transcript.
        transcript_path: transcript_path.map(|p| p.to_string_lossy().into_owned()),
    };

    match client::roundtrip(&stream, request) {
        Ok(Response::Finalize(FinalizeOutcome::NoActiveTask)) => {
            log(
                &log_path,
                agent,
                "stop: no active task for this session, safe no-op",
            );
        }
        Ok(Response::Finalize(FinalizeOutcome::Finalized(result))) => {
            eprintln!("{}", render::format_receipt_summary(&result));
        }
        Ok(other) => {
            log(
                &log_path,
                agent,
                &format!("stop: unexpected response kind for Finalize: {other:?}"),
            );
        }
        Err(e) => {
            log(
                &log_path,
                agent,
                &format!("stop: finalize request failed: {e}"),
            );
        }
    }
}
