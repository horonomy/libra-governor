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

use libra_governor_protocol::{FinalizeOutcome, Request, Response};

use super::event::NormalizedEvent;
use super::normalize::{normalize, EntryPoint, NormalizeError};
use super::render;
use super::AgentKind;
use crate::client;

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
        libra_governor_daemon::log::append_line(path, &format!("[{}] {message}", agent.label()));
    }
}

fn print_context(message: &str) {
    println!("{}", render::render_context_envelope(message));
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

    let event = match normalize(EntryPoint::PromptSubmit, &raw) {
        Ok(event) => event,
        Err(NormalizeError::Malformed(e)) => {
            log(&log_path, agent, &format!("malformed hook payload: {e}"));
            print_context("[libra-governor] preflight skipped: malformed hook payload.");
            return;
        }
    };

    let (session_id, cwd, prompt) = match event {
        NormalizedEvent::PromptSubmitted {
            session_id,
            cwd,
            prompt,
        } => (session_id, cwd, prompt),
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

    let event = match normalize(EntryPoint::ToolCompleted, &raw) {
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

    let (session_id, tool_name) = match event {
        NormalizedEvent::ToolCompleted {
            session_id,
            tool_name,
        } => (session_id, tool_name),
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

    let event = match normalize(EntryPoint::TurnCompleted, &raw) {
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

    let (session_id, model) = match event {
        NormalizedEvent::TurnCompleted { session_id, model } => (session_id, model),
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

    let request = Request::Finalize { session_id, model };

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
