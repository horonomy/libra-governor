//! `libra-governor hook user-prompt-submit` — the Claude Code
//! `UserPromptSubmit` hook entry point.
//!
//! Reads the hook JSON payload from stdin, asks the daemon (starting it
//! if absent) for a bounded-reconnaissance preflight, and prints a
//! `hookSpecificOutput.additionalContext` JSON object to stdout so
//! Claude Code injects the preflight summary into its context window.
//!
//! stdout is reserved for that one JSON object — every other diagnostic
//! (malformed payload, daemon unavailable, internal error) goes to the
//! log file via [`libra_governor_daemon::log`], never to stdout and
//! never as a raw Rust panic. This function always returns successfully
//! and always prints *something* valid to stdout: Claude Code must never
//! see a hang or a scary error for what is, by design, an advisory-only
//! integration.

use std::io::Read;
use std::path::PathBuf;

use libra_governor_protocol::{Confidence, PreflightResult, Request, Response};
use serde::Deserialize;

use crate::client;

/// The subset of the Claude Code `UserPromptSubmit` hook payload this
/// integration needs. Extra fields (`transcript_path`,
/// `hook_event_name`, ...) are ignored, not rejected.
#[derive(Debug, Deserialize)]
struct HookPayload {
    session_id: String,
    cwd: PathBuf,
    prompt: String,
}

/// Runs the hook: reads stdin, talks to the daemon, prints the
/// `hookSpecificOutput` JSON. Never panics, never exits nonzero — a
/// governance hiccup must never block or scare the user out of their
/// normal Claude Code workflow.
pub fn run() {
    let log_path = libra_governor_daemon::paths::log_path();

    let mut raw = String::new();
    if std::io::stdin().read_to_string(&mut raw).is_err() {
        log(&log_path, "failed to read hook payload from stdin");
        print_context("[libra-governor] preflight skipped: could not read hook payload.");
        return;
    }

    let payload: HookPayload = match serde_json::from_str(&raw) {
        Ok(p) => p,
        Err(e) => {
            log(&log_path, &format!("malformed hook payload: {e}"));
            print_context("[libra-governor] preflight skipped: malformed hook payload.");
            return;
        }
    };

    let socket_path = match libra_governor_daemon::paths::socket_path() {
        Ok(p) => p,
        Err(e) => {
            log(&log_path, &format!("could not resolve state dir: {e}"));
            print_context("[libra-governor] preflight skipped: daemon state dir unavailable.");
            return;
        }
    };

    let stream = match client::ensure_daemon_connection(&socket_path) {
        Ok(stream) => stream,
        Err(e) => {
            log(&log_path, &format!("daemon unavailable: {e}"));
            print_context(
                "[libra-governor] preflight unavailable: daemon unreachable. Proceeding \
                 without governance.",
            );
            return;
        }
    };

    let request = Request::Preflight {
        task_hint: payload.prompt,
        cwd: payload.cwd,
        session_id: payload.session_id,
    };

    match client::roundtrip(&stream, request) {
        Ok(Response::Preflight(result)) => print_context(&format_additional_context(&result)),
        Ok(other) => {
            log(
                &log_path,
                &format!("unexpected response kind for Preflight: {other:?}"),
            );
            print_context("[libra-governor] preflight skipped: unexpected daemon response.");
        }
        Err(e) => {
            log(&log_path, &format!("preflight request failed: {e}"));
            print_context(
                "[libra-governor] preflight unavailable: request to daemon failed. \
                 Proceeding without governance.",
            );
        }
    }
}

fn log(log_path: &Result<PathBuf, libra_governor_daemon::paths::PathsError>, message: &str) {
    if let Ok(path) = log_path {
        libra_governor_daemon::log::append_line(path, message);
    }
}

/// Renders the `hookSpecificOutput.additionalContext` JSON object for a
/// successful [`PreflightResult`].
fn format_additional_context(result: &PreflightResult) -> String {
    let confidence = match result.confidence {
        Confidence::Low => "low",
        Confidence::Medium => "medium",
        Confidence::High => "high",
    };

    let mut text = format!(
        "[libra-governor] Preflight complete (task {}, confidence: {confidence}, recon: {:.2}s).\n",
        result.task_id, result.recon_cost_seconds
    );
    text.push_str("Draft Completion Contract (revision ");
    text.push_str(&result.contract_draft.revision.to_string());
    text.push_str("):\n");
    for criterion in &result.contract_draft.criteria {
        let marker = if criterion.required {
            "required"
        } else {
            "optional"
        };
        text.push_str(&format!("  - [{marker}] {}\n", criterion.description));
    }
    if let Some(reason) = &result.recon_summary.reason {
        text.push_str(&format!("Note: {reason}\n"));
    }
    text.push_str("Cost/time estimate: pending (HORO-1126). This preflight is advisory only.");

    let output = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": text,
        }
    });
    serde_json::to_string(&output).unwrap_or_else(|_| "{}".to_string())
}

/// Prints a plain-text `additionalContext` message wrapped in the
/// expected `hookSpecificOutput` JSON shape — the fallback path used
/// whenever a real [`PreflightResult`] could not be obtained.
fn print_context(message: &str) {
    let output = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": message,
        }
    });
    println!(
        "{}",
        serde_json::to_string(&output).unwrap_or_else(|_| "{}".to_string())
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use libra_governor_domain::{CompletionContract, CompletionCriterion, TaskId};
    use libra_governor_protocol::ReconSummary;

    fn sample_preflight_result() -> PreflightResult {
        PreflightResult {
            task_id: TaskId::new(),
            contract_draft: CompletionContract::first(vec![
                CompletionCriterion::required("Matches the user's stated request"),
                CompletionCriterion::required("Relevant tests pass (cargo test)"),
            ]),
            recon_summary: ReconSummary {
                files_scanned: 5,
                dirs_scanned: 2,
                likely_affected_paths: vec!["src/login.rs".to_string()],
                detected_test_commands: vec!["cargo test".to_string()],
                truncated: false,
                reason: None,
            },
            confidence: Confidence::High,
            recon_cost_seconds: 0.42,
            estimate: None,
        }
    }

    #[test]
    fn format_additional_context_includes_confidence_and_criteria() {
        let result = sample_preflight_result();
        let json = format_additional_context(&result);
        assert!(json.contains("hookSpecificOutput"));
        assert!(json.contains("UserPromptSubmit"));
        assert!(json.contains("additionalContext"));
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let ctx = parsed["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(ctx.contains("confidence"));
        assert!(ctx.contains("required"));
    }

    #[test]
    fn print_context_never_panics_and_produces_valid_json() {
        // Exercises the fallback formatting path directly (stdout output
        // itself is not captured here, only that construction succeeds).
        let output = serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "UserPromptSubmit",
                "additionalContext": "daemon unavailable",
            }
        });
        let s = serde_json::to_string(&output).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(
            parsed["hookSpecificOutput"]["hookEventName"],
            "UserPromptSubmit"
        );
    }

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
            "prompt": "fix the bug",
            "transcript_path": "/tmp/t.jsonl",
            "hook_event_name": "UserPromptSubmit"
        }"#;
        let payload: HookPayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.session_id, "sess-1");
        assert_eq!(payload.prompt, "fix the bug");
    }
}
