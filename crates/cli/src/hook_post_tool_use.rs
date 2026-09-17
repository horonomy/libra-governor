//! `libra-governor hook post-tool-use` — the Claude Code `PostToolUse`
//! hook entry point.
//!
//! Reads the hook JSON payload from stdin and fires a cheap,
//! fire-and-forget `ToolInvoked` notification at the daemon (see
//! [`crate::client::fire_and_forget`]) so it can increment a per-session
//! tool-call counter for the eventual Execution Receipt (HORO-1126).
//! Never spawns the daemon (see `fire_and_forget` docs) and never blocks
//! on a response — this must not add perceptible latency to every tool
//! call. Never panics, never exits nonzero, and prints nothing to
//! stdout: MVP 1.0 is advisory only, there is nothing to inject into
//! Claude Code's context here.

use std::path::PathBuf;

use libra_governor_protocol::Request;
use serde::Deserialize;

use crate::client;

/// The subset of the Claude Code `PostToolUse` hook payload this
/// integration needs. Extra fields (`tool_input`, `tool_output`, `model`,
/// ...) are ignored, not rejected.
#[derive(Debug, Deserialize)]
struct HookPayload {
    session_id: String,
    tool_name: String,
}

pub fn run() {
    let log_path = libra_governor_daemon::paths::log_path();

    let mut raw = String::new();
    if std::io::Read::read_to_string(&mut std::io::stdin(), &mut raw).is_err() {
        log(
            &log_path,
            "post-tool-use: failed to read hook payload from stdin",
        );
        return;
    }

    let payload: HookPayload = match serde_json::from_str(&raw) {
        Ok(p) => p,
        Err(e) => {
            log(
                &log_path,
                &format!("post-tool-use: malformed hook payload: {e}"),
            );
            return;
        }
    };

    let socket_path = match libra_governor_daemon::paths::socket_path() {
        Ok(p) => p,
        Err(e) => {
            log(
                &log_path,
                &format!("post-tool-use: could not resolve state dir: {e}"),
            );
            return;
        }
    };

    let request = Request::ToolInvoked {
        session_id: payload.session_id,
        tool_name: payload.tool_name,
    };

    // Best-effort: a daemon-unavailable or write error here is not
    // actionable by the user and must never surface as hook output —
    // just log it and move on.
    if let Err(e) = client::fire_and_forget(&socket_path, request) {
        log(
            &log_path,
            &format!("post-tool-use: fire_and_forget failed: {e}"),
        );
    }
}

fn log(log_path: &Result<PathBuf, libra_governor_daemon::paths::PathsError>, message: &str) {
    if let Ok(path) = log_path {
        libra_governor_daemon::log::append_line(path, message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_hook_payload_fails_to_deserialize() {
        let result: Result<HookPayload, _> = serde_json::from_str("not json");
        assert!(result.is_err());
    }

    #[test]
    fn hook_payload_ignores_unknown_fields() {
        let json = r#"{
            "session_id": "sess-1",
            "cwd": "/repo",
            "tool_name": "Bash",
            "tool_input": {"command": "ls"},
            "tool_output": "some output",
            "hook_event_name": "PostToolUse"
        }"#;
        let payload: HookPayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.session_id, "sess-1");
        assert_eq!(payload.tool_name, "Bash");
    }
}
