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
//! `last_assistant_message`, `stop_hook_active`) and any other host
//! extras are ignored, never rejected.
//!
//! `transcript_path` used to be in that ignored list. It is captured on
//! the `Stop` payload now (HORO-1725): the host writes per-turn
//! `input_tokens`/`cache_creation_input_tokens`/
//! `cache_read_input_tokens`/`output_tokens` into that file, which is the
//! only source of *measured* resource usage available without a gateway.
//! Treating it as an irrelevant extra is what left every receipt's
//! `actual_usage` empty and every task's reservation pinned to the same
//! policy-target constant. This crate only relays the path — it never
//! opens the file; see `libra_governor_daemon::usage` for why the read
//! belongs to the daemon and what it extracts.
//!
//! `turn_id`/`agent_id` (HORO-1599) are the two of those extras this
//! integration now actually captures, for the shared execution identity
//! envelope (`libra_governor_domain::ExecutionIdentity`) — per
//! `docs/adr/0004-agent-adapter-contract.md`'s verified real schema,
//! both are Codex-only fields today; Claude Code's own verified hook
//! payload shape does not expose either, so they are always absent for
//! Claude Code (`Option<String>`, never defaulted to a guessed value).
//! No `parent_agent_id`-shaped field is documented in either host's
//! schema, so lineage is always `LineageStatus::Unknown` for both hosts
//! today — see `crates/cli/src/agent/identity.rs`.

use std::path::PathBuf;

use serde::Deserialize;

/// `UserPromptSubmit` payload.
#[derive(Debug, Deserialize)]
pub struct PromptSubmitPayload {
    pub session_id: String,
    pub cwd: PathBuf,
    pub prompt: String,
    #[serde(default)]
    pub turn_id: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
}

/// `PostToolUse` payload.
#[derive(Debug, Deserialize)]
pub struct ToolCompletedPayload {
    pub session_id: String,
    pub tool_name: String,
    #[serde(default)]
    pub turn_id: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Codex's native tool call identifier; absent in Claude payloads and
    /// retained verbatim as canonical provenance when supplied.
    #[serde(default, deserialize_with = "optional_native_call_id")]
    pub tool_use_id: Option<String>,
}

/// `Stop` payload. `model` is optional because it is not guaranteed
/// present on every host/version (Claude Code's own `Stop` payload does
/// expose it; treated the same way for Codex until proven otherwise).
#[derive(Debug, Deserialize)]
pub struct TurnCompletedPayload {
    pub session_id: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub turn_id: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Claude Code's path to this session's transcript. Absent for Codex,
    /// whose `Stop` payload documents no equivalent — so it stays `None`
    /// rather than being guessed at, exactly as `model`/`turn_id` do.
    #[serde(default)]
    pub transcript_path: Option<PathBuf>,
}

fn optional_native_call_id<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value.as_str().map(str::to_owned))
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
        assert_eq!(payload.tool_use_id.as_deref(), Some("call-1"));
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

    #[test]
    fn prompt_submit_payload_captures_codex_turn_and_agent_id() {
        let json = r#"{
            "session_id": "sess-1",
            "cwd": "/repo",
            "prompt": "fix the bug",
            "turn_id": "turn-1",
            "agent_id": "sub-1"
        }"#;
        let payload: PromptSubmitPayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.turn_id.as_deref(), Some("turn-1"));
        assert_eq!(payload.agent_id.as_deref(), Some("sub-1"));
    }

    #[test]
    fn prompt_submit_payload_leaves_turn_and_agent_id_absent_when_the_host_sends_neither() {
        // Claude Code's verified real hook payload shape does not expose
        // either field (docs/adr/0004-agent-adapter-contract.md) — this
        // must be an explicit absence, never a guessed/defaulted value.
        let json = r#"{"session_id": "sess-1", "cwd": "/repo", "prompt": "fix the bug"}"#;
        let payload: PromptSubmitPayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.turn_id, None);
        assert_eq!(payload.agent_id, None);
    }

    #[test]
    fn tool_completed_payload_captures_codex_turn_and_agent_id() {
        let json = r#"{"session_id": "sess-1", "tool_name": "Bash", "turn_id": "turn-1", "agent_id": "sub-1"}"#;
        let payload: ToolCompletedPayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.turn_id.as_deref(), Some("turn-1"));
        assert_eq!(payload.agent_id.as_deref(), Some("sub-1"));
    }

    #[test]
    fn turn_completed_payload_captures_codex_turn_and_agent_id() {
        let json = r#"{"session_id": "sess-1", "turn_id": "turn-1", "agent_id": "sub-1"}"#;
        let payload: TurnCompletedPayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.turn_id.as_deref(), Some("turn-1"));
        assert_eq!(payload.agent_id.as_deref(), Some("sub-1"));
    }
}
