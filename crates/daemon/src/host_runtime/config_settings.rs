//! Exact local callback ownership; adapter proposals never supply commands.

use serde_json::{json, Value};

use super::RegistryFailure;

pub(super) struct OwnedCallback {
    pub(super) event: &'static str,
    pub(super) command: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CallbackPresence {
    Absent,
    ExactOwned,
}

fn fail(reason: &'static str) -> RegistryFailure {
    RegistryFailure::new("configuration", reason)
}

fn handler(callback: &OwnedCallback) -> Value {
    json!({"type":"command", "command":callback.command})
}

fn canonical_candidate(command: &str) -> bool {
    command.contains(" adapter-hook ")
        && command.contains("--state-root ")
        && command.contains("--binding ")
        && command.contains("--installation ")
}

pub(super) fn ensure_no_canonical_callbacks(document: &Value) -> Result<(), RegistryFailure> {
    let Some(hooks) = document.get("hooks").and_then(Value::as_object) else {
        return Ok(());
    };
    for groups in hooks.values() {
        for group in groups
            .as_array()
            .ok_or_else(|| fail("invalid_hook_layout"))?
        {
            for member in group
                .get("hooks")
                .and_then(Value::as_array)
                .ok_or_else(|| fail("invalid_hook_layout"))?
            {
                if member
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(canonical_candidate)
                {
                    return Err(fail("conflicting_active_callbacks"));
                }
            }
        }
    }
    Ok(())
}

/// Compatibility refusals confer no ownership over legacy or orphan entries.
pub(super) fn ensure_activation_compatible(
    document: &Value,
    expected: &[OwnedCallback],
) -> Result<(), RegistryFailure> {
    let Some(hooks) = document.get("hooks").and_then(Value::as_object) else {
        return Ok(());
    };
    for groups in hooks.values() {
        for group in groups
            .as_array()
            .ok_or_else(|| fail("invalid_hook_layout"))?
        {
            for member in group
                .get("hooks")
                .and_then(Value::as_array)
                .ok_or_else(|| fail("invalid_hook_layout"))?
            {
                let Some(command) = member.get("command").and_then(Value::as_str) else {
                    continue;
                };
                if expected.iter().any(|callback| callback.command == command) {
                    continue;
                }
                let legacy = command.contains("libra-governor")
                    && ["hook user-prompt-submit", "hook post-tool-use", "hook stop"]
                        .iter()
                        .any(|suffix| command.ends_with(suffix));
                let canonical = canonical_candidate(command);
                if legacy || canonical {
                    return Err(fail("conflicting_active_callbacks"));
                }
            }
        }
    }
    Ok(())
}

/// A command match under a different event/matcher or with extra handler fields
/// is a conflict, not authority to delete the modified entry.
pub(super) fn observe(
    document: &Value,
    callbacks: &[OwnedCallback; 3],
) -> Result<[CallbackPresence; 3], RegistryFailure> {
    if !document.is_object() {
        return Err(fail("configuration_not_object"));
    }
    let mut result = [CallbackPresence::Absent; 3];
    let Some(hooks) = document.get("hooks") else {
        return Ok(result);
    };
    let hooks = hooks
        .as_object()
        .ok_or_else(|| fail("invalid_hook_layout"))?;
    for (event, groups) in hooks {
        let groups = groups
            .as_array()
            .ok_or_else(|| fail("invalid_hook_layout"))?;
        for group in groups {
            let object = group
                .as_object()
                .ok_or_else(|| fail("invalid_hook_layout"))?;
            let members = object
                .get("hooks")
                .and_then(Value::as_array)
                .ok_or_else(|| fail("invalid_hook_layout"))?;
            for member in members {
                let member_object = member
                    .as_object()
                    .ok_or_else(|| fail("invalid_hook_layout"))?;
                let Some(command) = member_object.get("command").and_then(Value::as_str) else {
                    continue;
                };
                for (index, callback) in callbacks.iter().enumerate() {
                    if command != callback.command {
                        continue;
                    }
                    let matcher_ok = object.get("matcher").is_none_or(Value::is_null);
                    if event != callback.event
                        || !matcher_ok
                        || *member != handler(callback)
                        || result[index] != CallbackPresence::Absent
                    {
                        return Err(fail("owned_callback_conflict"));
                    }
                    result[index] = CallbackPresence::ExactOwned;
                }
            }
        }
    }
    Ok(result)
}

/// Observe callbacks while also refusing edited command strings that retain a
/// corroborated binding or installation reference. A near match is only a
/// conflict signal; it never establishes ownership or authorizes removal.
pub(super) fn observe_correlated(
    document: &Value,
    callbacks: &[OwnedCallback; 3],
    binding_id: &str,
    installation_id: &str,
) -> Result<[CallbackPresence; 3], RegistryFailure> {
    if !is_canonical_uuid(binding_id) || !is_canonical_uuid(installation_id) {
        return Err(fail("invalid_callback_reference"));
    }

    let presence = observe(document, callbacks)?;
    let Some(hooks) = document.get("hooks").and_then(Value::as_object) else {
        return Ok(presence);
    };
    for groups in hooks.values() {
        for group in groups.as_array().unwrap() {
            for member in group["hooks"].as_array().unwrap() {
                let Some(command) = member.get("command").and_then(Value::as_str) else {
                    continue;
                };
                if callbacks.iter().any(|callback| command == callback.command) {
                    continue;
                }
                if command.contains(binding_id) || command.contains(installation_id) {
                    return Err(fail("owned_callback_conflict"));
                }
            }
        }
    }
    Ok(presence)
}

fn is_canonical_uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|parsed| parsed.to_string() == value)
}

/// The coordinator supplies callbacks corroborated by its immutable artifact.
/// The result is only a JSON delta; locking, drift and readback remain its job.
pub(super) fn connect(
    document: &Value,
    callbacks: &[OwnedCallback; 3],
    enabled: bool,
) -> Result<Value, RegistryFailure> {
    let presence = observe(document, callbacks)?;
    let mut next = document.clone();
    if enabled {
        if presence
            .iter()
            .all(|value| *value == CallbackPresence::ExactOwned)
        {
            return Ok(next);
        }
        let hooks = next
            .as_object_mut()
            .unwrap()
            .entry("hooks")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .unwrap();
        for (index, callback) in callbacks.iter().enumerate() {
            if presence[index] == CallbackPresence::Absent {
                hooks
                    .entry(callback.event)
                    .or_insert_with(|| json!([]))
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"hooks":[handler(callback)]}));
            }
        }
    } else if presence.contains(&CallbackPresence::ExactOwned) {
        let hooks = next["hooks"].as_object_mut().unwrap();
        for callback in callbacks {
            let Some(groups) = hooks.get_mut(callback.event) else {
                continue;
            };
            let groups = groups.as_array_mut().unwrap();
            groups.retain_mut(|group| {
                let members = group["hooks"].as_array_mut().unwrap();
                let before = members.len();
                members.retain(|member| *member != handler(callback));
                // Preserve a pre-existing empty group; prune only our emptied
                // otherwise plain container, never its unknown future fields.
                !(before != members.len()
                    && members.is_empty()
                    && group.as_object().unwrap().len() == 1)
            });
            if groups.is_empty() {
                hooks.remove(callback.event);
            }
        }
        if hooks.is_empty() {
            next.as_object_mut().unwrap().remove("hooks");
        }
    }
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn callbacks() -> [OwnedCallback; 3] {
        ["UserPromptSubmit", "PostToolUse", "Stop"].map(|event| OwnedCallback {
            event,
            command: format!("fixed-owned-callback-{event}"),
        })
    }

    #[test]
    fn roundtrip_preserves_foreign_groups_and_future_fields() {
        let callbacks = callbacks();
        let before = json!({"future":{"unknown":[1,2]},"mcpServers":{"x":{}},
            "statusLine":{"command":"foreign"},"hooks":{"Stop":[{"matcher":"foreign", "hooks":[{"type":"command","command":"other-product"}]}]}});
        let connected = connect(&before, &callbacks, true).unwrap();
        assert_eq!(
            observe(&connected, &callbacks).unwrap(),
            [CallbackPresence::ExactOwned; 3]
        );
        assert_eq!(connect(&connected, &callbacks, true).unwrap(), connected);
        assert_eq!(connect(&connected, &callbacks, false).unwrap(), before);
    }

    #[test]
    fn modified_owner_and_wrong_event_refuse_without_a_delta() {
        let callbacks = callbacks();
        let good = connect(&json!({}), &callbacks, true).unwrap();
        let mut modified = good.clone();
        modified["hooks"]["Stop"][0]["hooks"][0]["future"] = json!(true);
        assert_eq!(
            connect(&modified, &callbacks, false).unwrap_err().reason,
            "owned_callback_conflict"
        );
        let mut wrong = good.clone();
        wrong["hooks"]["SessionEnd"] = wrong["hooks"]["Stop"].take();
        wrong["hooks"].as_object_mut().unwrap().remove("Stop");
        assert_eq!(
            connect(&wrong, &callbacks, false).unwrap_err().reason,
            "owned_callback_conflict"
        );
    }

    #[test]
    fn removing_a_mixed_group_preserves_its_foreign_members_and_metadata() {
        let callbacks = callbacks();
        let before = json!({"hooks":{"Stop":[{"future":123,"hooks":[handler(&callbacks[2]),{"type":"command","command":"foreign"}]}]}});
        let after = connect(&before, &callbacks, false).unwrap();
        assert_eq!(
            after,
            json!({"hooks":{"Stop":[{"future":123,"hooks":[{"type":"command","command":"foreign"}]}]}})
        );
    }

    #[test]
    fn duplicate_exact_callbacks_and_wrong_matchers_are_conflicts() {
        let callbacks = callbacks();
        let duplicate = json!({"hooks":{"Stop":[
            {"hooks":[handler(&callbacks[2])]},
            {"hooks":[handler(&callbacks[2])]}
        ]}});
        assert_eq!(
            observe(&duplicate, &callbacks).unwrap_err().reason,
            "owned_callback_conflict"
        );

        let wrong_matcher = json!({"hooks":{"Stop":[
            {"matcher":"Bash", "hooks":[handler(&callbacks[2])]}
        ]}});
        assert_eq!(
            observe(&wrong_matcher, &callbacks).unwrap_err().reason,
            "owned_callback_conflict"
        );
    }

    #[test]
    fn partial_owned_sets_are_reported_without_becoming_complete_ownership() {
        let callbacks = callbacks();
        let partial = json!({"hooks":{
            "UserPromptSubmit":[{"hooks":[handler(&callbacks[0])]}],
            "Stop":[{"hooks":[{"type":"command","command":"foreign"}]}]
        }});
        assert_eq!(
            observe(&partial, &callbacks).unwrap(),
            [
                CallbackPresence::ExactOwned,
                CallbackPresence::Absent,
                CallbackPresence::Absent
            ]
        );
    }

    #[test]
    fn mixed_group_order_and_future_metadata_survive_connect_and_disconnect() {
        let callbacks = callbacks();
        let before = json!({
            "future_top_level": {"version": 91},
            "hooks": {
                "Stop": [
                    {"future_group": ["first"], "hooks": [
                        {"type":"command", "command":"foreign-before"},
                        handler(&callbacks[2]),
                        {"type":"command", "command":"foreign-after"}
                    ]},
                    {"future_empty_group": true, "hooks": []}
                ],
                "ForeignEvent": [
                    {"matcher":"future matcher", "extra":true, "hooks":[
                        {"type":"command", "command":"foreign-event-hook"}
                    ]}
                ]
            }
        });

        let connected = connect(&before, &callbacks, true).unwrap();
        assert_eq!(connected["future_top_level"], before["future_top_level"]);
        assert_eq!(connected["hooks"]["Stop"][0], before["hooks"]["Stop"][0]);
        assert_eq!(connected["hooks"]["Stop"][1], before["hooks"]["Stop"][1]);
        assert_eq!(
            connected["hooks"]["Stop"][0]["hooks"][0]["command"],
            "foreign-before"
        );
        assert_eq!(
            connected["hooks"]["Stop"][0]["hooks"][1],
            handler(&callbacks[2])
        );
        assert_eq!(
            connected["hooks"]["Stop"][0]["hooks"][2]["command"],
            "foreign-after"
        );

        let disconnected = connect(&connected, &callbacks, false).unwrap();
        assert_eq!(disconnected["future_top_level"], before["future_top_level"]);
        assert_eq!(
            disconnected["hooks"]["Stop"][0],
            json!({"future_group":["first"],"hooks":[
                {"type":"command","command":"foreign-before"},
                {"type":"command","command":"foreign-after"}
            ]})
        );
        assert_eq!(disconnected["hooks"]["Stop"][1], before["hooks"]["Stop"][1]);
        assert_eq!(
            disconnected["hooks"]["ForeignEvent"],
            before["hooks"]["ForeignEvent"]
        );
    }

    #[test]
    fn malformed_hook_shapes_refuse_with_fixed_layout_failure() {
        let callbacks = callbacks();
        let malformed = [
            json!({"hooks": []}),
            json!({"hooks": {"Stop": {}}}),
            json!({"hooks": {"Stop": [null]}}),
            json!({"hooks": {"Stop": [{}]}}),
            json!({"hooks": {"Stop": [{"hooks": {}}]}}),
            json!({"hooks": {"Stop": [{"hooks": [null]}]}}),
        ];
        for document in malformed {
            assert_eq!(
                observe(&document, &callbacks).unwrap_err().reason,
                "invalid_hook_layout"
            );
        }
    }

    #[test]
    fn correlated_observation_refuses_edited_references_even_under_unknown_events() {
        let binding_id = "00000000-0000-4000-8000-000000000001";
        let installation_id = "00000000-0000-4000-8000-000000000002";
        let callbacks = ["UserPromptSubmit", "PostToolUse", "Stop"].map(|event| OwnedCallback {
            event,
            command: format!(
                "libra-hook --binding {binding_id} --installation {installation_id} --slot {event}"
            ),
        });

        let edited = json!({"hooks":{"UnknownFutureEvent":[{"hooks":[{
            "type":"command",
            "command":format!("{} --edited", callbacks[1].command)
        }]}]}});
        assert_eq!(
            observe_correlated(&edited, &callbacks, binding_id, installation_id)
                .unwrap_err()
                .reason,
            "owned_callback_conflict"
        );

        let unrelated = json!({"hooks":{"UnknownFutureEvent":[{"hooks":[{
            "type":"command",
            "command":"other-tool --binding 00000000-0000-4000-8000-000000000099"
        }]}]}});
        assert_eq!(
            observe_correlated(&unrelated, &callbacks, binding_id, installation_id).unwrap(),
            [CallbackPresence::Absent; 3]
        );
    }

    #[test]
    fn correlated_observation_requires_canonical_reference_ids() {
        let callbacks = callbacks();
        for (binding_id, installation_id) in [
            ("NOT-A-UUID", "00000000-0000-4000-8000-000000000002"),
            (
                "00000000-0000-4000-8000-000000000001",
                "00000000-0000-4000-8000-00000000000A",
            ),
        ] {
            assert_eq!(
                observe_correlated(&json!({}), &callbacks, binding_id, installation_id)
                    .unwrap_err()
                    .reason,
                "invalid_callback_reference"
            );
        }
    }
}
