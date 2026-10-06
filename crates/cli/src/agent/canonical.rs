//! Canonical bridge for the three currently wired built-in hook entry points.
//!
//! This is pure local normalization. The returned context is transient product
//! input; it is never added to canonical lifecycle facts or persisted here.

mod binding;
pub use binding::HostBindingOutcome;
use libra_governor_daemon::host_binding::LibraNativeContext;
use libra_governor_domain::AgentKind;
use libra_governor_protocol::{
    validate_host_event, validate_host_json, HostBindingFailure, HostBindingReason,
    HostBindingStage, HostEventKind, HostEventScope, ValidatedHostCapabilitySnapshot,
    ValidatedHostEvent,
};
use serde_json::{json, Map, Value};
use time::OffsetDateTime;
use uuid::Uuid;

use super::event::NormalizedEvent;
use super::identity;
use super::normalize::{normalize, EntryPoint};

/// Runtime-selected package facts plus a validated public snapshot reference.
/// This is local context only; it does not make the snapshot authoritative.
#[derive(Debug, Clone)]
pub struct BuiltinHostContext {
    pub adapter_id: String,
    pub adapter_version: String,
    pub host_id: String,
    pub host_version: Option<String>,
    pub scope: HostEventScope,
    pub snapshot: ValidatedHostCapabilitySnapshot,
}

#[derive(Debug, Clone)]
pub struct NormalizedHostInput {
    event: ValidatedHostEvent,
    native_context: LibraNativeContext,
}

impl NormalizedHostInput {
    pub fn event(&self) -> &ValidatedHostEvent {
        &self.event
    }
    pub fn bind(self) -> HostBindingOutcome {
        binding::bind(self)
    }
}

pub fn normalize_builtin(
    agent: AgentKind,
    entry_point: EntryPoint,
    native_bytes: &[u8],
    context: &BuiltinHostContext,
) -> Result<NormalizedHostInput, HostBindingFailure> {
    let snapshot = context.snapshot.snapshot();
    let expected_adapter = match agent {
        AgentKind::ClaudeCode => "claude_code",
        AgentKind::Codex => "codex",
    };
    if context.adapter_id != expected_adapter
        || snapshot.adapter.id != context.adapter_id
        || snapshot.adapter.version != context.adapter_version
        || snapshot.host.tool_provider != agent.as_tool_provider()
        || snapshot.host.version != context.host_version
        || snapshot.context.scope != context.scope
    {
        return Err(HostBindingFailure {
            stage: HostBindingStage::Context,
            reason: HostBindingReason::ContextMismatch,
        });
    }
    let strict_native = validate_host_json(native_bytes)?;
    if strict_native
        .get("tool_use_id")
        .is_some_and(|v| !v.is_string())
    {
        return Err(failure_invalid_native());
    }
    let native = std::str::from_utf8(native_bytes).map_err(|_| failure_invalid_native())?;
    let normalized = normalize(entry_point, native).map_err(|_| failure_invalid_native())?;
    let source_name = entry_point_name(entry_point);
    let now = OffsetDateTime::now_utc();

    let (kind, facts, native_context, session_id, agent_id, turn_id, provenance) = match normalized
    {
        NormalizedEvent::PromptSubmitted {
            session_id,
            cwd,
            prompt,
            turn_id,
            agent_id,
        } => (
            HostEventKind::Lifecycle,
            json!({"event_type": "turn_start"}),
            LibraNativeContext::PromptAdmission {
                task_hint: prompt,
                cwd,
            },
            session_id,
            agent_id,
            turn_id,
            json!({"facts.event_type": "native_hook_mapping"}),
        ),
        NormalizedEvent::ToolCompleted {
            session_id,
            tool_name,
            native_call_id,
            turn_id,
            agent_id,
        } => {
            let facts = tool_facts(tool_name, native_call_id);
            let mut provenance = Map::new();
            provenance.insert("facts.tool_name".into(), json!("native_payload.tool_name"));
            if facts.get("native_call_id").is_some() {
                provenance.insert(
                    "facts.native_call_id".into(),
                    json!("native_payload.tool_use_id"),
                );
            }
            (
                HostEventKind::ToolAfter,
                facts,
                LibraNativeContext::None,
                session_id,
                agent_id,
                turn_id,
                Value::Object(provenance),
            )
        }
        NormalizedEvent::TurnCompleted {
            session_id,
            model,
            turn_id,
            agent_id,
            // This inactive projection does not replace the live daemon usage relay.
            transcript_path: _,
        } => (
            HostEventKind::Lifecycle,
            json!({"event_type": "turn_end"}),
            LibraNativeContext::Completion { model },
            session_id,
            agent_id,
            turn_id,
            json!({"facts.event_type": "native_hook_mapping"}),
        ),
        NormalizedEvent::RecognizedUnwired { .. } | NormalizedEvent::Unrecognized { .. } => {
            return Err(HostBindingFailure {
                stage: HostBindingStage::Event,
                reason: HostBindingReason::UnsupportedNativeEvent,
            });
        }
    };
    let identity = identity::capture(
        &context.host_id,
        agent,
        &session_id,
        agent_id.as_deref(),
        turn_id.as_deref(),
        now,
    )
    .map_err(|_| HostBindingFailure {
        stage: HostBindingStage::Event,
        reason: HostBindingReason::InvalidIdentity,
    })?;
    let identity_value = serde_json::to_value(identity).map_err(|_| failure_invalid_native())?;
    let observed_at = identity_value
        .get("observed_at")
        .and_then(Value::as_str)
        .ok_or_else(failure_invalid_native)?;
    let mut source = Map::new();
    source.insert("kind".into(), json!("hook"));
    source.insert("native_event_name".into(), json!(source_name));

    let mut event = json!({
        "schema_version": 1,
        "event_id": Uuid::new_v4().to_string(),
        "observed_at": observed_at,
        "adapter_id": context.adapter_id,
        "adapter_version": context.adapter_version,
        "source": Value::Object(source),
        "capability_snapshot_id": snapshot.snapshot_id,
        "identity": identity_value,
        "scope": context.scope,
        "kind": kind,
        "facts": facts,
        "quality": "reconstructed",
        "field_provenance": provenance,
    });
    if let Some(host_version) = &context.host_version {
        event
            .as_object_mut()
            .expect("event constructed as object")
            .insert("host_version".into(), json!(host_version));
    }
    let event = validate_host_event(event.to_string().as_bytes())?;
    Ok(NormalizedHostInput {
        event,
        native_context,
    })
}

fn tool_facts(tool_name: String, native_call_id: Option<String>) -> Value {
    let mut facts = Map::new();
    facts.insert("tool_name".into(), json!(tool_name));
    if let Some(native_call_id) = native_call_id {
        facts.insert("native_call_id".into(), json!(native_call_id));
    }
    Value::Object(facts)
}

fn entry_point_name(entry: EntryPoint) -> &'static str {
    match entry {
        EntryPoint::PromptSubmit => "UserPromptSubmit",
        EntryPoint::ToolCompleted => "PostToolUse",
        EntryPoint::TurnCompleted => "Stop",
    }
}

fn failure_invalid_native() -> HostBindingFailure {
    HostBindingFailure {
        stage: HostBindingStage::Event,
        reason: HostBindingReason::InvalidEvent,
    }
}

#[cfg(test)]
mod tests {
    use super::binding::RecordOnlyReason;
    use super::*;
    use libra_governor_protocol::{validate_host_snapshot, HostEventScope};
    use std::path::PathBuf;

    fn context(agent: AgentKind) -> BuiltinHostContext {
        let (adapter_id, provider) = match agent {
            AgentKind::ClaudeCode => ("claude_code", "claude_code"),
            AgentKind::Codex => ("codex", "codex"),
        };
        let snapshot = json!({
            "schema_version": 1,
            "snapshot_id": "snapshot-local",
            "observed_at": "2026-10-04T23:59:59.000Z",
            "adapter": {
                "id": adapter_id,
                "version": "1.0.0",
                "manifest_digest": format!("sha256:{}", "a".repeat(64)),
                "implementation_digest": format!("sha256:{}", "b".repeat(64))
            },
            "host": {"tool_provider": provider, "version": null, "version_source": "unknown"},
            "context": {"scope": "turn_task"},
            "lifecycle": {
                "registered": true,
                "installed": true,
                "enabled": false,
                "adapter_trust": "unknown",
                "host_trust": "unknown"
            },
            "capabilities": []
        });
        BuiltinHostContext {
            adapter_id: adapter_id.into(),
            adapter_version: "1.0.0".into(),
            host_id: "host-local-test".into(),
            host_version: None,
            scope: HostEventScope::TurnTask,
            snapshot: validate_host_snapshot(snapshot.to_string().as_bytes()).unwrap(),
        }
    }

    fn add_raw_member(native: &[u8], member: &str) -> Vec<u8> {
        let text = std::str::from_utf8(native).unwrap().trim_end();
        let object = text.strip_suffix('}').expect("fixture root is an object");
        format!("{object}, {member}}}").into_bytes()
    }

    fn assert_json_rejection(native: &[u8], reason: HostBindingReason) {
        let failure = normalize_builtin(
            AgentKind::Codex,
            EntryPoint::PromptSubmit,
            native,
            &context(AgentKind::Codex),
        )
        .unwrap_err();
        assert_eq!(failure.stage, HostBindingStage::Json);
        assert_eq!(failure.reason, reason);
    }

    #[test]
    fn existing_prompt_payloads_use_the_real_decoder_and_keep_context_out_of_event() {
        let codex = include_bytes!("../../tests/fixtures/agent/codex/user-prompt-submit.json");
        let legacy = normalize(
            EntryPoint::PromptSubmit,
            std::str::from_utf8(codex).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            legacy,
            NormalizedEvent::PromptSubmitted { ref session_id, ref prompt, .. }
                if session_id == "fixture-session" && prompt == "fix the login bug"
        ));
        let result = normalize_builtin(
            AgentKind::Codex,
            EntryPoint::PromptSubmit,
            codex,
            &context(AgentKind::Codex),
        )
        .unwrap();
        assert_eq!(result.event.source().native_event_name, "UserPromptSubmit");
        assert_eq!(
            result.event.identity().provider_session_id(),
            Some("fixture-session")
        );
        assert_eq!(result.event.identity().turn_id(), Some("turn-1"));
        assert_eq!(result.event.identity().agent_id(), None);
        match &result.native_context {
            LibraNativeContext::PromptAdmission { task_hint, cwd } => {
                assert_eq!(task_hint, "fix the login bug");
                assert_eq!(cwd, &PathBuf::from("__CWD__"));
            }
            other => panic!("unexpected native context: {other:?}"),
        }
        let encoded = serde_json::to_string(&result.event).unwrap();
        assert!(!encoded.contains("fix the login bug"));
        assert!(!encoded.contains("__CWD__"));

        let claude =
            include_bytes!("../../tests/fixtures/agent/claude-code/user-prompt-submit.json");
        let result = normalize_builtin(
            AgentKind::ClaudeCode,
            EntryPoint::PromptSubmit,
            claude,
            &context(AgentKind::ClaudeCode),
        )
        .unwrap();
        assert_eq!(result.event.adapter_id(), "claude_code");
        assert_eq!(result.event.identity().agent_id(), None);
        assert_eq!(
            result.event.identity().lineage_status(),
            libra_governor_domain::LineageStatus::Unknown
        );
    }

    #[test]
    fn codex_tool_call_id_and_stop_model_remain_native_side_facts() {
        let tool = include_bytes!("../../tests/fixtures/agent/codex/post-tool-use.json");
        let result = normalize_builtin(
            AgentKind::Codex,
            EntryPoint::ToolCompleted,
            tool,
            &context(AgentKind::Codex),
        )
        .unwrap();
        assert_eq!(result.event.kind(), HostEventKind::ToolAfter);
        assert_eq!(result.event.facts()["tool_name"], "Bash");
        assert_eq!(result.event.facts()["native_call_id"], "call-1");
        assert!(result.event.facts().get("operands").is_none());

        let stop = include_bytes!("../../tests/fixtures/agent/codex/stop.json");
        let result = normalize_builtin(
            AgentKind::Codex,
            EntryPoint::TurnCompleted,
            stop,
            &context(AgentKind::Codex),
        )
        .unwrap();
        assert_eq!(result.event.facts()["event_type"], "turn_end");
        match &result.native_context {
            LibraNativeContext::Completion { model } => {
                assert_eq!(model.as_deref(), Some("gpt-5-codex"))
            }
            other => panic!("unexpected native context: {other:?}"),
        }
        let encoded = serde_json::to_string(&result.event).unwrap();
        assert!(!encoded.contains("gpt-5-codex"));
        assert!(!encoded.contains("last_assistant_message"));
    }

    #[test]
    fn stop_transcript_path_stays_in_the_live_relay_not_the_inactive_projection() {
        let native = br#"{"hook_event_name":"Stop","session_id":"native-session","model":"observed-model","transcript_path":"/private/native-transcript.jsonl"}"#;
        let legacy = normalize(
            EntryPoint::TurnCompleted,
            std::str::from_utf8(native).unwrap(),
        )
        .unwrap();
        assert!(matches!(legacy, NormalizedEvent::TurnCompleted {
            transcript_path: Some(ref path), ..
        } if path == &std::path::PathBuf::from("/private/native-transcript.jsonl")));
        let bundle = normalize_builtin(
            AgentKind::ClaudeCode,
            EntryPoint::TurnCompleted,
            native,
            &context(AgentKind::ClaudeCode),
        )
        .unwrap();
        let encoded = serde_json::to_string(bundle.event()).unwrap();
        assert!(!encoded.contains("transcript"));
        assert!(matches!(
            bundle.bind(),
            HostBindingOutcome::RecordOnly { .. }
        ));
    }

    #[test]
    fn invalid_context_and_unwired_native_events_do_not_create_canonical_success() {
        let native = include_bytes!("../../tests/fixtures/agent/codex/user-prompt-submit.json");
        let mut mismatched_context = context(AgentKind::Codex);
        mismatched_context.scope = HostEventScope::Unknown;
        let failure = normalize_builtin(
            AgentKind::Codex,
            EntryPoint::PromptSubmit,
            native,
            &mismatched_context,
        )
        .unwrap_err();
        assert_eq!(failure.reason, HostBindingReason::ContextMismatch);

        let unwired =
            include_bytes!("../../tests/fixtures/agent/recognized-unwired-user-prompt-submit.json");
        let failure = normalize_builtin(
            AgentKind::Codex,
            EntryPoint::PromptSubmit,
            unwired,
            &context(AgentKind::Codex),
        )
        .unwrap_err();
        assert_eq!(failure.reason, HostBindingReason::UnsupportedNativeEvent);
    }

    #[test]
    fn bounded_json_preparse_rejects_size_depth_nodes_utf8_and_nonfinite_before_normalization() {
        use libra_governor_protocol::HostBindingReason as Reason;

        let fixture = include_bytes!("../../tests/fixtures/agent/codex/user-prompt-submit.json");

        let mut oversized = fixture.to_vec();
        oversized.resize(fixture.len() + 65_537, b' ');
        assert_json_rejection(&oversized, Reason::InputTooLarge);

        let mut deep: Value = serde_json::from_slice(fixture).unwrap();
        let mut nested = json!(null);
        for _ in 0..20 {
            nested = json!([nested]);
        }
        deep["ignored_deep_value"] = nested;
        assert_json_rejection(
            serde_json::to_string(&deep).unwrap().as_bytes(),
            Reason::InputTooDeep,
        );

        let mut many_nodes: Value = serde_json::from_slice(fixture).unwrap();
        many_nodes["ignored_many_nodes"] = json!((0..5_000).collect::<Vec<_>>());
        assert_json_rejection(
            serde_json::to_string(&many_nodes).unwrap().as_bytes(),
            Reason::TooManyNodes,
        );

        let mut invalid_utf8 = fixture.to_vec();
        invalid_utf8.push(0xff);
        assert_json_rejection(&invalid_utf8, Reason::MalformedJson);

        let nonfinite = add_raw_member(fixture, "\"ignored_nonfinite\": NaN");
        assert_json_rejection(&nonfinite, Reason::MalformedJson);
    }

    #[test]
    fn nested_duplicate_keys_fail_canonical_preparse_while_legacy_decode_remains_compatible() {
        use libra_governor_protocol::HostBindingReason as Reason;

        let fixture = include_bytes!("../../tests/fixtures/agent/codex/user-prompt-submit.json");
        let duplicate = add_raw_member(fixture, "\"ignored_metadata\": {\"tag\": 1, \"tag\": 2}");

        // The older typed normalizer has historically consumed serde_json::Value,
        // which applies last-value-wins to duplicate keys in ignored metadata.
        let legacy = normalize(
            EntryPoint::PromptSubmit,
            std::str::from_utf8(&duplicate).unwrap(),
        )
        .unwrap();
        assert!(matches!(legacy, NormalizedEvent::PromptSubmitted { .. }));
        assert_json_rejection(&duplicate, Reason::DuplicateJsonKey);
    }

    #[test]
    fn non_string_tool_use_ids_cannot_produce_canonical_success() {
        let fixture = include_bytes!("../../tests/fixtures/agent/codex/post-tool-use.json");
        let fixture_text = std::str::from_utf8(fixture).unwrap();

        for wrong_value in ["true", "17", "null"] {
            let native = fixture_text.replace(
                "\"tool_use_id\": \"call-1\"",
                &format!("\"tool_use_id\": {wrong_value}"),
            );
            assert!(!native.is_empty());
            let outcome = normalize_builtin(
                AgentKind::Codex,
                EntryPoint::ToolCompleted,
                native.as_bytes(),
                &context(AgentKind::Codex),
            );
            let failure = outcome.expect_err("malformed native call identifiers must not bind");
            assert_eq!(failure.stage, HostBindingStage::Event);
            assert_eq!(failure.reason, HostBindingReason::InvalidEvent);
        }
    }
    #[test]
    fn intact_native_bundle_cannot_substitute_a_same_id_changed_observation() {
        let native = include_bytes!("../../tests/fixtures/agent/codex/post-tool-use.json");
        let a = normalize_builtin(
            AgentKind::Codex,
            EntryPoint::ToolCompleted,
            native,
            &context(AgentKind::Codex),
        )
        .unwrap();
        let original = serde_json::to_value(a.event()).unwrap();
        let mut changed = original.clone();
        changed["identity"]["agent_id"] = json!("different-agent");
        changed["facts"]["native_call_id"] = json!("different-call");
        let b = validate_host_event(changed.to_string().as_bytes()).unwrap();
        assert_eq!(a.event().event_id(), b.event_id());
        assert_ne!(a.event().identity().agent_id(), b.identity().agent_id());
        assert_ne!(a.event().facts(), b.facts());
        // The supported API accepts no replacement event or detached context.
        // Cloning/read-only inspection leaves the complete original bundle intact.
        for outcome in [a.clone().bind(), a.bind()] {
            match outcome {
                HostBindingOutcome::RecordOnly {
                    event,
                    reason: RecordOnlyReason::MissingAttributionIdentity,
                } => {
                    assert_eq!(serde_json::to_value(event).unwrap(), original);
                }
                other => panic!("unknown native lineage must remain record-only: {other:?}"),
            }
        }
    }
}
