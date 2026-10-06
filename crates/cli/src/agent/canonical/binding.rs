//! Concrete built-in native-name correlation around Libra's shared pure binder.

use super::{LibraNativeContext, NormalizedHostInput};
use libra_governor_protocol::{
    HostBindingFailure, HostBindingReason, HostBindingStage, HostEventKind, HostEventScope,
};

#[cfg(test)]
use libra_governor_protocol::ValidatedHostEvent;

pub use libra_governor_daemon::host_binding::HostBindingOutcome;
#[cfg(test)]
pub use libra_governor_daemon::host_binding::{BindingCandidate, RecordOnlyReason};

pub(super) fn bind(input: NormalizedHostInput) -> HostBindingOutcome {
    let NormalizedHostInput {
        event,
        native_context,
    } = input;
    // Preserve the old reducer's precedence: an unknown scope remains
    // record-only before any native-name mismatch is considered.
    if event.scope() == HostEventScope::Unknown {
        return libra_governor_daemon::host_binding::bind(event, native_context);
    }

    if event.kind() == HostEventKind::Lifecycle {
        match (
            event
                .facts()
                .get("event_type")
                .and_then(|value| value.as_str()),
            &native_context,
        ) {
            (Some("turn_start"), LibraNativeContext::PromptAdmission { .. })
                if event.source().native_event_name != "UserPromptSubmit" =>
            {
                return rejected(HostBindingReason::ContextMismatch);
            }
            (Some("turn_end"), LibraNativeContext::Completion { .. })
                if event.source().native_event_name != "Stop" =>
            {
                return rejected(HostBindingReason::ContextMismatch);
            }
            _ => {}
        }
    }

    // The concrete built-in wrapper requires its actual PostToolUse name.
    // Generic external providers reach the daemon binder without this rule.
    if event.kind() == HostEventKind::ToolAfter
        && (event.source().native_event_name != "PostToolUse"
            || !matches!(&native_context, LibraNativeContext::None))
    {
        return rejected(HostBindingReason::ContextMismatch);
    }
    libra_governor_daemon::host_binding::bind(event, native_context)
}

fn rejected(reason: HostBindingReason) -> HostBindingOutcome {
    HostBindingOutcome::Rejected(HostBindingFailure {
        stage: HostBindingStage::Context,
        reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use libra_governor_protocol::validate_host_event;
    use serde_json::json;
    use std::path::PathBuf;

    const OBSERVED_AT: &str = "2026-10-05T00:00:00.000Z";

    fn event(
        kind: &str,
        facts: serde_json::Value,
        lineage: &str,
        scope: &str,
        native_event_name: &str,
    ) -> ValidatedHostEvent {
        let mut identity = json!({
            "envelope_version": 1,
            "observed_at": OBSERVED_AT,
            "host_id": "host-local",
            "tool_provider": "codex",
            "provider_session_id": "session-native",
            "agent_id": "agent-native",
            "turn_id": "turn-native",
            "lineage_status": lineage
        });
        if lineage == "child" {
            identity["parent_agent_id"] = json!("parent-native");
        }
        let value = json!({
            "schema_version": 1,
            "event_id": "observation-1",
            "observed_at": OBSERVED_AT,
            "adapter_id": "codex",
            "adapter_version": "1.0.0",
            "source": {"kind":"hook", "native_event_name":native_event_name},
            "capability_snapshot_id": "snapshot-1",
            "identity": identity,
            "scope": scope,
            "kind": kind,
            "facts": facts,
            "quality": "reconstructed",
            "field_provenance": {}
        });
        validate_host_event(value.to_string().as_bytes()).unwrap()
    }

    fn prompt_context() -> LibraNativeContext {
        LibraNativeContext::PromptAdmission {
            task_hint: "fix defect".into(),
            cwd: PathBuf::from("/repo"),
        }
    }

    #[test]
    fn unknown_scope_precedes_native_correlation_and_attribution() {
        let event = event(
            "tool_after",
            json!({"tool_name":"Edit"}),
            "unknown",
            "unknown",
            "wrong.native.name",
        );
        assert!(matches!(
            bind(NormalizedHostInput {
                event,
                native_context: LibraNativeContext::Completion { model: None },
            }),
            HostBindingOutcome::RecordOnly {
                reason: RecordOnlyReason::UnknownScope,
                ..
            }
        ));
    }

    #[test]
    fn prompt_and_stop_name_checks_apply_only_with_matching_native_context() {
        let wrong_prompt = event(
            "lifecycle",
            json!({"event_type":"turn_start"}),
            "root",
            "turn_task",
            "other.prompt.name",
        );
        assert!(matches!(
            bind(NormalizedHostInput {
                event: wrong_prompt,
                native_context: prompt_context(),
            }),
            HostBindingOutcome::Rejected(HostBindingFailure {
                reason: HostBindingReason::ContextMismatch,
                ..
            })
        ));

        let no_prompt_context = event(
            "lifecycle",
            json!({"event_type":"turn_start"}),
            "root",
            "turn_task",
            "other.prompt.name",
        );
        assert!(matches!(
            bind(NormalizedHostInput {
                event: no_prompt_context,
                native_context: LibraNativeContext::None,
            }),
            HostBindingOutcome::RecordOnly {
                reason: RecordOnlyReason::MissingNativeContext,
                ..
            }
        ));

        let wrong_stop = event(
            "lifecycle",
            json!({"event_type":"turn_end"}),
            "root",
            "turn_task",
            "other.stop.name",
        );
        assert!(matches!(
            bind(NormalizedHostInput {
                event: wrong_stop,
                native_context: LibraNativeContext::Completion {
                    model: Some("observed-model".into()),
                },
            }),
            HostBindingOutcome::Rejected(HostBindingFailure {
                reason: HostBindingReason::ContextMismatch,
                ..
            })
        ));
    }

    #[test]
    fn post_tool_name_and_context_mismatch_precede_attribution() {
        let wrong_name = event(
            "tool_after",
            json!({"tool_name":"Edit"}),
            "child",
            "turn_task",
            "other.tool.name",
        );
        assert!(matches!(
            bind(NormalizedHostInput {
                event: wrong_name,
                native_context: LibraNativeContext::None,
            }),
            HostBindingOutcome::Rejected(HostBindingFailure {
                reason: HostBindingReason::ContextMismatch,
                ..
            })
        ));

        let wrong_context = event(
            "tool_after",
            json!({"tool_name":"Edit"}),
            "child",
            "turn_task",
            "PostToolUse",
        );
        assert!(matches!(
            bind(NormalizedHostInput {
                event: wrong_context,
                native_context: prompt_context(),
            }),
            HostBindingOutcome::Rejected(HostBindingFailure {
                reason: HostBindingReason::ContextMismatch,
                ..
            })
        ));
    }

    #[test]
    fn unrelated_event_branches_keep_their_existing_record_only_reasons() {
        let before = event(
            "tool_before",
            json!({"tool_name":"Edit"}),
            "root",
            "turn_task",
            "unrelated.native.event",
        );
        assert!(matches!(
            bind(NormalizedHostInput {
                event: before,
                native_context: LibraNativeContext::None,
            }),
            HostBindingOutcome::RecordOnly {
                reason: RecordOnlyReason::UnsupportedToolBoundary,
                ..
            }
        ));
    }
}

// Keep the original canonical reducer coverage beside its compatibility
// wrapper. These assertions continue to protect native behavior after the pure
// mapping moved to the daemon library.
#[cfg(test)]
mod legacy_tests {
    use super::*;
    use libra_governor_protocol::validate_host_event;
    use std::path::PathBuf;

    const OBSERVED_AT: &str = "2026-10-05T00:00:00.000Z";

    fn event(kind: &str, facts: serde_json::Value, lineage: &str) -> ValidatedHostEvent {
        let mut identity = serde_json::json!({
            "envelope_version": 1,
            "observed_at": OBSERVED_AT,
            "host_id": "host-local",
            "tool_provider": "codex",
            "provider_session_id": "session-native",
            "agent_id": "agent-native",
            "turn_id": "turn-native",
            "lineage_status": lineage
        });
        if lineage == "child" {
            identity["parent_agent_id"] = serde_json::json!("parent-native");
        }
        let native_event_name = if kind == "tool_after" {
            "PostToolUse"
        } else if facts.get("event_type") == Some(&serde_json::json!("turn_start")) {
            "UserPromptSubmit"
        } else {
            "Stop"
        };
        let value = serde_json::json!({
            "schema_version": 1,
            "event_id": "observation-1",
            "observed_at": OBSERVED_AT,
            "adapter_id": "codex",
            "adapter_version": "1.0.0",
            "source": {"kind": "hook", "native_event_name": native_event_name},
            "capability_snapshot_id": "snapshot-1",
            "identity": identity,
            "scope": "turn_task",
            "kind": kind,
            "facts": facts,
            "quality": "reconstructed",
            "field_provenance": {}
        });
        validate_host_event(value.to_string().as_bytes()).unwrap()
    }

    fn synthetic_bind(
        event: ValidatedHostEvent,
        native_context: LibraNativeContext,
    ) -> HostBindingOutcome {
        bind(NormalizedHostInput {
            event,
            native_context,
        })
    }

    #[test]
    fn candidates_preserve_full_identity_and_correlated_prompt_context() {
        let event = event(
            "lifecycle",
            serde_json::json!({"event_type": "turn_start"}),
            "root",
        );
        let context = LibraNativeContext::PromptAdmission {
            task_hint: "fix defect".into(),
            cwd: "/repo".into(),
        };
        let result = synthetic_bind(event, context);
        match result {
            HostBindingOutcome::Candidate(BindingCandidate::Preflight {
                event,
                task_hint,
                cwd,
            }) => {
                assert_eq!(task_hint, "fix defect");
                assert_eq!(cwd, PathBuf::from("/repo"));
                assert_eq!(
                    event.identity().provider_session_id(),
                    Some("session-native")
                );
                assert_eq!(event.identity().agent_id(), Some("agent-native"));
                assert_eq!(event.identity().turn_id(), Some("turn-native"));
            }
            other => panic!("unexpected binding result: {other:?}"),
        }
    }

    #[test]
    fn completion_candidate_preserves_model_and_full_root_identity() {
        let event = event(
            "lifecycle",
            serde_json::json!({"event_type": "turn_end"}),
            "root",
        );
        match synthetic_bind(
            event,
            LibraNativeContext::Completion {
                model: Some("observed-model".into()),
            },
        ) {
            HostBindingOutcome::Candidate(BindingCandidate::TurnCompletion { event, model }) => {
                assert_eq!(model.as_deref(), Some("observed-model"));
                assert_eq!(
                    event.identity().provider_session_id(),
                    Some("session-native")
                );
                assert_eq!(event.identity().agent_id(), Some("agent-native"));
                assert_eq!(event.identity().turn_id(), Some("turn-native"));
            }
            other => panic!("unexpected binding result: {other:?}"),
        }
    }

    #[test]
    fn unknown_or_child_lineage_never_yields_an_effect_candidate() {
        for lineage in ["unknown", "child"] {
            let event = event(
                "tool_after",
                serde_json::json!({"tool_name": "Edit", "native_call_id": "call-native"}),
                lineage,
            );
            assert!(matches!(
                synthetic_bind(event, LibraNativeContext::None),
                HostBindingOutcome::RecordOnly {
                    reason: RecordOnlyReason::MissingAttributionIdentity,
                    ..
                }
            ));
        }
    }

    #[test]
    fn missing_dimensions_and_unknown_scope_are_record_only() {
        let mut raw = serde_json::json!({
            "schema_version": 1,
            "event_id": "observation-1",
            "observed_at": OBSERVED_AT,
            "adapter_id": "codex",
            "adapter_version": "1.0.0",
            "source": {"kind": "hook", "native_event_name": "PostToolUse"},
            "capability_snapshot_id": "snapshot-1",
            "identity": {"envelope_version": 1, "observed_at": OBSERVED_AT, "host_id": "host", "tool_provider": "codex", "lineage_status": "unknown"},
            "scope": "turn_task",
            "kind": "tool_after",
            "facts": {"tool_name": "Bash"},
            "quality": "literal",
            "field_provenance": {}
        });
        let event = validate_host_event(raw.to_string().as_bytes()).unwrap();
        assert!(matches!(
            synthetic_bind(event, LibraNativeContext::None),
            HostBindingOutcome::RecordOnly {
                reason: RecordOnlyReason::MissingAttributionIdentity,
                ..
            }
        ));
        raw["scope"] = serde_json::json!("unknown");
        let event = validate_host_event(raw.to_string().as_bytes()).unwrap();
        assert!(matches!(
            synthetic_bind(event, LibraNativeContext::None),
            HostBindingOutcome::RecordOnly {
                reason: RecordOnlyReason::UnknownScope,
                ..
            }
        ));
    }

    #[test]
    fn usage_unwired_lifecycle_and_missing_context_are_record_only() {
        let usage = event(
            "usage",
            serde_json::json!({"measure": "input_tokens", "amount": 1, "unit": "token", "aggregation": "delta", "observation_scope": "turn_task"}),
            "root",
        );
        assert!(matches!(
            synthetic_bind(usage, LibraNativeContext::None),
            HostBindingOutcome::RecordOnly {
                reason: RecordOnlyReason::UnsupportedUsage,
                ..
            }
        ));
        let lifecycle = event(
            "lifecycle",
            serde_json::json!({"event_type": "session_end"}),
            "root",
        );
        assert!(matches!(
            synthetic_bind(lifecycle, LibraNativeContext::None),
            HostBindingOutcome::RecordOnly {
                reason: RecordOnlyReason::UnsupportedLifecycle,
                ..
            }
        ));
        let start = event(
            "lifecycle",
            serde_json::json!({"event_type": "turn_start"}),
            "root",
        );
        assert!(matches!(
            synthetic_bind(start, LibraNativeContext::None),
            HostBindingOutcome::RecordOnly {
                reason: RecordOnlyReason::MissingNativeContext,
                ..
            }
        ));
    }
}
