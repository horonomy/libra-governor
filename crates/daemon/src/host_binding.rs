//! Pure mapping of validated host observations into conservative Libra candidates.
//!
//! A candidate is not a daemon request, resolved task, authorization, or
//! evidence of accepted host attribution.

use std::path::PathBuf;

use libra_governor_domain::LineageStatus;
use libra_governor_protocol::{
    HostBindingFailure, HostBindingReason, HostBindingStage, HostEventKind, HostEventScope,
    ValidatedHostEvent,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibraNativeContext {
    PromptAdmission { task_hint: String, cwd: PathBuf },
    Completion { model: Option<String> },
    None,
}

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

/// Reduce a validated event with only explicit transient native context.
/// Native event names are interpreted by the concrete adapter wrapper.
pub fn bind(event: ValidatedHostEvent, native_context: LibraNativeContext) -> HostBindingOutcome {
    if event.scope() == HostEventScope::Unknown {
        return record(event, RecordOnlyReason::UnknownScope);
    }
    match event.kind() {
        HostEventKind::Lifecycle => {
            match event.facts().get("event_type").and_then(|v| v.as_str()) {
                Some("turn_start") => match native_context {
                    LibraNativeContext::PromptAdmission { task_hint, cwd } => {
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
            if !matches!(native_context, LibraNativeContext::None) {
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
    use serde_json::json;

    const OBSERVED_AT: &str = "2026-10-05T00:00:00.000Z";

    fn event(kind: &str, scope: &str, lineage: &str, native_name: &str) -> ValidatedHostEvent {
        let mut identity = json!({
            "envelope_version": 1,
            "observed_at": OBSERVED_AT,
            "host_id": "host-generic",
            "tool_provider": "unrelated_provider",
            "provider_session_id": "session-native",
            "agent_id": "agent-native",
            "turn_id": "turn-native",
            "lineage_status": lineage
        });
        if lineage == "child" {
            identity["parent_agent_id"] = json!("parent-native");
        }
        let facts = match kind {
            "lifecycle" => json!({"event_type":"turn_start"}),
            "usage" => {
                json!({"measure":"input_tokens","amount":3,"unit":"token","aggregation":"delta","observation_scope":"turn_task"})
            }
            _ => json!({"tool_name":"Edit", "native_call_id":"call-native"}),
        };
        let raw = json!({
            "schema_version":1,
            "event_id":"observation-generic",
            "observed_at":OBSERVED_AT,
            "adapter_id":"external_adapter",
            "adapter_version":"1.0.0",
            "source":{"kind":"hook","native_event_name":native_name},
            "capability_snapshot_id":"snapshot-generic",
            "identity":identity,
            "scope":scope,
            "kind":kind,
            "facts":facts,
            "quality":"reconstructed",
            "field_provenance":{}
        });
        validate_host_event(raw.to_string().as_bytes()).unwrap()
    }

    fn none() -> LibraNativeContext {
        LibraNativeContext::None
    }

    #[test]
    fn unrelated_provider_and_native_name_can_yield_only_a_completed_tool_candidate() {
        let event = event("tool_after", "turn_task", "root", "custom_driver.tool.end");
        match bind(event, none()) {
            HostBindingOutcome::Candidate(BindingCandidate::CompletedTool { event }) => {
                assert_eq!(event.adapter_id(), "external_adapter");
                assert_eq!(event.identity().tool_provider(), "unrelated_provider");
                assert_eq!(
                    event.identity().provider_session_id(),
                    Some("session-native")
                );
                assert_eq!(event.identity().agent_id(), Some("agent-native"));
                assert_eq!(event.identity().turn_id(), Some("turn-native"));
                assert_eq!(event.facts()["native_call_id"], "call-native");
            }
            _ => panic!("complete generic tool observation should remain a candidate"),
        }
    }

    #[test]
    fn unknown_missing_and_child_attribution_remain_record_only() {
        for lineage in ["unknown", "child"] {
            assert!(matches!(
                bind(
                    event("tool_after", "turn_task", lineage, "custom.tool.end"),
                    none()
                ),
                HostBindingOutcome::RecordOnly {
                    reason: RecordOnlyReason::MissingAttributionIdentity,
                    ..
                }
            ));
        }

        let mut raw =
            serde_json::to_value(event("tool_after", "turn_task", "root", "custom.tool.end"))
                .unwrap();
        raw["identity"].as_object_mut().unwrap().remove("agent_id");
        let missing = validate_host_event(raw.to_string().as_bytes()).unwrap();
        assert!(matches!(
            bind(missing, none()),
            HostBindingOutcome::RecordOnly {
                reason: RecordOnlyReason::MissingAttributionIdentity,
                ..
            }
        ));
    }

    #[test]
    fn unknown_scope_precedes_kind_context_and_attribution_classification() {
        let unknown = event("tool_after", "unknown", "root", "unrelated.name");
        assert!(matches!(
            bind(
                unknown,
                LibraNativeContext::PromptAdmission {
                    task_hint: "untrusted prompt claim".into(),
                    cwd: PathBuf::from("/untrusted/cwd"),
                }
            ),
            HostBindingOutcome::RecordOnly {
                reason: RecordOnlyReason::UnknownScope,
                ..
            }
        ));
    }

    #[test]
    fn lifecycle_without_context_and_usage_never_become_effect_candidates() {
        let lifecycle = event("lifecycle", "turn_task", "root", "generic.lifecycle.start");
        assert!(matches!(
            bind(lifecycle, none()),
            HostBindingOutcome::RecordOnly {
                reason: RecordOnlyReason::MissingNativeContext,
                ..
            }
        ));
        let usage = event("usage", "turn_task", "root", "unrelated.usage.delta");
        assert!(matches!(
            bind(usage, none()),
            HostBindingOutcome::RecordOnly {
                reason: RecordOnlyReason::UnsupportedUsage,
                ..
            }
        ));
    }
}
