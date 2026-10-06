//! Pure, closed request-to-slot planning and independent plan validation.
//!
//! This module never reads or writes a host configuration. Its validated plan
//! is only a typed result for the operation-local consumer to use.

use serde::{Deserialize, Deserializer, Serialize};

use super::RegistryFailure;

const SCHEMA_VERSION: u32 = 1;
const SLOT_COUNT: usize = 3;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigSlot {
    PromptSubmit,
    ToolCompleted,
    TurnCompleted,
}

impl ConfigSlot {
    pub fn native_event(self) -> &'static str {
        match self {
            Self::PromptSubmit => "UserPromptSubmit",
            Self::ToolCompleted => "PostToolUse",
            Self::TurnCompleted => "Stop",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigIntent {
    Install,
    Enable,
    Disable,
    Uninstall,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DesiredSlot {
    Present,
    Absent,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotObserved {
    Absent,
    ExactOwned,
    Conflict,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotAction {
    Add,
    Preserve,
    Remove,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileRequest {
    pub schema_version: u32,
    pub intent: ConfigIntent,
    pub binding_id: String,
    pub installation_id: String,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_revision",
        skip_serializing_if = "Option::is_none"
    )]
    pub target_revision: Option<String>,
    pub slots: Vec<SlotRequest>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SlotRequest {
    pub slot: ConfigSlot,
    pub callback_ref: String,
    pub desired: DesiredSlot,
    pub observed: SlotObserved,
    #[serde(default = "missing_nullable_string")]
    pub owned_digest: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProfilePlan {
    pub schema_version: u32,
    pub binding_id: String,
    pub installation_id: String,
    #[serde(
        default,
        deserialize_with = "deserialize_optional_revision",
        skip_serializing_if = "Option::is_none"
    )]
    pub target_revision: Option<String>,
    pub changes: Vec<SlotChange>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SlotChange {
    pub slot: ConfigSlot,
    pub action: SlotAction,
    pub callback_ref: String,
    #[serde(default = "missing_nullable_string")]
    pub expected_owned_digest: Option<String>,
    pub placement: Placement,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Placement {
    pub event: String,
    #[serde(default = "missing_nullable_string")]
    pub matcher: Option<String>,
}

/// This type cannot be deserialized or constructed outside this module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedConnectionPlan {
    plan: ProfilePlan,
}

impl ValidatedConnectionPlan {
    pub fn plan(&self) -> &ProfilePlan {
        &self.plan
    }

    pub fn changes(&self) -> &[SlotChange] {
        &self.plan.changes
    }
}

/// Build the shipped profile's only supported plan, then pass it through the
/// same independent validator used for externally produced plans.
pub fn build_plan(request: &ProfileRequest) -> Result<ValidatedConnectionPlan, RegistryFailure> {
    validate_request(request)?;
    let mut changes = Vec::with_capacity(SLOT_COUNT);
    for slot_request in &request.slots {
        let action = action_for(slot_request)?;
        changes.push(SlotChange {
            slot: slot_request.slot,
            action,
            callback_ref: slot_request.callback_ref.clone(),
            expected_owned_digest: match slot_request.observed {
                SlotObserved::ExactOwned => slot_request.owned_digest.clone(),
                SlotObserved::Absent => None,
                SlotObserved::Conflict => return Err(fail("slot conflict")),
            },
            placement: Placement {
                event: slot_request.slot.native_event().to_owned(),
                matcher: None,
            },
        });
    }
    let plan = ProfilePlan {
        schema_version: SCHEMA_VERSION,
        binding_id: request.binding_id.clone(),
        installation_id: request.installation_id.clone(),
        target_revision: request.target_revision.clone(),
        changes,
    };
    validate_plan(request, plan)
}

/// Independently validate a driver-produced closed plan against its request.
pub fn validate_plan(
    request: &ProfileRequest,
    plan: ProfilePlan,
) -> Result<ValidatedConnectionPlan, RegistryFailure> {
    validate_request(request)?;
    if plan.schema_version != SCHEMA_VERSION
        || plan.binding_id != request.binding_id
        || plan.installation_id != request.installation_id
        || plan.target_revision != request.target_revision
        || plan.changes.len() != SLOT_COUNT
    {
        return Err(fail("plan metadata mismatch"));
    }

    let mut seen = [false; SLOT_COUNT];
    for change in &plan.changes {
        let index = slot_index(change.slot);
        if seen[index] {
            return Err(fail("duplicate plan slot"));
        }
        seen[index] = true;
        let source = request
            .slots
            .iter()
            .find(|slot| slot.slot == change.slot)
            .ok_or_else(|| fail("plan slot mismatch"))?;
        let action = action_for(source)?;
        let expected_digest = match source.observed {
            SlotObserved::Absent => None,
            SlotObserved::ExactOwned => source.owned_digest.clone(),
            SlotObserved::Conflict => return Err(fail("slot conflict")),
        };
        if change.action != action
            || change.callback_ref != source.callback_ref
            || change.expected_owned_digest != expected_digest
            || change.placement.event != change.slot.native_event()
            || change.placement.matcher.is_some()
        {
            return Err(fail("plan slot does not match request"));
        }
    }
    if seen.iter().any(|present| !present) {
        return Err(fail("missing plan slot"));
    }
    Ok(ValidatedConnectionPlan { plan })
}

fn validate_request(request: &ProfileRequest) -> Result<(), RegistryFailure> {
    if request.schema_version != SCHEMA_VERSION {
        return Err(fail("unsupported profile schema"));
    }
    if !is_canonical_uuid(&request.binding_id) || !is_canonical_uuid(&request.installation_id) {
        return Err(fail("invalid profile identity"));
    }
    if request
        .target_revision
        .as_deref()
        .is_some_and(|revision| !is_sha256(revision))
    {
        return Err(fail("invalid target revision"));
    }
    if request.slots.len() != SLOT_COUNT {
        return Err(fail("profile requires three slots"));
    }

    let mut seen = [false; SLOT_COUNT];
    for slot in &request.slots {
        let index = slot_index(slot.slot);
        if seen[index] {
            return Err(fail("duplicate profile slot"));
        }
        seen[index] = true;
        if !is_lower_hex(&slot.callback_ref, 64) {
            return Err(fail("invalid callback reference"));
        }
        match (slot.observed, slot.owned_digest.as_deref()) {
            (SlotObserved::Absent, None) => {}
            (SlotObserved::ExactOwned, Some(digest)) if is_sha256(digest) => {}
            (SlotObserved::Conflict, _) => return Err(fail("slot conflict")),
            _ => return Err(fail("invalid observed ownership")),
        }
        match request.intent {
            ConfigIntent::Enable if slot.desired != DesiredSlot::Present => {
                return Err(fail("enable requires present slots"));
            }
            ConfigIntent::Install | ConfigIntent::Disable | ConfigIntent::Uninstall
                if slot.desired != DesiredSlot::Absent =>
            {
                return Err(fail("intent requires absent slots"));
            }
            _ => {}
        }
    }
    if seen.iter().any(|present| !present) {
        return Err(fail("profile requires three slots"));
    }
    Ok(())
}

fn action_for(slot: &SlotRequest) -> Result<SlotAction, RegistryFailure> {
    match (slot.desired, slot.observed) {
        (DesiredSlot::Present, SlotObserved::Absent) => Ok(SlotAction::Add),
        (DesiredSlot::Present, SlotObserved::ExactOwned) => Ok(SlotAction::Preserve),
        (DesiredSlot::Absent, SlotObserved::Absent | SlotObserved::ExactOwned) => {
            if slot.observed == SlotObserved::Absent {
                Ok(SlotAction::Preserve)
            } else {
                Ok(SlotAction::Remove)
            }
        }
        (_, SlotObserved::Conflict) => Err(fail("slot conflict")),
    }
}

fn slot_index(slot: ConfigSlot) -> usize {
    match slot {
        ConfigSlot::PromptSubmit => 0,
        ConfigSlot::ToolCompleted => 1,
        ConfigSlot::TurnCompleted => 2,
    }
}

fn is_canonical_uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|parsed| parsed.to_string() == value)
}

fn is_sha256(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|hex| is_lower_hex(hex, 64))
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn deserialize_optional_revision<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    String::deserialize(deserializer).map(Some)
}

fn missing_nullable_string() -> Option<String> {
    // Distinguish an omitted required nullable field from an explicit JSON null.
    Some(String::new())
}

fn fail(reason: &'static str) -> RegistryFailure {
    RegistryFailure::new("config_profile", reason)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const BINDING: &str = "00000000-0000-4000-8000-000000000001";
    const INSTALLATION: &str = "00000000-0000-4000-8000-000000000002";
    const REF_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const REF_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const REF_C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const DIGEST: &str = "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

    fn request(
        intent: ConfigIntent,
        desired: DesiredSlot,
        observed: SlotObserved,
    ) -> ProfileRequest {
        ProfileRequest {
            schema_version: 1,
            intent,
            binding_id: BINDING.into(),
            installation_id: INSTALLATION.into(),
            target_revision: None,
            slots: [
                (ConfigSlot::PromptSubmit, REF_A),
                (ConfigSlot::ToolCompleted, REF_B),
                (ConfigSlot::TurnCompleted, REF_C),
            ]
            .into_iter()
            .map(|(slot, callback_ref)| SlotRequest {
                slot,
                callback_ref: callback_ref.into(),
                desired,
                observed,
                owned_digest: (observed == SlotObserved::ExactOwned).then(|| DIGEST.into()),
            })
            .collect(),
        }
    }

    #[test]
    fn builtin_builder_maps_each_supported_state_and_placement() {
        let install = build_plan(&request(
            ConfigIntent::Install,
            DesiredSlot::Absent,
            SlotObserved::Absent,
        ))
        .unwrap();
        assert!(install.changes().iter().all(|change| {
            change.action == SlotAction::Preserve
                && change.placement.event == change.slot.native_event()
                && change.placement.matcher.is_none()
        }));

        let enable = build_plan(&request(
            ConfigIntent::Enable,
            DesiredSlot::Present,
            SlotObserved::Absent,
        ))
        .unwrap();
        assert!(enable
            .changes()
            .iter()
            .all(|change| change.action == SlotAction::Add));

        let disable = build_plan(&request(
            ConfigIntent::Disable,
            DesiredSlot::Absent,
            SlotObserved::ExactOwned,
        ))
        .unwrap();
        assert!(disable
            .changes()
            .iter()
            .all(|change| change.action == SlotAction::Remove));
    }

    #[test]
    fn independent_validator_rejects_crossed_callbacks_wrong_actions_and_events() {
        let request = request(
            ConfigIntent::Enable,
            DesiredSlot::Present,
            SlotObserved::Absent,
        );
        let valid = build_plan(&request).unwrap().plan().clone();

        let mut crossed = valid.clone();
        crossed.changes[0].callback_ref = REF_B.into();
        assert!(validate_plan(&request, crossed).is_err());

        let mut wrong_action = valid.clone();
        wrong_action.changes[0].action = SlotAction::Remove;
        assert!(validate_plan(&request, wrong_action).is_err());

        let mut wrong_event = valid;
        wrong_event.changes[0].placement.event = "Stop".into();
        assert!(validate_plan(&request, wrong_event).is_err());

        let valid = build_plan(&request).unwrap().plan().clone();
        let mut duplicate = valid.clone();
        duplicate.changes[2].slot = duplicate.changes[1].slot;
        assert!(validate_plan(&request, duplicate).is_err());

        let mut missing = valid.clone();
        missing.changes.pop();
        assert!(validate_plan(&request, missing).is_err());

        let mut bad_identity = valid;
        bad_identity.installation_id = INSTALLATION.replace('2', "3");
        assert!(validate_plan(&request, bad_identity).is_err());
    }

    #[test]
    fn malformed_slot_sets_and_conflicting_ownership_refuse() {
        let mut missing = request(
            ConfigIntent::Install,
            DesiredSlot::Absent,
            SlotObserved::Absent,
        );
        missing.slots.pop();
        assert!(build_plan(&missing).is_err());

        let mut duplicate = request(
            ConfigIntent::Install,
            DesiredSlot::Absent,
            SlotObserved::Absent,
        );
        duplicate.slots[2].slot = duplicate.slots[1].slot;
        assert!(build_plan(&duplicate).is_err());

        assert!(build_plan(&request(
            ConfigIntent::Enable,
            DesiredSlot::Present,
            SlotObserved::Conflict,
        ))
        .is_err());
    }

    #[test]
    fn identity_revision_callback_digest_and_intent_coherence_are_checked() {
        let mut invalid = request(
            ConfigIntent::Install,
            DesiredSlot::Absent,
            SlotObserved::Absent,
        );
        invalid.binding_id = "not-a-uuid".into();
        assert!(build_plan(&invalid).is_err());

        let mut invalid = request(
            ConfigIntent::Install,
            DesiredSlot::Absent,
            SlotObserved::Absent,
        );
        invalid.target_revision = Some("SHA256:bad".into());
        assert!(build_plan(&invalid).is_err());

        let mut invalid = request(
            ConfigIntent::Install,
            DesiredSlot::Absent,
            SlotObserved::Absent,
        );
        invalid.slots[0].callback_ref = "ABC".into();
        assert!(build_plan(&invalid).is_err());

        let mut invalid = request(
            ConfigIntent::Enable,
            DesiredSlot::Present,
            SlotObserved::ExactOwned,
        );
        invalid.slots[0].owned_digest = Some("sha256:bad".into());
        assert!(build_plan(&invalid).is_err());

        let inconsistent = request(
            ConfigIntent::Install,
            DesiredSlot::Present,
            SlotObserved::Absent,
        );
        assert!(build_plan(&inconsistent).is_err());
    }

    #[test]
    fn serde_closes_request_and_plan_and_rejects_unknown_schema_versions() {
        let raw = json!({
            "schema_version": 1,
            "intent": "install",
            "binding_id": BINDING,
            "installation_id": INSTALLATION,
            "slots": [] ,
            "surprise": true
        });
        assert!(serde_json::from_value::<ProfileRequest>(raw).is_err());

        let raw_null_revision = json!({
            "schema_version": 1,
            "intent": "install",
            "binding_id": BINDING,
            "installation_id": INSTALLATION,
            "target_revision": null,
            "slots": []
        });
        assert!(serde_json::from_value::<ProfileRequest>(raw_null_revision).is_err());

        let mut invalid = request(
            ConfigIntent::Install,
            DesiredSlot::Absent,
            SlotObserved::Absent,
        );
        invalid.schema_version = 2;
        assert!(build_plan(&invalid).is_err());

        let mut plan = build_plan(&request(
            ConfigIntent::Install,
            DesiredSlot::Absent,
            SlotObserved::Absent,
        ))
        .unwrap()
        .plan()
        .clone();
        plan.schema_version = 2;
        assert!(validate_plan(
            &request(
                ConfigIntent::Install,
                DesiredSlot::Absent,
                SlotObserved::Absent
            ),
            plan
        )
        .is_err());

        let raw_plan_unknown = json!({
            "schema_version": 1,
            "binding_id": BINDING,
            "installation_id": INSTALLATION,
            "changes": [],
            "extra": false
        });
        assert!(serde_json::from_value::<ProfilePlan>(raw_plan_unknown).is_err());
    }

    #[test]
    fn absent_and_present_target_revisions_round_trip_through_validation() {
        for target_revision in [None, Some(DIGEST.to_owned())] {
            let mut request = request(
                ConfigIntent::Enable,
                DesiredSlot::Present,
                SlotObserved::Absent,
            );
            request.target_revision = target_revision;

            let request_json = serde_json::to_vec(&request).unwrap();
            let decoded_request: ProfileRequest = serde_json::from_slice(&request_json).unwrap();
            assert_eq!(decoded_request, request);
            let plan = build_plan(&decoded_request).unwrap();

            let plan_json = serde_json::to_vec(plan.plan()).unwrap();
            let decoded_plan: ProfilePlan = serde_json::from_slice(&plan_json).unwrap();
            assert_eq!(decoded_plan, *plan.plan());
            let validated = validate_plan(&decoded_request, decoded_plan).unwrap();
            assert_eq!(validated.plan().target_revision, request.target_revision);
        }
    }

    #[test]
    fn missing_required_nullable_slot_digest_deserializes_to_refusal_sentinel() {
        let raw = json!({
            "schema_version": 1,
            "intent": "install",
            "binding_id": BINDING,
            "installation_id": INSTALLATION,
            "slots": [
                {"slot":"prompt_submit", "callback_ref":REF_A, "desired":"absent", "observed":"absent"},
                {"slot":"tool_completed", "callback_ref":REF_B, "desired":"absent", "observed":"absent", "owned_digest":null},
                {"slot":"turn_completed", "callback_ref":REF_C, "desired":"absent", "observed":"absent", "owned_digest":null}
            ]
        });
        let decoded: ProfileRequest = serde_json::from_value(raw).unwrap();
        assert!(build_plan(&decoded).is_err());
        let projected: ProfileRequest =
            serde_json::from_slice(&serde_json::to_vec(&decoded).unwrap()).unwrap();
        assert!(build_plan(&projected).is_err());
    }
}
