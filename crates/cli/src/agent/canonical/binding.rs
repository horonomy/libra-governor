//! Pure interpretation of validated host observations for Libra.
//!
//! Candidates retain the complete canonical event and transient native context.
//! They are not daemon requests, resolved tasks, or authorization to perform effects.

use std::path::PathBuf;

use super::{LibraNativeContext, NormalizedHostInput};

use libra_governor_domain::LineageStatus;
use libra_governor_protocol::{
    HostBindingFailure, HostBindingReason, HostBindingStage, HostEventKind, HostEventScope,
    ValidatedHostEvent,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordOnlyReason {
    MissingNativeContext,
    MissingAttributionIdentity,
    UnknownScope,
    UnsupportedLifecycle,
    UnsupportedToolBoundary,
    UnsupportedUsage,
}

#[derive(Debug, Clone)]
pub enum BindingCandidate {
    Preflight {
        event: ValidatedHostEvent,
        task_hint: String,
        cwd: PathBuf,
    },
    CompletedTool {
        event: ValidatedHostEvent,
    },
    TurnCompletion {
        event: ValidatedHostEvent,
        model: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub enum HostBindingOutcome {
    Candidate(BindingCandidate),
    RecordOnly {
        event: ValidatedHostEvent,
        reason: RecordOnlyReason,
    },
    Rejected(HostBindingFailure),
}

pub(super) fn bind(input: NormalizedHostInput) -> HostBindingOutcome {
    let NormalizedHostInput {
        event,
        native_context,
    } = input;
    if event.scope() == HostEventScope::Unknown {
        return record(event, RecordOnlyReason::UnknownScope);
    }
    match event.kind() {
        HostEventKind::Lifecycle => {
            match event.facts().get("event_type").and_then(|v| v.as_str()) {
                Some("turn_start") => match native_context {
                    LibraNativeContext::PromptAdmission { task_hint, cwd } => {
                        if event.source().native_event_name != "UserPromptSubmit" {
                            return rejected(HostBindingReason::ContextMismatch);
                        }
                        if task_hint.is_empty() || cwd.as_os_str().is_empty() {
                            return record(event, RecordOnlyReason::MissingNativeContext);
                        }
                        if let Some(reason) = attribution_gap(&event) {
                            return record(event, reason);
                        }
                        HostBindingOutcome::Candidate(BindingCandidate::Preflight {
                            event,
                            task_hint,
                            cwd,
                        })
                    }
                    LibraNativeContext::None | LibraNativeContext::Completion { .. } => {
                        record(event, RecordOnlyReason::MissingNativeContext)
                    }
                },
                Some("turn_end") => match native_context {
                    LibraNativeContext::Completion { model } => {
                        if event.source().native_event_name != "Stop" {
                            return rejected(HostBindingReason::ContextMismatch);
                        }
                        if let Some(reason) = attribution_gap(&event) {
                            return record(event, reason);
                        }
                        HostBindingOutcome::Candidate(BindingCandidate::TurnCompletion {
                            event,
                            model,
                        })
                    }
                    LibraNativeContext::None | LibraNativeContext::PromptAdmission { .. } => {
                        record(event, RecordOnlyReason::MissingNativeContext)
                    }
                },
                _ => record(event, RecordOnlyReason::UnsupportedLifecycle),
            }
        }
        HostEventKind::ToolAfter => {
            if event.source().native_event_name != "PostToolUse"
                || !matches!(native_context, LibraNativeContext::None)
            {
                return rejected(HostBindingReason::ContextMismatch);
            }
            if let Some(reason) = attribution_gap(&event) {
                return record(event, reason);
            }
            HostBindingOutcome::Candidate(BindingCandidate::CompletedTool { event })
        }
        HostEventKind::ToolBefore | HostEventKind::ToolFailure => {
            record(event, RecordOnlyReason::UnsupportedToolBoundary)
        }
        HostEventKind::Usage => record(event, RecordOnlyReason::UnsupportedUsage),
    }
}

fn attribution_gap(event: &ValidatedHostEvent) -> Option<RecordOnlyReason> {
    let identity = event.identity();
    if identity.lineage_status() != LineageStatus::Root
        || identity.provider_session_id().is_none_or(str::is_empty)
        || identity.agent_id().is_none_or(str::is_empty)
        || identity.turn_id().is_none_or(str::is_empty)
    {
        return Some(RecordOnlyReason::MissingAttributionIdentity);
    }
    None
}

fn record(event: ValidatedHostEvent, reason: RecordOnlyReason) -> HostBindingOutcome {
    HostBindingOutcome::RecordOnly { event, reason }
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

    // Synthetic root-lineage fixtures exercise pure projection only.
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
