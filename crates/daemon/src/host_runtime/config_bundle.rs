//! Installed product validator identity; adapters cannot replace its schema.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(super) const PROFILE_ID: &str = "libra.claude-hooks.v1";
pub(super) const REQUEST_SCHEMA: &[u8] =
    include_bytes!("../../resources/config-profiles/libra.claude-hooks.v1/request.schema.json");
pub(super) const PLAN_SCHEMA: &[u8] =
    include_bytes!("../../resources/config-profiles/libra.claude-hooks.v1/plan.schema.json");

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ConfigValidatorRef {
    pub(super) id: String,
    pub(super) version: u32,
    pub(super) digest: String,
}

pub(super) fn sha256(bytes: &[u8]) -> String {
    format!(
        "sha256:{}",
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}

pub(super) fn validator_ref() -> ConfigValidatorRef {
    let mut bytes = b"libra-config-validator-v1\0".to_vec();
    for resource in [REQUEST_SCHEMA, PLAN_SCHEMA] {
        bytes.extend_from_slice(&(resource.len() as u64).to_be_bytes());
        bytes.extend_from_slice(resource);
    }
    ConfigValidatorRef {
        id: PROFILE_ID.into(),
        version: 1,
        digest: sha256(&bytes),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_schema_shapes_match_pure_profile_roundtrip() {
        use super::super::config_profile::*;
        let slots = [
            ConfigSlot::PromptSubmit,
            ConfigSlot::ToolCompleted,
            ConfigSlot::TurnCompleted,
        ];
        let request = ProfileRequest {
            schema_version: 1,
            intent: ConfigIntent::Install,
            binding_id: "12345678-1234-4234-8234-123456789abc".into(),
            installation_id: "12345678-1234-4234-8234-123456789abd".into(),
            target_revision: None,
            slots: slots
                .map(|slot| SlotRequest {
                    slot,
                    callback_ref: "a".repeat(64),
                    desired: DesiredSlot::Absent,
                    observed: SlotObserved::Absent,
                    owned_digest: None,
                })
                .to_vec(),
        };
        let plan = build_plan(&request).unwrap();
        for (schema, value) in [
            (REQUEST_SCHEMA, serde_json::to_value(&request).unwrap()),
            (PLAN_SCHEMA, serde_json::to_value(plan.plan()).unwrap()),
        ] {
            let schema: serde_json::Value = serde_json::from_slice(schema).unwrap();
            let validator = jsonschema::validator_for(&schema).unwrap();
            assert!(validator.is_valid(&value));
            let mut invalid = value;
            invalid["target_revision"] = serde_json::Value::Null;
            assert!(!validator.is_valid(&invalid));
        }
        assert_eq!(validator_ref().id, PROFILE_ID);
        assert_eq!(validator_ref().version, 1);
    }
}
