//! [`normalize`] — total function turning one raw hook payload into a
//! [`NormalizedEvent`] for a given [`EntryPoint`] (HORO-1157).
//!
//! Never panics: malformed JSON or a missing required field is a
//! returned [`NormalizeError`], never a crash. An unexpected
//! `hook_event_name` inside an otherwise-valid payload is not an error
//! either — it becomes [`NormalizedEvent::RecognizedUnwired`] or
//! [`NormalizedEvent::Unrecognized`] (see that type's docs). Which
//! `EntryPoint` is calling is determined by which hook subcommand the
//! host invoked (`hook user-prompt-submit` / `codex-hook
//! user-prompt-submit`, ...), never guessed from payload content — so in
//! ordinary operation `hook_event_name` agrees with `entry` and this
//! function behaves exactly like the pre-HORO-1157 per-hook `serde_json`
//! parse it replaces (see `crates/cli/tests/agent_contract.rs`).

use super::event::NormalizedEvent;
use super::payload::{
    PromptSubmitPayload, ToolCompletedPayload, ToolRequestedPayload, TurnCompletedPayload,
};

/// Recognized agent lifecycle hook event names this integration does
/// not (yet) wire to any daemon action — see the HORO-1157 non-goals.
/// Each would need new protocol variants and ledger semantics nobody
/// has specified yet.
const KNOWN_UNWIRED_EVENTS: [&str; 7] = [
    "SessionStart",
    "SessionEnd",
    "SubagentStart",
    "SubagentStop",
    "Interrupt",
    "PreCompact",
    "PostCompact",
];

/// Which hook subcommand is calling [`normalize`] — determines the
/// payload shape/required fields expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryPoint {
    PromptSubmit,
    ToolRequested,
    ToolCompleted,
    TurnCompleted,
}

impl EntryPoint {
    fn canonical_event_name(self) -> &'static str {
        match self {
            EntryPoint::PromptSubmit => "UserPromptSubmit",
            EntryPoint::ToolRequested => "PreToolUse",
            EntryPoint::ToolCompleted => "PostToolUse",
            EntryPoint::TurnCompleted => "Stop",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NormalizeError {
    #[error("malformed hook payload: {0}")]
    Malformed(#[from] serde_json::Error),
}

/// A session id that can name a session: blank or whitespace-only is absent,
/// never a lane shared by every event that lacks one.
fn native_session(session_id: Option<String>) -> Option<String> {
    session_id.filter(|s| !s.trim().is_empty())
}

/// Parses `raw` into a [`NormalizedEvent`] for `entry`. See module docs.
pub fn normalize(entry: EntryPoint, raw: &str) -> Result<NormalizedEvent, NormalizeError> {
    let value: serde_json::Value = serde_json::from_str(raw)?;
    // serde's derived struct deserializers also accept a JSON array
    // positionally, so `["s-1"]` would otherwise decode as session `s-1`:
    // an identity read out of a payload that names no field at all.
    if !value.is_object() {
        return Err(<serde_json::Error as serde::de::Error>::custom(
            "hook payload must be a JSON object",
        )
        .into());
    }
    let hook_event_name = value.get("hook_event_name").and_then(|v| v.as_str());

    if let Some(name) = hook_event_name {
        if name != entry.canonical_event_name() {
            return Ok(if KNOWN_UNWIRED_EVENTS.contains(&name) {
                NormalizedEvent::RecognizedUnwired {
                    hook_event_name: name.to_string(),
                }
            } else {
                NormalizedEvent::Unrecognized {
                    hook_event_name: name.to_string(),
                }
            });
        }
    }

    match entry {
        EntryPoint::PromptSubmit => {
            let payload: PromptSubmitPayload = serde_json::from_value(value)?;
            Ok(NormalizedEvent::PromptSubmitted {
                session_id: native_session(payload.session_id),
                cwd: payload.cwd,
                prompt: payload.prompt,
                turn_id: payload.turn_id,
                agent_id: payload.agent_id,
                prompt_id: payload.prompt_id,
            })
        }
        EntryPoint::ToolRequested => {
            let payload: ToolRequestedPayload = serde_json::from_value(value)?;
            Ok(NormalizedEvent::ToolRequested {
                session_id: native_session(payload.session_id),
                agent_id: payload.agent_id,
            })
        }
        EntryPoint::ToolCompleted => {
            let payload: ToolCompletedPayload = serde_json::from_value(value)?;
            Ok(NormalizedEvent::ToolCompleted {
                session_id: native_session(payload.session_id),
                tool_name: payload.tool_name,
                native_call_id: payload.tool_use_id,
                turn_id: payload.turn_id,
                agent_id: payload.agent_id,
            })
        }
        EntryPoint::TurnCompleted => {
            let payload: TurnCompletedPayload = serde_json::from_value(value)?;
            Ok(NormalizedEvent::TurnCompleted {
                session_id: native_session(payload.session_id),
                model: payload.model,
                turn_id: payload.turn_id,
                agent_id: payload.agent_id,
                prompt_id: payload.prompt_id,
                transcript_path: payload.transcript_path,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_json_is_a_returned_error_not_a_panic() {
        let err = normalize(EntryPoint::PromptSubmit, "not json").unwrap_err();
        assert!(matches!(err, NormalizeError::Malformed(_)));
    }

    #[test]
    fn missing_required_field_is_a_returned_error_not_a_panic() {
        let err = normalize(EntryPoint::PromptSubmit, r#"{"session_id": "s"}"#).unwrap_err();
        assert!(matches!(err, NormalizeError::Malformed(_)));
    }

    #[test]
    fn matching_hook_event_name_parses_normally() {
        let json = r#"{
            "session_id": "s",
            "cwd": "/repo",
            "prompt": "fix it",
            "hook_event_name": "UserPromptSubmit"
        }"#;
        let event = normalize(EntryPoint::PromptSubmit, json).unwrap();
        assert_eq!(
            event,
            NormalizedEvent::PromptSubmitted {
                session_id: Some("s".to_string()),
                cwd: "/repo".into(),
                prompt: "fix it".to_string(),
                turn_id: None,
                agent_id: None,
                prompt_id: None,
            }
        );
    }

    #[test]
    fn absent_hook_event_name_parses_normally() {
        let json = r#"{"session_id": "s", "tool_name": "Bash"}"#;
        let event = normalize(EntryPoint::ToolCompleted, json).unwrap();
        assert_eq!(
            event,
            NormalizedEvent::ToolCompleted {
                session_id: Some("s".to_string()),
                tool_name: "Bash".to_string(),
                native_call_id: None,
                turn_id: None,
                agent_id: None,
            }
        );
    }

    #[test]
    fn a_known_but_unwired_lifecycle_event_name_is_recognized_not_an_error() {
        let json = r#"{"session_id": "s", "cwd": "/repo", "hook_event_name": "SessionStart"}"#;
        let event = normalize(EntryPoint::PromptSubmit, json).unwrap();
        assert_eq!(
            event,
            NormalizedEvent::RecognizedUnwired {
                hook_event_name: "SessionStart".to_string()
            }
        );
    }

    #[test]
    fn session_start_sources_are_recognized_as_unwired() {
        for source in ["startup", "resume", "clear", "compact", "fork"] {
            let json = format!(
                r#"{{"session_id":"fixture-session","hook_event_name":"SessionStart","source":"{source}"}}"#
            );
            let event = normalize(EntryPoint::PromptSubmit, &json).unwrap();
            assert_eq!(
                event,
                NormalizedEvent::RecognizedUnwired {
                    hook_event_name: "SessionStart".to_string()
                },
                "source {source} must not imply identity lifecycle behavior"
            );
        }
    }

    #[test]
    fn subagent_fields_do_not_turn_unwired_hooks_into_identity_events() {
        for hook_event_name in ["SubagentStart", "SubagentStop"] {
            let json = format!(
                r#"{{"session_id":"fixture-parent-session","hook_event_name":"{hook_event_name}","agent_id":"fixture-agent","turn_id":"fixture-turn","agent_type":"worker"}}"#
            );
            let event = normalize(EntryPoint::PromptSubmit, &json).unwrap();
            assert_eq!(
                event,
                NormalizedEvent::RecognizedUnwired {
                    hook_event_name: hook_event_name.to_string()
                }
            );
        }
    }

    #[test]
    fn every_known_unwired_lifecycle_name_stays_unwired() {
        for hook_event_name in [
            "SessionStart",
            "SessionEnd",
            "SubagentStart",
            "SubagentStop",
            "Interrupt",
            "PreCompact",
            "PostCompact",
        ] {
            let json = format!(
                r#"{{"session_id":"fixture-session","hook_event_name":"{hook_event_name}"}}"#
            );
            let event = normalize(EntryPoint::PromptSubmit, &json).unwrap();
            assert_eq!(
                event,
                NormalizedEvent::RecognizedUnwired {
                    hook_event_name: hook_event_name.to_string()
                }
            );
        }
    }

    #[test]
    fn a_genuinely_unknown_event_name_is_unrecognized_not_an_error() {
        let json = r#"{"session_id": "s", "cwd": "/repo", "hook_event_name": "SomeFutureEvent"}"#;
        let event = normalize(EntryPoint::PromptSubmit, json).unwrap();
        assert_eq!(
            event,
            NormalizedEvent::Unrecognized {
                hook_event_name: "SomeFutureEvent".to_string()
            }
        );
    }

    #[test]
    fn codex_turn_and_agent_id_flow_through_to_the_normalized_event() {
        let json = r#"{
            "session_id": "s",
            "cwd": "/repo",
            "prompt": "fix it",
            "hook_event_name": "UserPromptSubmit",
            "turn_id": "turn-1",
            "agent_id": "sub-1"
        }"#;
        let event = normalize(EntryPoint::PromptSubmit, json).unwrap();
        assert_eq!(
            event,
            NormalizedEvent::PromptSubmitted {
                session_id: Some("s".to_string()),
                cwd: "/repo".into(),
                prompt: "fix it".to_string(),
                turn_id: Some("turn-1".to_string()),
                agent_id: Some("sub-1".to_string()),
                prompt_id: None,
            }
        );
    }

    #[test]
    fn prompt_id_is_carried_only_when_the_host_supplies_a_string() {
        let with = r#"{"session_id":"s","cwd":"/r","prompt":"p","prompt_id":"pid-1"}"#;
        assert!(matches!(
            normalize(EntryPoint::PromptSubmit, with).unwrap(),
            NormalizedEvent::PromptSubmitted { prompt_id: Some(ref p), .. } if p == "pid-1"
        ));
        let junk = r#"{"session_id":"s","cwd":"/r","prompt":"p","prompt_id":7}"#;
        assert!(matches!(
            normalize(EntryPoint::PromptSubmit, junk).unwrap(),
            NormalizedEvent::PromptSubmitted {
                prompt_id: None,
                ..
            }
        ));
    }

    #[test]
    fn pre_tool_use_carries_session_and_optional_agent_id_only() {
        let main = r#"{"session_id":"s","hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"ls"}}"#;
        assert_eq!(
            normalize(EntryPoint::ToolRequested, main).unwrap(),
            NormalizedEvent::ToolRequested {
                session_id: Some("s".into()),
                agent_id: None
            }
        );
        let sub = r#"{"session_id":"s","agent_id":"sub-9","tool_name":"Bash"}"#;
        assert_eq!(
            normalize(EntryPoint::ToolRequested, sub).unwrap(),
            NormalizedEvent::ToolRequested {
                session_id: Some("s".into()),
                agent_id: Some("sub-9".into())
            }
        );
        assert_eq!(
            normalize(EntryPoint::ToolRequested, r#"{"tool_name":"Bash"}"#).unwrap(),
            NormalizedEvent::ToolRequested {
                session_id: None,
                agent_id: None
            }
        );
        // A different event name on this entry point is not a PreToolUse.
        let other = r#"{"session_id":"s","hook_event_name":"PostToolUse"}"#;
        assert!(matches!(
            normalize(EntryPoint::ToolRequested, other).unwrap(),
            NormalizedEvent::Unrecognized { .. }
        ));
    }

    #[test]
    fn a_non_object_payload_is_malformed_not_positionally_decoded() {
        for entry in [
            EntryPoint::PromptSubmit,
            EntryPoint::ToolCompleted,
            EntryPoint::TurnCompleted,
        ] {
            for raw in [r#"["s-1"]"#, r#"["s-1","/repo","fix it"]"#, r#""s-1""#, "7"] {
                let err = normalize(entry, raw).unwrap_err();
                assert!(
                    matches!(err, NormalizeError::Malformed(_)),
                    "{entry:?} {raw}"
                );
            }
        }
    }

    #[test]
    fn a_session_the_host_did_not_name_is_absent_never_coerced_or_shared() {
        for session in [
            "",
            r#","session_id":null"#,
            r#","session_id":123"#,
            r#","session_id":"""#,
            r#","session_id":"   ""#,
            r#","session_id":["s-1"]"#,
            r#","session_id":{"id":"s-1"}"#,
        ] {
            let raw = if session.is_empty() {
                r#"{"cwd":"/r","prompt":"p","tool_name":"Bash","agent_id":"a-1"}"#.to_string()
            } else {
                format!(
                    r#"{{"cwd":"/r","prompt":"p","tool_name":"Bash","agent_id":"a-1"{session}}}"#
                )
            };
            for entry in [
                EntryPoint::PromptSubmit,
                EntryPoint::ToolRequested,
                EntryPoint::ToolCompleted,
                EntryPoint::TurnCompleted,
            ] {
                let event = normalize(entry, &raw).unwrap();
                let session_id = match event {
                    NormalizedEvent::PromptSubmitted {
                        session_id,
                        agent_id,
                        ..
                    }
                    | NormalizedEvent::ToolCompleted {
                        session_id,
                        agent_id,
                        ..
                    }
                    | NormalizedEvent::ToolRequested {
                        session_id,
                        agent_id,
                    }
                    | NormalizedEvent::TurnCompleted {
                        session_id,
                        agent_id,
                        ..
                    } => {
                        assert_eq!(agent_id.as_deref(), Some("a-1"), "{raw}");
                        session_id
                    }
                    other => panic!("{raw}: {other:?}"),
                };
                assert_eq!(session_id, None, "{entry:?} {raw}");
            }
        }
    }

    #[test]
    fn a_padded_session_is_kept_verbatim() {
        let event = normalize(
            EntryPoint::ToolRequested,
            r#"{"session_id":" s 1 ","tool_name":"Bash"}"#,
        )
        .unwrap();
        assert_eq!(
            event,
            NormalizedEvent::ToolRequested {
                session_id: Some(" s 1 ".into()),
                agent_id: None
            }
        );
    }

    #[test]
    fn a_blank_agent_id_is_carried_as_blank_not_dropped() {
        // The legacy route must be able to tell "no agent" from "an agent
        // we cannot name".
        let event = normalize(
            EntryPoint::ToolCompleted,
            r#"{"session_id":"s","tool_name":"Bash","agent_id":"  "}"#,
        )
        .unwrap();
        assert!(matches!(
            event,
            NormalizedEvent::ToolCompleted { agent_id: Some(ref a), .. } if a == "  "
        ));
    }
}
