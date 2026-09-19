//! Shared hook payload structs serving both Claude Code and Codex
//! (HORO-1157).
//!
//! Verified field-name-compatible between the two hosts — Codex's real
//! hook payload schemas name the same fields Claude Code's do for
//! `UserPromptSubmit`/`PostToolUse`/`Stop` (see
//! `docs/adr/0004-agent-adapter-contract.md`). Every field either host
//! might omit is `Option`/`#[serde(default)]`, and nothing here uses
//! `#[serde(deny_unknown_fields)]`: Codex's extra fields (`turn_id`,
//! `permission_mode`, `tool_use_id`, `agent_id`, `agent_type`,
//! `last_assistant_message`, `stop_hook_active`) and Claude's own extras
//! (`transcript_path`) are ignored, never rejected.

use std::path::PathBuf;

use serde::Deserialize;

/// `UserPromptSubmit` payload.
#[derive(Debug, Deserialize)]
pub struct PromptSubmitPayload {
    pub session_id: String,
    pub cwd: PathBuf,
    pub prompt: String,
}

/// `PostToolUse` payload.
#[derive(Debug, Deserialize)]
pub struct ToolCompletedPayload {
    pub session_id: String,
    pub tool_name: String,
}

/// `Stop` payload. `model` is optional because it is not guaranteed
/// present on every host/version (Claude Code's own `Stop` payload does
/// expose it; treated the same way for Codex until proven otherwise).
#[derive(Debug, Deserialize)]
pub struct TurnCompletedPayload {
    pub session_id: String,
    #[serde(default)]
    pub model: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_submit_payload_fails_to_deserialize_malformed_json() {
        let result: Result<PromptSubmitPayload, _> = serde_json::from_str("not json");
        assert!(result.is_err());
    }

    #[test]
    fn prompt_submit_payload_ignores_unknown_fields_from_either_host() {
        let json = r#"{
            "session_id": "sess-1",
            "cwd": "/repo",
            "prompt": "fix the bug",
            "transcript_path": "/tmp/t.jsonl",
            "hook_event_name": "UserPromptSubmit",
            "turn_id": "turn-1",
            "permission_mode": "auto",
            "agent_id": "sub-1",
            "agent_type": "reviewer"
        }"#;
        let payload: PromptSubmitPayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.session_id, "sess-1");
        assert_eq!(payload.prompt, "fix the bug");
    }

    #[test]
    fn tool_completed_payload_ignores_unknown_fields_from_either_host() {
        let json = r#"{
            "session_id": "sess-1",
            "cwd": "/repo",
            "tool_name": "Bash",
            "tool_input": {"command": "ls"},
            "tool_response": "some output",
            "tool_use_id": "call-1",
            "hook_event_name": "PostToolUse"
        }"#;
        let payload: ToolCompletedPayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.session_id, "sess-1");
        assert_eq!(payload.tool_name, "Bash");
    }

    #[test]
    fn turn_completed_payload_ignores_unknown_fields_and_missing_model() {
        let json = r#"{
            "session_id": "sess-1",
            "cwd": "/repo",
            "hook_event_name": "Stop",
            "stop_hook_active": false,
            "last_assistant_message": "done"
        }"#;
        let payload: TurnCompletedPayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.session_id, "sess-1");
        assert_eq!(payload.model, None);
    }

    #[test]
    fn turn_completed_payload_parses_model_when_present() {
        let json = r#"{"session_id": "sess-1", "model": "claude-sonnet-5"}"#;
        let payload: TurnCompletedPayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.model.as_deref(), Some("claude-sonnet-5"));
    }
}
