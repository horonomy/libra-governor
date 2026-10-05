//! Bounded offline validation for CanonicalHostEvent v1 and its snapshot.
//!
//! The shared JSON Schemas are bundled as contract provenance. This module
//! implements only their event/snapshot wire shapes and delegates identity
//! semantics to `libra-governor-domain`; it is not a general schema engine.

use std::collections::{BTreeMap, HashSet};

use libra_governor_domain::ExecutionIdentity;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Number, Value};

const MAX_EVENT_BYTES: usize = 65_536;
const MAX_EVENT_DEPTH: usize = 16;
const MAX_EVENT_NODES: usize = 4_096;

#[allow(dead_code)]
const CANONICAL_EVENT_SCHEMA: &str =
    include_str!("../contracts/host-adapter/v1/canonical-host-event.schema.json");
#[allow(dead_code)]
const HOST_CAPABILITY_SNAPSHOT_SCHEMA: &str =
    include_str!("../contracts/host-adapter/v1/host-capability-snapshot.schema.json");

/// Where a bounded failure occurred. It deliberately contains no input text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostBindingStage {
    Json,
    Event,
    Snapshot,
    Context,
}

/// Stable, product-local refusal reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostBindingReason {
    MalformedJson,
    DuplicateJsonKey,
    InputTooLarge,
    InputTooDeep,
    TooManyNodes,
    InvalidEvent,
    InvalidIdentity,
    InvalidSnapshot,
    ContextMismatch,
    UnsupportedNativeEvent,
    MissingNativeContext,
    MissingAttributionIdentity,
    UnsupportedEventKind,
    UnsupportedUsage,
}

/// A redacted failure result. No payload value or parser diagnostic is retained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostBindingFailure {
    pub stage: HostBindingStage,
    pub reason: HostBindingReason,
}

impl HostBindingFailure {
    fn new(stage: HostBindingStage, reason: HostBindingReason) -> Self {
        Self { stage, reason }
    }
}

#[derive(Debug)]
struct UniqueValue(Value);

impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(UniqueValueVisitor)
    }
}

fn nullable_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)
}

struct UniqueValueVisitor;

impl<'de> Visitor<'de> for UniqueValueVisitor {
    type Value = UniqueValue;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a bounded JSON value with unique object keys")
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(UniqueValue(Value::Null))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_unit()
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Number(Number::from(value))))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Number(Number::from(value))))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Number::from_f64(value)
            .map(|number| UniqueValue(Value::Number(number)))
            .ok_or_else(|| E::custom("non-finite number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::String(value.to_owned())))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::String(value)))
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = seq.next_element::<UniqueValue>()? {
            values.push(value.0);
        }
        Ok(UniqueValue(Value::Array(values)))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = Map::new();
        let mut keys = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key.clone()) {
                return Err(de::Error::custom("duplicate key"));
            }
            let value = map.next_value::<UniqueValue>()?;
            values.insert(key, value.0);
        }
        Ok(UniqueValue(Value::Object(values)))
    }
}

pub fn validate_host_json(bytes: &[u8]) -> Result<Value, HostBindingFailure> {
    validate_bounded_json(bytes, MAX_EVENT_BYTES, MAX_EVENT_DEPTH, MAX_EVENT_NODES)
}

/// Parse product-owned JSON profiles without duplicate keys or implicit bounds changes.
/// This validates syntax and limits only; callers retain their own schema/semantic checks.
pub fn validate_bounded_json(
    bytes: &[u8],
    max_bytes: usize,
    max_depth: usize,
    max_nodes: usize,
) -> Result<Value, HostBindingFailure> {
    if bytes.len() > max_bytes {
        return Err(HostBindingFailure::new(
            HostBindingStage::Json,
            HostBindingReason::InputTooLarge,
        ));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| {
        HostBindingFailure::new(HostBindingStage::Json, HostBindingReason::MalformedJson)
    })?;
    scan_json_limits(text.as_bytes(), max_depth, max_nodes)?;
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let value = UniqueValue::deserialize(&mut deserializer).map_err(|error| {
        let reason = if error.to_string().contains("duplicate key") {
            HostBindingReason::DuplicateJsonKey
        } else {
            HostBindingReason::MalformedJson
        };
        HostBindingFailure::new(HostBindingStage::Json, reason)
    })?;
    deserializer.end().map_err(|_| {
        HostBindingFailure::new(HostBindingStage::Json, HostBindingReason::MalformedJson)
    })?;
    Ok(value.0)
}

/// Pre-scans syntax delimiters before serde allocation to enforce public bounds.
fn scan_json_limits(
    bytes: &[u8],
    max_depth: usize,
    max_nodes: usize,
) -> Result<(), HostBindingFailure> {
    let mut in_string = false;
    let mut escaped = false;
    let mut in_atom = false;
    let mut depth = 0usize;
    let mut nodes = 0usize;
    for byte in bytes.iter().copied() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
                in_atom = false;
            }
            continue;
        }
        match byte {
            b'"' => {
                nodes += 1;
                in_string = true;
                in_atom = false;
            }
            b'{' | b'[' => {
                nodes += 1;
                depth += 1;
                in_atom = false;
                if depth > max_depth {
                    return Err(HostBindingFailure::new(
                        HostBindingStage::Json,
                        HostBindingReason::InputTooDeep,
                    ));
                }
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                in_atom = false;
            }
            b',' | b':' | b' ' | b'\n' | b'\r' | b'\t' => in_atom = false,
            _ if !in_atom => {
                nodes += 1;
                in_atom = true;
            }
            _ => {}
        }
        if nodes > max_nodes {
            return Err(HostBindingFailure::new(
                HostBindingStage::Json,
                HostBindingReason::TooManyNodes,
            ));
        }
    }
    Ok(())
}

fn optional_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    String::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostEventScope {
    Host,
    Session,
    Agent,
    TurnTask,
    ProjectWorktree,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostEventKind {
    Lifecycle,
    ToolBefore,
    ToolAfter,
    ToolFailure,
    Usage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostEventQuality {
    Literal,
    Reconstructed,
    Heuristic,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostEventSource {
    pub kind: HostEventSourceKind,
    pub native_event_name: String,
    #[serde(
        default,
        deserialize_with = "optional_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub native_schema_ref: Option<String>,
    #[serde(
        default,
        deserialize_with = "optional_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub native_event_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "optional_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub replay_key: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostEventSourceKind {
    Hook,
    Rollout,
    Statusline,
    Cli,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostEventWire {
    schema_version: u32,
    event_id: String,
    observed_at: String,
    adapter_id: String,
    adapter_version: String,
    #[serde(
        default,
        deserialize_with = "optional_string",
        skip_serializing_if = "Option::is_none"
    )]
    host_version: Option<String>,
    source: HostEventSource,
    capability_snapshot_id: String,
    identity: ExecutionIdentity,
    scope: HostEventScope,
    kind: HostEventKind,
    facts: Value,
    quality: HostEventQuality,
    field_provenance: BTreeMap<String, String>,
}

/// A canonical event after closed-shape, bounds, and identity-v1 validation.
#[derive(Debug, Clone)]
pub struct ValidatedHostEvent(HostEventWire, Value);

impl Serialize for ValidatedHostEvent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut value = serde_json::to_value(&self.0).map_err(serde::ser::Error::custom)?;
        // Keep the bounded canonical observation lossless. The owning identity
        // serializer has a different timestamp precision.
        value["identity"] = self.1.clone();
        value.serialize(serializer)
    }
}

impl ValidatedHostEvent {
    pub fn event_id(&self) -> &str {
        &self.0.event_id
    }

    pub fn observed_at(&self) -> &str {
        &self.0.observed_at
    }

    pub fn adapter_id(&self) -> &str {
        &self.0.adapter_id
    }

    pub fn adapter_version(&self) -> &str {
        &self.0.adapter_version
    }

    pub fn host_version(&self) -> Option<&str> {
        self.0.host_version.as_deref()
    }

    pub fn source(&self) -> &HostEventSource {
        &self.0.source
    }

    pub fn capability_snapshot_id(&self) -> &str {
        &self.0.capability_snapshot_id
    }

    pub fn identity(&self) -> &ExecutionIdentity {
        &self.0.identity
    }

    pub fn scope(&self) -> HostEventScope {
        self.0.scope
    }

    pub fn kind(&self) -> HostEventKind {
        self.0.kind
    }

    pub fn facts(&self) -> &Value {
        &self.0.facts
    }

    pub fn quality(&self) -> HostEventQuality {
        self.0.quality
    }

    pub fn field_provenance(&self) -> &BTreeMap<String, String> {
        &self.0.field_provenance
    }
}

pub fn validate_host_event(bytes: &[u8]) -> Result<ValidatedHostEvent, HostBindingFailure> {
    let mut value = validate_host_json(bytes)?;
    if !is_version_one(&value["schema_version"]) {
        return Err(HostBindingFailure::new(
            HostBindingStage::Event,
            HostBindingReason::InvalidEvent,
        ));
    }
    let mut raw_identity = value
        .get("identity")
        .cloned()
        .ok_or_else(invalid_canonical_identity)?;
    validate_canonical_identity(&raw_identity)?;
    // Identity v1 accepts additive fields but ignores them. Retain only its
    // known factual projection, including lossless observation timestamps.
    raw_identity
        .as_object_mut()
        .ok_or_else(invalid_canonical_identity)?
        .retain(|key, _| {
            matches!(
                key.as_str(),
                "envelope_version"
                    | "observed_at"
                    | "host_id"
                    | "tool_provider"
                    | "lineage_status"
                    | "tool_instance_id"
                    | "provider_session_id"
                    | "agent_id"
                    | "turn_id"
                    | "parent_agent_id"
                    | "session_lineage_id"
                    | "event_id"
                    | "repo_id"
                    | "worktree_id"
            )
        });
    raw_identity["envelope_version"] = Value::from(1);
    value["schema_version"] = Value::from(1);
    value["identity"] = raw_identity.clone();
    let wire: HostEventWire = serde_json::from_value(value).map_err(|_| {
        HostBindingFailure::new(HostBindingStage::Event, HostBindingReason::InvalidEvent)
    })?;
    if wire.schema_version != 1
        || wire.event_id.is_empty()
        || wire.capability_snapshot_id.is_empty()
        || wire.adapter_version.is_empty()
        || !valid_adapter_id(&wire.adapter_id)
        || wire.source.native_event_name.is_empty()
        || wire.field_provenance.values().any(String::is_empty)
    {
        return Err(HostBindingFailure::new(
            HostBindingStage::Event,
            HostBindingReason::InvalidEvent,
        ));
    }
    let observed_at = parse_utc_timestamp(&wire.observed_at)?;
    if wire.identity.observed_at() != observed_at.observed_at() {
        return Err(HostBindingFailure::new(
            HostBindingStage::Event,
            HostBindingReason::InvalidIdentity,
        ));
    }
    validate_event_facts(wire.kind, &wire.facts)?;
    Ok(ValidatedHostEvent(wire, raw_identity))
}

fn is_version_one(value: &Value) -> bool {
    value.is_number() && value.as_f64() == Some(1.0)
}

fn invalid_canonical_identity() -> HostBindingFailure {
    HostBindingFailure::new(HostBindingStage::Event, HostBindingReason::InvalidIdentity)
}

fn validate_canonical_identity(value: &Value) -> Result<(), HostBindingFailure> {
    let fields = value.as_object().ok_or_else(invalid_canonical_identity)?;
    if !fields.get("envelope_version").is_some_and(is_version_one) {
        return Err(invalid_canonical_identity());
    }
    for key in ["observed_at", "host_id", "tool_provider", "lineage_status"] {
        if !fields.get(key).is_some_and(Value::is_string) {
            return Err(invalid_canonical_identity());
        }
    }
    let observed = fields["observed_at"]
        .as_str()
        .ok_or_else(invalid_canonical_identity)?;
    if !observed.ends_with('Z') {
        return Err(invalid_canonical_identity());
    }
    let provider = fields["tool_provider"]
        .as_str()
        .ok_or_else(invalid_canonical_identity)?;
    if provider.len() > 32
        || !valid_adapter_id(provider)
        || fields["host_id"].as_str().is_none_or(str::is_empty)
    {
        return Err(invalid_canonical_identity());
    }
    for key in [
        "tool_instance_id",
        "provider_session_id",
        "agent_id",
        "turn_id",
        "parent_agent_id",
        "session_lineage_id",
        "event_id",
        "repo_id",
        "worktree_id",
    ] {
        if fields.get(key).is_some_and(|v| !v.is_string()) {
            return Err(invalid_canonical_identity());
        }
    }
    match fields["lineage_status"].as_str() {
        Some("child") => {
            if fields
                .get("parent_agent_id")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                return Err(invalid_canonical_identity());
            }
        }
        Some("root" | "unknown") => {
            if fields.contains_key("parent_agent_id") {
                return Err(invalid_canonical_identity());
            }
        }
        _ => return Err(invalid_canonical_identity()),
    }
    Ok(())
}

fn parse_utc_timestamp(value: &str) -> Result<ExecutionIdentity, HostBindingFailure> {
    if !value.ends_with('Z') {
        return Err(HostBindingFailure::new(
            HostBindingStage::Event,
            HostBindingReason::InvalidEvent,
        ));
    }
    let identity = serde_json::json!({
        "envelope_version": 1,
        "observed_at": value,
        "host_id": "timestamp-validation",
        "tool_provider": "codex",
        "lineage_status": "unknown"
    });
    serde_json::from_value(identity).map_err(|_| {
        HostBindingFailure::new(HostBindingStage::Event, HostBindingReason::InvalidEvent)
    })
}

fn valid_adapter_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(byte))
}

fn object(value: &Value) -> Result<&Map<String, Value>, HostBindingFailure> {
    value.as_object().ok_or_else(|| {
        HostBindingFailure::new(HostBindingStage::Event, HostBindingReason::InvalidEvent)
    })
}

fn closed_keys(map: &Map<String, Value>, allowed: &[&str]) -> Result<(), HostBindingFailure> {
    if map.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(HostBindingFailure::new(
            HostBindingStage::Event,
            HostBindingReason::InvalidEvent,
        ));
    }
    Ok(())
}

fn required_string<'a>(
    map: &'a Map<String, Value>,
    key: &str,
    nonempty: bool,
) -> Result<&'a str, HostBindingFailure> {
    let value = map.get(key).and_then(Value::as_str).ok_or_else(|| {
        HostBindingFailure::new(HostBindingStage::Event, HostBindingReason::InvalidEvent)
    })?;
    if nonempty && value.is_empty() {
        return Err(HostBindingFailure::new(
            HostBindingStage::Event,
            HostBindingReason::InvalidEvent,
        ));
    }
    Ok(value)
}

fn optional_string_field(
    map: &Map<String, Value>,
    key: &str,
    nonempty: bool,
) -> Result<(), HostBindingFailure> {
    if let Some(value) = map.get(key) {
        let value = value.as_str().ok_or_else(|| {
            HostBindingFailure::new(HostBindingStage::Event, HostBindingReason::InvalidEvent)
        })?;
        if nonempty && value.is_empty() {
            return Err(HostBindingFailure::new(
                HostBindingStage::Event,
                HostBindingReason::InvalidEvent,
            ));
        }
    }
    Ok(())
}

fn enum_field<'a>(
    map: &'a Map<String, Value>,
    key: &str,
    values: &[&str],
) -> Result<&'a str, HostBindingFailure> {
    let value = required_string(map, key, false)?;
    if !values.contains(&value) {
        return Err(HostBindingFailure::new(
            HostBindingStage::Event,
            HostBindingReason::InvalidEvent,
        ));
    }
    Ok(value)
}

fn validate_event_facts(kind: HostEventKind, facts: &Value) -> Result<(), HostBindingFailure> {
    let facts = object(facts)?;
    match kind {
        HostEventKind::Lifecycle => {
            closed_keys(facts, &["event_type", "native_lifecycle_id"])?;
            enum_field(
                facts,
                "event_type",
                &[
                    "session_start",
                    "session_end",
                    "turn_start",
                    "turn_end",
                    "subagent_start",
                    "subagent_end",
                    "interrupt",
                    "compact",
                ],
            )?;
            optional_string_field(facts, "native_lifecycle_id", false)?;
        }
        HostEventKind::ToolBefore | HostEventKind::ToolAfter | HostEventKind::ToolFailure => {
            closed_keys(
                facts,
                &[
                    "tool_name",
                    "native_call_id",
                    "action_kind",
                    "operand_completeness",
                    "working_directory",
                    "operands",
                    "operands_availability",
                    "result_availability",
                    "result",
                    "failure_code",
                ],
            )?;
            required_string(facts, "tool_name", true)?;
            optional_string_field(facts, "native_call_id", false)?;
            optional_string_field(facts, "working_directory", true)?;
            optional_string_field(facts, "failure_code", false)?;
            let action = facts
                .get("action_kind")
                .map(|_| {
                    enum_field(
                        facts,
                        "action_kind",
                        &["shell_command", "file_mutation", "unknown"],
                    )
                })
                .transpose()?;
            let completeness = facts
                .get("operand_completeness")
                .map(|_| {
                    enum_field(
                        facts,
                        "operand_completeness",
                        &["complete", "partial", "unknown"],
                    )
                })
                .transpose()?;
            facts
                .get("operands_availability")
                .map(|_| {
                    enum_field(
                        facts,
                        "operands_availability",
                        &["observed", "unavailable", "redacted", "not_provided"],
                    )
                })
                .transpose()?;
            facts
                .get("result_availability")
                .map(|_| {
                    enum_field(
                        facts,
                        "result_availability",
                        &["observed", "unavailable", "redacted", "not_provided"],
                    )
                })
                .transpose()?;
            let operands = facts.get("operands");
            if let Some(operands) = operands {
                validate_operands(operands)?;
            }
            if operands
                .is_some_and(|value| action == Some("shell_command") && !is_command_operands(value))
            {
                return Err(invalid_event());
            }
            if operands
                .is_some_and(|value| action == Some("file_mutation") && !is_file_operands(value))
            {
                return Err(invalid_event());
            }
            if completeness == Some("complete")
                && (!matches!(action, Some("shell_command" | "file_mutation"))
                    || operands.is_none()
                    || facts.get("operands_availability").and_then(Value::as_str)
                        != Some("observed"))
            {
                return Err(invalid_event());
            }
            if action == Some("unknown")
                && (completeness.is_some_and(|value| !matches!(value, "partial" | "unknown"))
                    || operands.is_some())
            {
                return Err(invalid_event());
            }
        }
        HostEventKind::Usage => {
            closed_keys(
                facts,
                &[
                    "measure",
                    "amount",
                    "unit",
                    "aggregation",
                    "observation_scope",
                    "model",
                ],
            )?;
            required_string(facts, "measure", true)?;
            required_string(facts, "unit", true)?;
            enum_field(facts, "aggregation", &["delta", "cumulative"])?;
            enum_field(
                facts,
                "observation_scope",
                &[
                    "host",
                    "session",
                    "agent",
                    "turn_task",
                    "project_worktree",
                    "unknown",
                ],
            )?;
            optional_string_field(facts, "model", false)?;
            let amount = facts
                .get("amount")
                .and_then(Value::as_number)
                .and_then(Number::as_f64);
            if amount.is_none_or(|amount| !amount.is_finite() || amount < 0.0) {
                return Err(invalid_event());
            }
        }
    }
    Ok(())
}

fn invalid_event() -> HostBindingFailure {
    HostBindingFailure::new(HostBindingStage::Event, HostBindingReason::InvalidEvent)
}

fn is_command_operands(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|map| map.contains_key("command"))
}

fn is_file_operands(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|map| map.contains_key("files"))
}

fn validate_operands(value: &Value) -> Result<(), HostBindingFailure> {
    let operands = object(value)?;
    if operands.len() == 1 && operands.contains_key("command") {
        closed_keys(operands, &["command"])?;
        required_string(operands, "command", true)?;
        return Ok(());
    }
    if operands.len() == 1 && operands.contains_key("files") {
        closed_keys(operands, &["files"])?;
        let files = operands
            .get("files")
            .and_then(Value::as_array)
            .ok_or_else(invalid_event)?;
        if files.is_empty() {
            return Err(invalid_event());
        }
        for file in files {
            let file = object(file)?;
            closed_keys(file, &["operation", "path", "destination_path"])?;
            let operation = enum_field(file, "operation", &["create", "update", "delete", "move"])?;
            required_string(file, "path", true)?;
            if operation == "move" {
                required_string(file, "destination_path", true)?;
            } else if file.contains_key("destination_path") {
                return Err(invalid_event());
            }
        }
        return Ok(());
    }
    Err(invalid_event())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostCapabilityState {
    Supported,
    Limited,
    #[serde(rename = "trust-required")]
    TrustRequired,
    #[serde(rename = "admin-disabled")]
    AdminDisabled,
    Unavailable,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostVersionSource {
    Native,
    HostSchema,
    RuntimeProbe,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostTrustState {
    Trusted,
    Untrusted,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCapabilitySnapshotAdapter {
    pub id: String,
    pub version: String,
    pub manifest_digest: String,
    pub implementation_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCapabilitySnapshotHost {
    pub tool_provider: String,
    #[serde(deserialize_with = "nullable_string")]
    pub version: Option<String>,
    pub version_source: HostVersionSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCapabilitySnapshotContext {
    pub scope: HostEventScope,
    #[serde(
        default,
        deserialize_with = "optional_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub profile: Option<String>,
    #[serde(
        default,
        deserialize_with = "optional_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub project_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCapabilitySnapshotLifecycle {
    pub registered: bool,
    pub installed: bool,
    pub enabled: bool,
    pub adapter_trust: HostTrustState,
    pub host_trust: HostTrustState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCapabilityEvidence {
    pub kind: HostCapabilityEvidenceKind,
    pub observed_at: String,
    pub source_ref: String,
    pub result: String,
    #[serde(
        default,
        deserialize_with = "optional_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub digest: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostCapabilityEvidenceKind {
    Manifest,
    HostSchema,
    HostVersion,
    RuntimeProbe,
    ConfigObservation,
    TrustObservation,
    AdminObservation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCapability {
    pub key: String,
    pub state: HostCapabilityState,
    pub reason_codes: Vec<String>,
    pub limits: Map<String, Value>,
    pub evidence: Vec<HostCapabilityEvidence>,
    #[serde(
        default,
        deserialize_with = "optional_string",
        skip_serializing_if = "Option::is_none"
    )]
    pub last_success_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCapabilitySnapshot {
    pub schema_version: u32,
    pub snapshot_id: String,
    pub observed_at: String,
    pub adapter: HostCapabilitySnapshotAdapter,
    pub host: HostCapabilitySnapshotHost,
    pub context: HostCapabilitySnapshotContext,
    pub lifecycle: HostCapabilitySnapshotLifecycle,
    pub capabilities: Vec<HostCapability>,
}

/// A shape-validated snapshot value. It contains declarations/evidence only,
/// never private runtime provenance or effective authority.
#[derive(Debug, Clone, Serialize)]
pub struct ValidatedHostCapabilitySnapshot(HostCapabilitySnapshot);

impl ValidatedHostCapabilitySnapshot {
    pub fn snapshot(&self) -> &HostCapabilitySnapshot {
        &self.0
    }
}

pub fn validate_host_snapshot(
    bytes: &[u8],
) -> Result<ValidatedHostCapabilitySnapshot, HostBindingFailure> {
    let mut value = validate_host_json(bytes).map_err(|_| {
        HostBindingFailure::new(
            HostBindingStage::Snapshot,
            HostBindingReason::InvalidSnapshot,
        )
    })?;
    if !is_version_one(&value["schema_version"]) {
        return Err(HostBindingFailure::new(
            HostBindingStage::Snapshot,
            HostBindingReason::InvalidSnapshot,
        ));
    }
    value["schema_version"] = Value::from(1);
    let snapshot: HostCapabilitySnapshot = serde_json::from_value(value).map_err(|_| {
        HostBindingFailure::new(
            HostBindingStage::Snapshot,
            HostBindingReason::InvalidSnapshot,
        )
    })?;
    if snapshot.schema_version != 1
        || snapshot.snapshot_id.is_empty()
        || !valid_adapter_id(&snapshot.adapter.id)
        || snapshot.adapter.version.is_empty()
        || !valid_digest(&snapshot.adapter.manifest_digest)
        || !valid_digest(&snapshot.adapter.implementation_digest)
        || !valid_provider(&snapshot.host.tool_provider)
        || snapshot.host.version.as_ref().is_some_and(String::is_empty)
        || snapshot.capabilities.iter().any(|capability| {
            !valid_capability_key(&capability.key)
                || capability
                    .reason_codes
                    .iter()
                    .any(|reason| !valid_reason_code(reason))
                || !valid_capability_limits(&capability.limits)
                || capability.evidence.iter().any(|evidence| {
                    evidence.observed_at.is_empty()
                        || evidence.source_ref.is_empty()
                        || evidence.result.is_empty()
                        || evidence
                            .digest
                            .as_ref()
                            .is_some_and(|digest| !valid_digest(digest))
                })
        })
    {
        return Err(HostBindingFailure::new(
            HostBindingStage::Snapshot,
            HostBindingReason::InvalidSnapshot,
        ));
    }
    parse_utc_timestamp(&snapshot.observed_at).map_err(|_| {
        HostBindingFailure::new(
            HostBindingStage::Snapshot,
            HostBindingReason::InvalidSnapshot,
        )
    })?;
    for capability in &snapshot.capabilities {
        for evidence in &capability.evidence {
            parse_utc_timestamp(&evidence.observed_at).map_err(|_| {
                HostBindingFailure::new(
                    HostBindingStage::Snapshot,
                    HostBindingReason::InvalidSnapshot,
                )
            })?;
        }
        if let Some(timestamp) = capability.last_success_at.as_deref() {
            parse_utc_timestamp(timestamp).map_err(|_| {
                HostBindingFailure::new(
                    HostBindingStage::Snapshot,
                    HostBindingReason::InvalidSnapshot,
                )
            })?;
        }
    }
    Ok(ValidatedHostCapabilitySnapshot(snapshot))
}

fn valid_provider(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 32
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(byte))
}

fn valid_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_capability_key(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_.-".contains(byte))
}

fn valid_reason_code(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_.-".contains(byte))
}

fn valid_capability_limits(limits: &Map<String, Value>) -> bool {
    let string_arrays_valid = ["event_classes", "tool_classes"].iter().all(|key| {
        limits.get(*key).is_none_or(|value| {
            value.as_array().is_some_and(|items| {
                items
                    .iter()
                    .all(|item| item.as_str().is_some_and(|s| !s.is_empty()))
            })
        })
    });
    let execution_mode_valid = limits.get("execution_mode").is_none_or(|value| {
        value.as_str().is_some_and(|value| {
            matches!(value, "synchronous" | "asynchronous" | "mixed" | "unknown")
        })
    });
    let failure_behavior_valid = limits.get("failure_behavior").is_none_or(|value| {
        value
            .as_str()
            .is_some_and(|value| matches!(value, "fail_open" | "fail_closed" | "skip" | "unknown"))
    });
    string_arrays_valid && execution_mode_valid && failure_behavior_valid
}

#[cfg(test)]
mod tests {
    use super::*;

    const OBSERVED_AT: &str = "2026-10-05T00:00:00.000Z";

    fn event(kind: &str, facts: Value) -> Value {
        serde_json::json!({
            "schema_version": 1,
            "event_id": "observation-1",
            "observed_at": OBSERVED_AT,
            "adapter_id": "codex",
            "adapter_version": "1.0.0",
            "source": {"kind": "hook", "native_event_name": "Stop"},
            "capability_snapshot_id": "snapshot-1",
            "identity": {
                "envelope_version": 1,
                "observed_at": OBSERVED_AT,
                "host_id": "host-1",
                "tool_provider": "codex",
                "lineage_status": "unknown",
                "future_identity_extension": {"opaque": true}
            },
            "scope": "turn_task",
            "kind": kind,
            "facts": facts,
            "quality": "reconstructed",
            "field_provenance": {"source.kind": "native_hook"}
        })
    }

    fn snapshot() -> Value {
        serde_json::json!({
            "schema_version": 1,
            "snapshot_id": "snapshot-1",
            "observed_at": "2026-10-04T23:59:59.000Z",
            "adapter": {
                "id": "codex",
                "version": "1.0.0",
                "manifest_digest": format!("sha256:{}", "a".repeat(64)),
                "implementation_digest": format!("sha256:{}", "b".repeat(64))
            },
            "host": {"tool_provider": "codex", "version": null, "version_source": "unknown"},
            "context": {"scope": "turn_task"},
            "lifecycle": {
                "registered": true,
                "installed": true,
                "enabled": false,
                "adapter_trust": "unknown",
                "host_trust": "unknown"
            },
            "capabilities": [{
                "key": "tool.post.observe",
                "state": "unknown",
                "reason_codes": ["native_boundary_unverified"],
                "limits": {"event_classes": ["PostToolUse"], "future_limit": true},
                "evidence": [{
                    "kind": "host_schema",
                    "observed_at": "2026-10-04T23:59:59.000Z",
                    "source_ref": "test-contract-vector",
                    "result": "shape_only"
                }]
            }]
        })
    }

    #[test]
    fn canonical_event_kind_branches_validate_without_promoting_facts() {
        let samples = [
            event("lifecycle", serde_json::json!({"event_type": "turn_start"})),
            event("lifecycle", serde_json::json!({"event_type": "turn_end"})),
            event("tool_before", serde_json::json!({"tool_name": "Bash"})),
            event(
                "tool_after",
                serde_json::json!({"tool_name": "Bash", "native_call_id": "call-1"}),
            ),
            event(
                "tool_failure",
                serde_json::json!({"tool_name": "Edit", "failure_code": "unknown"}),
            ),
            event(
                "usage",
                serde_json::json!({
                    "measure": "input_tokens",
                    "amount": 0,
                    "unit": "token",
                    "aggregation": "delta",
                    "observation_scope": "turn_task"
                }),
            ),
        ];
        for sample in samples {
            let validated = validate_host_event(sample.to_string().as_bytes()).unwrap();
            assert_eq!(validated.event_id(), "observation-1");
            assert_eq!(validated.identity().tool_provider(), "codex");
        }

        let partial = event(
            "tool_after",
            serde_json::json!({
                "tool_name": "Bash",
                "action_kind": "shell_command",
                "operand_completeness": "partial"
            }),
        );
        assert!(validate_host_event(partial.to_string().as_bytes()).is_ok());
    }

    #[test]
    fn schema_and_identity_single_faults_fail_closed_but_identity_extensions_are_ignored() {
        let valid = event("lifecycle", serde_json::json!({"event_type": "turn_start"}));
        assert!(validate_host_event(valid.to_string().as_bytes()).is_ok());

        let mut invalids = Vec::new();
        for (key, value) in [
            ("schema_version", serde_json::json!(2)),
            ("adapter_id", serde_json::json!("Codex")),
            ("scope", serde_json::json!("session_id")),
            ("kind", serde_json::json!("session_end")),
            ("future_top_level", serde_json::json!(true)),
        ] {
            let mut changed = valid.clone();
            changed
                .as_object_mut()
                .unwrap()
                .insert(key.to_string(), value);
            invalids.push(changed);
        }
        let mut absent_parent = valid.clone();
        absent_parent["identity"]["lineage_status"] = serde_json::json!("child");
        invalids.push(absent_parent);
        let mut unexpected_parent = valid.clone();
        unexpected_parent["identity"]["parent_agent_id"] = serde_json::json!("parent-1");
        invalids.push(unexpected_parent);
        let mut mismatched_time = valid.clone();
        mismatched_time["observed_at"] = serde_json::json!("2026-10-05T00:00:01.000Z");
        invalids.push(mismatched_time);
        let mut malformed_facts = valid;
        malformed_facts["facts"]["unexpected"] = serde_json::json!("field");
        invalids.push(malformed_facts);
        for invalid in invalids {
            assert!(validate_host_event(invalid.to_string().as_bytes()).is_err());
        }
    }

    #[test]
    fn json_duplicate_utf8_size_depth_and_node_limits_are_enforced() {
        let duplicate = br#"{"schema_version":1,"schema_version":1}"#;
        assert_eq!(
            validate_host_event(duplicate).unwrap_err().reason,
            HostBindingReason::DuplicateJsonKey
        );
        assert!(validate_host_event(&[0xff]).is_err());
        assert_eq!(
            validate_host_event(&vec![b' '; MAX_EVENT_BYTES + 1])
                .unwrap_err()
                .reason,
            HostBindingReason::InputTooLarge
        );
        let deeply_nested = format!("{{\"x\":{}}}", "[".repeat(17) + "0" + &"]".repeat(17));
        assert_eq!(
            validate_host_event(deeply_nested.as_bytes())
                .unwrap_err()
                .reason,
            HostBindingReason::InputTooDeep
        );
        let many_nodes = serde_json::json!({"x": (0..MAX_EVENT_NODES).collect::<Vec<_>>()});
        assert_eq!(
            validate_host_event(many_nodes.to_string().as_bytes())
                .unwrap_err()
                .reason,
            HostBindingReason::TooManyNodes
        );
        assert!(validate_host_event(br#"{"amount":1e9999}"#).is_err());
    }

    #[test]
    fn fact_profile_conditionals_reject_inconsistent_claims() {
        let invalid = [
            event("lifecycle", serde_json::json!({"event_type": "complete"})),
            event(
                "tool_after",
                serde_json::json!({
                    "tool_name": "Bash",
                    "action_kind": "shell_command",
                    "operands": {"files": [{"operation": "update", "path": "x"}]}
                }),
            ),
            event(
                "tool_after",
                serde_json::json!({"tool_name": "Bash", "action_kind": "unknown", "operands": {"command": "ls"}}),
            ),
            event(
                "tool_after",
                serde_json::json!({
                    "tool_name": "Edit",
                    "action_kind": "file_mutation",
                    "operand_completeness": "complete",
                    "operands_availability": "observed",
                    "operands": {"files": [{"operation": "move", "path": "a"}]}
                }),
            ),
            event(
                "usage",
                serde_json::json!({
                    "measure": "input_tokens",
                    "amount": -1,
                    "unit": "token",
                    "aggregation": "delta",
                    "observation_scope": "turn_task"
                }),
            ),
            event(
                "usage",
                serde_json::json!({
                    "measure": "input_tokens",
                    "amount": 1,
                    "unit": "token",
                    "aggregation": "delta",
                    "observation_scope": "session_id"
                }),
            ),
        ];
        for value in invalid {
            assert_eq!(
                validate_host_event(value.to_string().as_bytes())
                    .unwrap_err()
                    .reason,
                HostBindingReason::InvalidEvent
            );
        }

        let valid_partial = event(
            "tool_after",
            serde_json::json!({
                "tool_name": "Bash",
                "action_kind": "shell_command",
                "operand_completeness": "partial",
                "operands": {"command": "echo safe"}
            }),
        );
        assert!(validate_host_event(valid_partial.to_string().as_bytes()).is_ok());
    }

    #[test]
    fn snapshot_schema_is_validated_but_unknown_support_remains_unknown() {
        let input = snapshot();
        let validated = validate_host_snapshot(input.to_string().as_bytes()).unwrap();
        assert_eq!(validated.snapshot().snapshot_id, "snapshot-1");
        assert_eq!(
            validated.snapshot().capabilities[0].state,
            HostCapabilityState::Unknown
        );

        for mutate in [
            ("schema_version", serde_json::json!(2)),
            ("snapshot_id", serde_json::json!("")),
        ] {
            let mut invalid = input.clone();
            invalid[mutate.0] = mutate.1;
            assert!(validate_host_snapshot(invalid.to_string().as_bytes()).is_err());
        }
        let mut invalid_digest = input.clone();
        invalid_digest["adapter"]["manifest_digest"] = serde_json::json!("sha256:bad");
        assert!(validate_host_snapshot(invalid_digest.to_string().as_bytes()).is_err());
        let mut invalid_limit = input;
        invalid_limit["capabilities"][0]["limits"]["execution_mode"] =
            serde_json::json!("optimistic");
        assert!(validate_host_snapshot(invalid_limit.to_string().as_bytes()).is_err());
    }

    #[test]
    fn bundled_schema_resources_are_from_the_pinned_public_revision() {
        let provenance: Value = serde_json::from_str(include_str!(
            "../contracts/host-adapter/v1/source-provenance.json"
        ))
        .unwrap();
        assert_eq!(
            provenance["source_revision"],
            "98f7cf041e89bed8ba10484470cfb48592ff1e77"
        );
        let event_schema: Value = serde_json::from_str(CANONICAL_EVENT_SCHEMA).unwrap();
        let snapshot_schema: Value = serde_json::from_str(HOST_CAPABILITY_SNAPSHOT_SCHEMA).unwrap();
        assert_eq!(
            event_schema["$id"],
            "https://horonomy.github.io/contracts/host-adapter/v1/canonical-host-event.schema.json"
        );
        assert_eq!(
            snapshot_schema["$id"],
            "https://horonomy.github.io/contracts/host-adapter/v1/host-capability-snapshot.schema.json"
        );
    }
    #[test]
    fn version_constants_accept_schema_numeric_equivalence_without_defaulting() {
        let base = event("lifecycle", serde_json::json!({"event_type":"turn_start"}));
        for token in ["1", "1.0", "1e0", "1.00e+0"] {
            for key in ["schema_version", "envelope_version"] {
                let raw = base
                    .to_string()
                    .replace(&format!("\"{key}\":1"), &format!("\"{key}\":{token}"));
                let parsed = validate_host_event(raw.as_bytes()).unwrap();
                let serialized = serde_json::to_vec(&parsed).unwrap();
                assert!(validate_host_event(&serialized).is_ok());
            }
            let raw = snapshot().to_string().replace(
                "\"schema_version\":1",
                &format!("\"schema_version\":{token}"),
            );
            let parsed = validate_host_snapshot(raw.as_bytes()).unwrap();
            assert!(
                validate_host_snapshot(&serde_json::to_vec(parsed.snapshot()).unwrap()).is_ok()
            );
        }
        for bad in [
            Value::Null,
            serde_json::json!(true),
            serde_json::json!("1"),
            serde_json::json!(0),
            serde_json::json!(-1),
            serde_json::json!(2),
            serde_json::json!(1.5),
        ] {
            let mut raw = base.clone();
            raw["schema_version"] = bad.clone();
            assert!(validate_host_event(raw.to_string().as_bytes()).is_err());
            raw = base.clone();
            raw["identity"]["envelope_version"] = bad.clone();
            assert!(validate_host_event(raw.to_string().as_bytes()).is_err());
            let mut raw = snapshot();
            raw["schema_version"] = bad;
            assert!(validate_host_snapshot(raw.to_string().as_bytes()).is_err());
        }
        for pointer in ["/schema_version", "/identity/envelope_version"] {
            let mut raw = base.clone();
            let (parent, key) = pointer.rsplit_once('/').unwrap();
            raw.pointer_mut(parent)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove(key);
            assert!(validate_host_event(raw.to_string().as_bytes()).is_err());
        }
        let mut raw = snapshot();
        raw.as_object_mut().unwrap().remove("schema_version");
        assert!(validate_host_snapshot(raw.to_string().as_bytes()).is_err());
    }

    #[test]
    fn serialization_omits_absent_fields_and_preserves_known_identity_precision() {
        let mut raw = event("lifecycle", serde_json::json!({"event_type":"turn_start"}));
        let precise = "2026-10-05T00:00:00.123456789Z";
        raw["observed_at"] = Value::from(precise);
        raw["identity"]["observed_at"] = Value::from(precise);
        raw["identity"]["provider_session_id"] = Value::from("native-session");
        let parsed = validate_host_event(raw.to_string().as_bytes()).unwrap();
        let serialized = serde_json::to_value(parsed).unwrap();
        assert_eq!(serialized["identity"]["observed_at"], precise);
        assert_eq!(
            serialized["identity"]["provider_session_id"],
            "native-session"
        );
        assert!(serialized["identity"]
            .get("future_identity_extension")
            .is_none());
        assert!(serialized.get("host_version").is_none());
        assert!(serialized["source"].get("native_schema_ref").is_none());
        assert!(validate_host_event(serialized.to_string().as_bytes()).is_ok());
        let snapshot = validate_host_snapshot(snapshot().to_string().as_bytes()).unwrap();
        let serialized = serde_json::to_value(snapshot.snapshot()).unwrap();
        assert!(serialized["context"].get("profile").is_none());
        assert!(serialized["context"].get("project_ref").is_none());
        assert!(serialized["host"]
            .get("version")
            .is_some_and(Value::is_null));
        assert!(validate_host_snapshot(serialized.to_string().as_bytes()).is_ok());
    }

    #[test]
    fn raw_identity_optional_types_and_lineage_constraints_still_reject() {
        let base = event("lifecycle", serde_json::json!({"event_type":"turn_start"}));
        for key in [
            "tool_instance_id",
            "provider_session_id",
            "agent_id",
            "turn_id",
            "parent_agent_id",
            "session_lineage_id",
            "event_id",
            "repo_id",
            "worktree_id",
        ] {
            let mut raw = base.clone();
            raw["identity"][key] = Value::Null;
            assert!(validate_host_event(raw.to_string().as_bytes()).is_err());
        }
        for provider in [
            "UpperCase",
            "invalid.provider",
            "abcdefghijklmnopqrstuvwxyz0123456789",
        ] {
            let mut raw = base.clone();
            raw["identity"]["tool_provider"] = Value::from(provider);
            assert!(validate_host_event(raw.to_string().as_bytes()).is_err());
        }
        let mut raw = base;
        raw["identity"]["observed_at"] = Value::from("2026-10-05T00:00:00.000+00:00");
        assert!(validate_host_event(raw.to_string().as_bytes()).is_err());
    }
    #[test]
    fn bundled_contract_schemas_match_the_reviewed_canonical_pins() {
        use sha2::{Digest, Sha256};
        let event = include_bytes!("../contracts/host-adapter/v1/canonical-host-event.schema.json");
        let snapshot =
            include_bytes!("../contracts/host-adapter/v1/host-capability-snapshot.schema.json");
        assert_eq!(
            Sha256::digest(event)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
            "38ff7b5426781b10e7743c4909609d285b12f4080901f5263ca0c54aa42ac94a"
        );
        assert_eq!(
            Sha256::digest(snapshot)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
            "41fea89b3c854745a4d8b8c41e06b84bfd56ff8e4c4d18da642a351d652210b2"
        );
    }

    #[test]
    fn public_contract_vectors_preserve_partial_unknown_and_external_facts() {
        for bytes in [
            include_bytes!(
                "../contracts/host-adapter/v1/fixtures/valid-event-codex-tool-before.json"
            )
            .as_slice(),
            include_bytes!("../contracts/host-adapter/v1/fixtures/valid-event-record-only.json")
                .as_slice(),
            include_bytes!(
                "../contracts/host-adapter/v1/fixtures/valid-event-synthetic-usage.json"
            )
            .as_slice(),
        ] {
            let event = validate_host_event(bytes).unwrap();
            assert!(validate_host_event(&serde_json::to_vec(&event).unwrap()).is_ok());
        }
        for bytes in [
            include_bytes!("../contracts/host-adapter/v1/fixtures/valid-snapshot-claude-trust-required.json").as_slice(),
            include_bytes!("../contracts/host-adapter/v1/fixtures/valid-snapshot-admin-disabled.json").as_slice(),
            include_bytes!("../contracts/host-adapter/v1/fixtures/valid-snapshot-codex-shape-only-unknown.json").as_slice(),
        ] {
            let snapshot = validate_host_snapshot(bytes).unwrap();
            assert!(validate_host_snapshot(&serde_json::to_vec(snapshot.snapshot()).unwrap()).is_ok());
        }
        for bytes in [
            include_bytes!(
                "../contracts/host-adapter/v1/fixtures/invalid-event-absent-identity.json"
            )
            .as_slice(),
            include_bytes!("../contracts/host-adapter/v1/fixtures/invalid-malformed-event.json")
                .as_slice(),
        ] {
            assert!(validate_host_event(bytes).is_err());
        }
    }
}
