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

pub(super) fn validate_request_value(
    request: &super::config_profile::ProfileRequest,
) -> Result<serde_json::Value, super::RegistryFailure> {
    let value = serde_json::to_value(request)
        .map_err(|_| super::RegistryFailure::new("profile", "request_encoding_refused"))?;
    validate_schema(REQUEST_SCHEMA, &value)?;
    Ok(value)
}

pub(super) fn validate_external_plan(
    request: &super::config_profile::ProfileRequest,
    value: &serde_json::Value,
) -> Result<super::config_profile::ValidatedConnectionPlan, super::RegistryFailure> {
    validate_schema(PLAN_SCHEMA, value)?;
    // The pinned integer/const schema admits 1.0. Project only this admitted
    // version into serde's integer representation, after schema validation.
    let mut projected = value.clone();
    projected["schema_version"] = serde_json::json!(1);
    let plan = serde_json::from_value(projected)
        .map_err(|_| super::RegistryFailure::new("profile", "plan_decoding_refused"))?;
    super::config_profile::validate_plan(request, plan)
}

fn validate_schema(bytes: &[u8], value: &serde_json::Value) -> Result<(), super::RegistryFailure> {
    let schema: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|_| super::RegistryFailure::new("profile", "shipped_schema_refused"))?;
    let validator = jsonschema::validator_for(&schema)
        .map_err(|_| super::RegistryFailure::new("profile", "shipped_schema_refused"))?;
    if !validator.is_valid(value) {
        return Err(super::RegistryFailure::new(
            "profile",
            "profile_schema_refused",
        ));
    }
    Ok(())
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
        let mut external = serde_json::to_value(plan.plan()).unwrap();
        external["schema_version"] = serde_json::json!(1.0);
        assert_eq!(validate_external_plan(&request, &external).unwrap(), plan);
        for version in [
            serde_json::json!(true),
            serde_json::json!(2),
            serde_json::json!("1"),
        ] {
            external["schema_version"] = version;
            assert!(validate_external_plan(&request, &external).is_err());
        }
        external["schema_version"] = serde_json::json!(1);
        external["command"] = serde_json::json!("PRIVATE_COMMAND_CANARY");
        assert!(validate_external_plan(&request, &external).is_err());
    }
}
