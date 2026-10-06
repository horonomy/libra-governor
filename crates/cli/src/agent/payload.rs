//! Shared hook payload structs serving both Claude Code and Codex
//! (HORO-1157).
//!
//! Both hosts use `session_id` and the wired event fields normalized
//! below. Host- and event-specific fields stay optional, and unknown
//! fields are ignored (`#[serde(deny_unknown_fields)]` is never used).
//! Current documented schemas differ: Codex supplies `turn_id` on its
//! turn-scoped hooks; Claude Code documents `agent_id` for hooks running
//! with `--agent` or inside a subagent; both document `tool_use_id` on
//! `PostToolUse` and `transcript_path` on hook inputs. See
//! `docs/native-agent-lifecycle-characterization.md` for the current
//! evidence boundary and source links.
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
//! `turn_id`/`agent_id` (HORO-1599) are retained for the shared execution
//! identity envelope (`libra_governor_domain::ExecutionIdentity`) only
//! when an input actually supplies them. The current Codex schema
//! documents `turn_id`; both hosts document `agent_id` in subagent
//! contexts. Missing values remain absent. Neither host documents a
//! `parent_agent_id` field, so captured lineage remains
//! `LineageStatus::Unknown`; see `crates/cli/src/agent/identity.rs`.

use std::path::PathBuf;

use serde::Deserialize;

/// `UserPromptSubmit` payload.
#[derive(Debug, Deserialize)]
pub struct PromptSubmitPayload {
    pub session_id: String,
    pub cwd: PathBuf,
    pub prompt: String,
    /// Codex turn ID where supplied. Claude Code does not document this
    /// field in its hook payloads.
    #[serde(default)]
    pub turn_id: Option<String>,
    /// Optional subagent ID; current Claude Code and Codex hook schemas
    /// expose it in subagent contexts.
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
    /// Host-native tool call identifier, documented by both current
    /// schemas; retained verbatim as canonical provenance when supplied.
    #[serde(default, deserialize_with = "optional_native_call_id")]
    pub tool_use_id: Option<String>,
}

/// `Stop` payload. Codex documents `model` here; Claude Code currently
/// documents it only for `SessionStart`, so it remains optional.
#[derive(Debug, Deserialize)]
pub struct TurnCompletedPayload {
    pub session_id: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub turn_id: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Optional host transcript path. This crate relays it but does not
    /// parse it as a lineage or identity source.
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
        // Optional IDs remain absent when the host omits them. In
        // particular, this event does not invent a turn or agent ID.
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
