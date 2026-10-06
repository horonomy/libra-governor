//! Closed builtin installation intent and locally completed evidence.

use std::path::{Component, Path};

use serde::{Deserialize, Deserializer, Serialize};

use super::config_bundle::{sha256, validator_ref, ConfigValidatorRef};
use super::config_profile::{ConfigIntent, ConfigSlot};
use super::RegistryFailure;

const SLOTS: [ConfigSlot; 3] = [
    ConfigSlot::PromptSubmit,
    ConfigSlot::ToolCompleted,
    ConfigSlot::TurnCompleted,
];
const MAX_REVISION: u64 = 9_007_199_254_740_991;

fn nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InstallationContext {
    pub(super) scope: String,
    pub(super) state_root: String,
    pub(super) target_path: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProductBinary {
    pub(super) path: String,
    pub(super) sha256: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum FileFingerprint {
    Absent,
    File { sha256: String },
}

impl FileFingerprint {
    pub(super) fn from_bytes(bytes: Option<&[u8]>) -> Self {
        bytes.map_or(Self::Absent, |bytes| Self::File {
            sha256: sha256(bytes),
        })
    }

    fn valid(&self) -> bool {
        match self {
            Self::Absent => true,
            Self::File { sha256 } => digest_valid(sha256),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct HookHandler {
    #[serde(rename = "type")]
    pub(super) kind: String,
    pub(super) command: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct OwnedHook {
    pub(super) slot: ConfigSlot,
    pub(super) event: String,
    #[serde(deserialize_with = "nullable")]
    pub(super) matcher: Option<String>,
    pub(super) handler: HookHandler,
    pub(super) sha256: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ConnectionEvidence {
    pub(super) target: FileFingerprint,
    pub(super) owned: Vec<OwnedHook>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct FileDelta {
    pub(super) before: FileFingerprint,
    pub(super) after: FileFingerprint,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TargetDelta {
    pub(super) before: FileFingerprint,
    pub(super) after: FileFingerprint,
    pub(super) owned_before: Vec<OwnedHook>,
    pub(super) owned_after: Vec<OwnedHook>,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum IntentPhase {
    Prepared,
    GateClosed,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PendingIntent {
    pub(super) transaction_id: String,
    pub(super) operation: ConfigIntent,
    pub(super) phase: IntentPhase,
    pub(super) base_revision: u64,
    pub(super) artifact: FileDelta,
    #[serde(deserialize_with = "nullable")]
    pub(super) target: Option<TargetDelta>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InstallationRecord {
    pub(super) installation_id: String,
    pub(super) adapter_id: String,
    pub(super) validator_ref: ConfigValidatorRef,
    pub(super) context: InstallationContext,
    pub(super) binary: ProductBinary,
    pub(super) artifact_sha256: String,
    pub(super) revision: u64,
    pub(super) installed: bool,
    pub(super) desired_enabled: bool,
    #[serde(deserialize_with = "nullable")]
    pub(super) connection: Option<ConnectionEvidence>,
    #[serde(deserialize_with = "nullable")]
    pub(super) pending: Option<PendingIntent>,
}

impl std::fmt::Debug for InstallationRecord {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InstallationRecord")
            .field("installed", &self.installed)
            .field("desired_enabled", &self.desired_enabled)
            .field("pending", &self.pending.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ArtifactSlot {
    pub(super) slot: ConfigSlot,
    pub(super) callback_ref: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InstallationArtifact {
    pub(super) schema_version: u32,
    pub(super) registry_id: String,
    pub(super) binding_id: String,
    pub(super) installation_id: String,
    pub(super) adapter_id: String,
    pub(super) validator_ref: ConfigValidatorRef,
    pub(super) context: InstallationContext,
    pub(super) binary: ProductBinary,
    pub(super) slots: Vec<ArtifactSlot>,
}

fn fail() -> RegistryFailure {
    RegistryFailure::new("lifecycle", "invalid_lifecycle_state")
}

fn lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(super) fn digest_valid(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|value| lower_hex(value, 64))
}

pub(super) fn uuid_valid(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|id| id.hyphenated().to_string() == value)
}

fn path_valid(value: &str) -> bool {
    let path = Path::new(value);
    !value.is_empty()
        && value.len() <= 4096
        && !value.contains(['\0', '\n', '\r'])
        && path.is_absolute()
        && path.file_name().is_some()
        && path
            .components()
            .all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
        && path
            .components()
            .collect::<std::path::PathBuf>()
            .as_os_str()
            == path.as_os_str()
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub(super) fn slot_name(slot: ConfigSlot) -> &'static str {
    match slot {
        ConfigSlot::PromptSubmit => "prompt_submit",
        ConfigSlot::ToolCompleted => "tool_completed",
        ConfigSlot::TurnCompleted => "turn_completed",
    }
}

impl InstallationRecord {
    pub(super) fn prepare_install(
        registry: &str,
        revision: u64,
        context: InstallationContext,
        binary: ProductBinary,
    ) -> Result<(String, Self), RegistryFailure> {
        let binding = uuid::Uuid::new_v4().to_string();
        let mut record = Self {
            installation_id: uuid::Uuid::new_v4().to_string(),
            adapter_id: "claude_code".into(),
            validator_ref: validator_ref(),
            context,
            binary,
            artifact_sha256: String::new(),
            revision,
            installed: false,
            desired_enabled: false,
            connection: None,
            pending: None,
        };
        record.artifact_sha256 = sha256(&record.artifact_bytes(registry, &binding)?);
        record.pending = Some(PendingIntent {
            transaction_id: uuid::Uuid::new_v4().to_string(),
            operation: ConfigIntent::Install,
            phase: IntentPhase::Prepared,
            base_revision: 0,
            artifact: FileDelta {
                before: FileFingerprint::Absent,
                after: FileFingerprint::File {
                    sha256: record.artifact_sha256.clone(),
                },
            },
            target: None,
        });
        record.validate(registry, &binding, revision)?;
        Ok((binding, record))
    }

    pub(super) fn owned_hooks(&self, binding: &str) -> Vec<OwnedHook> {
        SLOTS.into_iter().map(|slot| {
            let command = format!("{} adapter-hook --state-root {} --binding {} --installation {} --slot {}",
                quote(&self.binary.path), quote(&self.context.state_root), quote(binding),
                quote(&self.installation_id), quote(slot_name(slot)));
            let handler = HookHandler { kind:"command".into(), command };
            let projection = serde_json::json!({"slot":slot,"event":slot.native_event(),"matcher":null,"handler":handler});
            let mut bytes=b"libra-claude-owned-hook-v1\0".to_vec();
            bytes.extend(serde_json::to_vec(&projection).expect("fixed ownership projection"));
            OwnedHook { slot, event:slot.native_event().into(), matcher:None, handler, sha256:sha256(&bytes) }
        }).collect()
    }

    pub(super) fn artifact(&self, registry: &str, binding: &str) -> InstallationArtifact {
        let slots = SLOTS
            .into_iter()
            .map(|slot| {
                let material = format!(
                    "libra-claude-callback-v1\n{registry}\n{binding}\n{}\n{}",
                    self.installation_id,
                    slot_name(slot)
                );
                ArtifactSlot {
                    slot,
                    callback_ref: sha256(material.as_bytes())[7..].into(),
                }
            })
            .collect();
        InstallationArtifact {
            schema_version: 1,
            registry_id: registry.into(),
            binding_id: binding.into(),
            installation_id: self.installation_id.clone(),
            adapter_id: self.adapter_id.clone(),
            validator_ref: self.validator_ref.clone(),
            context: self.context.clone(),
            binary: self.binary.clone(),
            slots,
        }
    }

    pub(super) fn artifact_bytes(
        &self,
        registry: &str,
        binding: &str,
    ) -> Result<Vec<u8>, RegistryFailure> {
        serde_json::to_vec(&self.artifact(registry, binding)).map_err(|_| fail())
    }

    pub(super) fn validate(
        &self,
        registry: &str,
        binding: &str,
        revision: u64,
    ) -> Result<(), RegistryFailure> {
        if !lower_hex(registry, 32)
            || !uuid_valid(binding)
            || !uuid_valid(&self.installation_id)
            || self.adapter_id != "claude_code"
            || self.validator_ref != validator_ref()
            || self.context.scope != "user"
            || !path_valid(&self.context.state_root)
            || !path_valid(&self.context.target_path)
            || !path_valid(&self.binary.path)
            || !digest_valid(&self.binary.sha256)
            || !digest_valid(&self.artifact_sha256)
            || self.revision == 0
            || self.revision > revision
            || revision > MAX_REVISION
            || sha256(&self.artifact_bytes(registry, binding)?) != self.artifact_sha256
        {
            return Err(fail());
        }
        let owned = self.owned_hooks(binding);
        let valid_set = |set: &[OwnedHook]| set.is_empty() || set == owned;
        if let Some(connection) = &self.connection {
            if !connection.target.valid()
                || !valid_set(&connection.owned)
                || (!connection.owned.is_empty() && connection.target == FileFingerprint::Absent)
            {
                return Err(fail());
            }
        }
        let expected = FileFingerprint::File {
            sha256: self.artifact_sha256.clone(),
        };
        let Some(pending) = &self.pending else {
            if !self.installed
                || (self.desired_enabled
                    && self.connection.as_ref().is_none_or(|c| c.owned != owned))
                || (!self.desired_enabled
                    && self
                        .connection
                        .as_ref()
                        .is_some_and(|c| !c.owned.is_empty()))
            {
                return Err(fail());
            }
            return Ok(());
        };
        if self.desired_enabled
            || !uuid_valid(&pending.transaction_id)
            || pending.base_revision >= self.revision
        {
            return Err(fail());
        }
        if let Some(target) = &pending.target {
            if !target.before.valid()
                || !target.after.valid()
                || !valid_set(&target.owned_before)
                || !valid_set(&target.owned_after)
                || (!target.owned_before.is_empty() && target.before == FileFingerprint::Absent)
                || (!target.owned_after.is_empty() && target.after == FileFingerprint::Absent)
            {
                return Err(fail());
            }
        }
        match (pending.operation, pending.phase) {
            (ConfigIntent::Install, IntentPhase::Prepared) => {
                if self.installed
                    || pending.base_revision != 0
                    || self.connection.is_some()
                    || pending.target.is_some()
                    || pending.artifact.before != FileFingerprint::Absent
                    || pending.artifact.after != expected
                {
                    return Err(fail());
                }
            }
            (ConfigIntent::Disable, IntentPhase::GateClosed) => {
                if !self.installed
                    || pending.base_revision == 0
                    || pending.artifact.before != expected
                    || pending.artifact.after != expected
                {
                    return Err(fail());
                }
                let historical = self.connection.as_ref().map(|c| TargetDelta {
                    before: c.target.clone(),
                    after: c.target.clone(),
                    owned_before: c.owned.clone(),
                    owned_after: c.owned.clone(),
                });
                if pending.target != historical {
                    return Err(fail());
                }
            }
            (operation, IntentPhase::Prepared) => {
                let target = pending.target.as_ref().ok_or_else(fail)?;
                if !self.installed
                    || pending.base_revision == 0
                    || pending.artifact.before != expected
                {
                    return Err(fail());
                }
                match operation {
                    ConfigIntent::Enable
                        if pending.artifact.after == expected
                            && target.owned_after == owned
                            && target.owned_before.is_empty() =>
                    {
                        if self
                            .connection
                            .as_ref()
                            .is_some_and(|c| !c.owned.is_empty())
                        {
                            return Err(fail());
                        }
                    }
                    ConfigIntent::Disable
                        if pending.artifact.after == expected && target.owned_after.is_empty() => {}
                    ConfigIntent::Uninstall
                        if pending.artifact.after == FileFingerprint::Absent
                            && target.owned_after.is_empty() =>
                    {
                        if self
                            .connection
                            .as_ref()
                            .is_some_and(|c| !c.owned.is_empty())
                        {
                            return Err(fail());
                        }
                    }
                    _ => return Err(fail()),
                }
            }
            _ => return Err(fail()),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REGISTRY: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const BINDING: &str = "12345678-1234-4234-8234-123456789abc";
    const INSTALLATION: &str = "12345678-1234-4234-8234-123456789abd";
    const TRANSACTION: &str = "12345678-1234-4234-8234-123456789abe";

    fn fixture() -> InstallationRecord {
        let mut record = InstallationRecord {
            installation_id: INSTALLATION.into(),
            adapter_id: "claude_code".into(),
            validator_ref: validator_ref(),
            context: InstallationContext {
                scope: "user".into(),
                state_root: "/tmp/state".into(),
                target_path: "/tmp/home/.claude/settings.json".into(),
            },
            binary: ProductBinary {
                path: "/tmp/libra-governor".into(),
                sha256: sha256(b"product"),
            },
            artifact_sha256: String::new(),
            revision: 3,
            installed: true,
            desired_enabled: false,
            connection: None,
            pending: None,
        };
        record.artifact_sha256 = sha256(&record.artifact_bytes(REGISTRY, BINDING).unwrap());
        record
    }

    fn file(record: &InstallationRecord) -> FileFingerprint {
        FileFingerprint::File {
            sha256: record.artifact_sha256.clone(),
        }
    }

    #[test]
    fn stable_disabled_connected_and_incoherent_flags_are_distinct() {
        let mut record = fixture();
        assert!(record.validate(REGISTRY, BINDING, 3).is_ok());
        record.desired_enabled = true;
        assert!(record.validate(REGISTRY, BINDING, 3).is_err());
        record.connection = Some(ConnectionEvidence {
            target: FileFingerprint::from_bytes(Some(b"settings")),
            owned: record.owned_hooks(BINDING),
        });
        assert!(record.validate(REGISTRY, BINDING, 3).is_ok());
        record.desired_enabled = false;
        assert!(record.validate(REGISTRY, BINDING, 3).is_err());
        record.connection.as_mut().unwrap().owned.clear();
        assert!(record.validate(REGISTRY, BINDING, 3).is_ok());
        record.installed = false;
        assert!(record.validate(REGISTRY, BINDING, 3).is_err());
    }

    #[test]
    fn first_install_has_expected_artifact_without_fabricated_completion() {
        let mut record = fixture();
        record.installed = false;
        record.revision = 1;
        record.pending = Some(PendingIntent {
            transaction_id: TRANSACTION.into(),
            operation: ConfigIntent::Install,
            phase: IntentPhase::Prepared,
            base_revision: 0,
            artifact: FileDelta {
                before: FileFingerprint::Absent,
                after: file(&record),
            },
            target: None,
        });
        assert!(record.validate(REGISTRY, BINDING, 1).is_ok());
        record.installed = true;
        assert!(record.validate(REGISTRY, BINDING, 1).is_err());
        record.installed = false;
        record.pending.as_mut().unwrap().base_revision = 1;
        assert!(record.validate(REGISTRY, BINDING, 2).is_err());
    }

    #[test]
    fn pending_enable_closes_gate_and_preserves_exact_slot_authority() {
        let mut record = fixture();
        record.revision = 4;
        record.pending = Some(PendingIntent {
            transaction_id: TRANSACTION.into(),
            operation: ConfigIntent::Enable,
            phase: IntentPhase::Prepared,
            base_revision: 3,
            artifact: FileDelta {
                before: file(&record),
                after: file(&record),
            },
            target: Some(TargetDelta {
                before: FileFingerprint::Absent,
                after: FileFingerprint::from_bytes(Some(b"settings")),
                owned_before: vec![],
                owned_after: record.owned_hooks(BINDING),
            }),
        });
        assert!(record.validate(REGISTRY, BINDING, 4).is_ok());
        record.desired_enabled = true;
        assert!(record.validate(REGISTRY, BINDING, 4).is_err());
        record.desired_enabled = false;
        record
            .pending
            .as_mut()
            .unwrap()
            .target
            .as_mut()
            .unwrap()
            .owned_after
            .pop();
        assert!(record.validate(REGISTRY, BINDING, 4).is_err());
    }

    #[test]
    fn disable_gate_closed_carries_only_historical_evidence() {
        let mut record = fixture();
        record.revision = 4;
        let target = FileFingerprint::from_bytes(Some(b"connected"));
        let owned = record.owned_hooks(BINDING);
        record.connection = Some(ConnectionEvidence {
            target: target.clone(),
            owned: owned.clone(),
        });
        record.pending = Some(PendingIntent {
            transaction_id: TRANSACTION.into(),
            operation: ConfigIntent::Disable,
            phase: IntentPhase::GateClosed,
            base_revision: 3,
            artifact: FileDelta {
                before: file(&record),
                after: file(&record),
            },
            target: Some(TargetDelta {
                before: target.clone(),
                after: target,
                owned_before: owned.clone(),
                owned_after: owned,
            }),
        });
        assert!(record.validate(REGISTRY, BINDING, 4).is_ok());
        record
            .pending
            .as_mut()
            .unwrap()
            .target
            .as_mut()
            .unwrap()
            .owned_after
            .clear();
        assert!(record.validate(REGISTRY, BINDING, 4).is_err());
    }

    #[test]
    fn required_nullable_fields_and_unknown_state_fields_refuse() {
        let record = fixture();
        let value = serde_json::to_value(&record).unwrap();
        assert!(serde_json::from_value::<InstallationRecord>(value.clone()).is_ok());
        for key in ["connection", "pending"] {
            let mut missing = value.clone();
            missing.as_object_mut().unwrap().remove(key);
            assert!(serde_json::from_value::<InstallationRecord>(missing).is_err());
        }
        let mut unknown = value;
        unknown["supported"] = serde_json::json!(true);
        assert!(serde_json::from_value::<InstallationRecord>(unknown).is_err());
    }

    #[test]
    fn artifact_is_immutable_across_lifecycle_flags_and_hook_tampering_refuses() {
        let mut record = fixture();
        let bytes = record.artifact_bytes(REGISTRY, BINDING).unwrap();
        record.revision = 9;
        record.desired_enabled = true;
        assert!(record.artifact_bytes(REGISTRY, BINDING).unwrap() == bytes);
        let mut hooks = record.owned_hooks(BINDING);
        hooks[0].handler.command.push_str(" arbitrary");
        record.connection = Some(ConnectionEvidence {
            target: FileFingerprint::from_bytes(Some(b"settings")),
            owned: hooks,
        });
        assert!(record.validate(REGISTRY, BINDING, 9).is_err());
        assert!(
            record
                .artifact_bytes("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", BINDING)
                .unwrap()
                != bytes
        );
    }
}
