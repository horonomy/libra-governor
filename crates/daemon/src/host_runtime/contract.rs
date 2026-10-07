//! Pure, bounded validation for the pinned host-adapter contracts.

use std::sync::Arc;
use std::{cmp::Ordering, collections::HashSet};

use jsonschema::{Draft, Registry, Validator};
use num_cmp::NumCmp;
use semver::Version;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::RegistryFailure;

pub(super) const MAX_MANIFEST_BYTES: usize = 65_536;
const MAX_SETTINGS_BYTES: usize = 65_536;
const MAX_SETTINGS_DEPTH: usize = 16;
const MAX_SETTINGS_NODES: usize = 4_096;
const MAX_PROTOCOL_BYTES: usize = 1_048_576;
const MAX_PROTOCOL_DEPTH: usize = 32;
const MAX_PROTOCOL_NODES: usize = 65_536;

const SCHEMA_EVENT: &str =
    include_str!("../../../protocol/contracts/host-adapter/v1/canonical-host-event.schema.json");
const SCHEMA_SNAPSHOT: &str = include_str!(
    "../../../protocol/contracts/host-adapter/v1/host-capability-snapshot.schema.json"
);
const SCHEMA_MANIFEST: &str = include_str!(
    "../../../protocol/contracts/host-adapter/v1/executable-adapter-manifest.schema.json"
);
const SCHEMA_PROTOCOL: &str =
    include_str!("../../../protocol/contracts/host-adapter/v1/protocol-message.schema.json");
const SCHEMA_CLI: &str =
    include_str!("../../../protocol/contracts/host-adapter/v1/cli-operation-envelope.schema.json");

const PINNED: [(&str, &str); 5] = [
    (
        "canonical-host-event.schema.json",
        "38ff7b5426781b10e7743c4909609d285b12f4080901f5263ca0c54aa42ac94a",
    ),
    (
        "host-capability-snapshot.schema.json",
        "41fea89b3c854745a4d8b8c41e06b84bfd56ff8e4c4d18da642a351d652210b2",
    ),
    (
        "executable-adapter-manifest.schema.json",
        "9d9e66b0ee578097ceeeba34d5219b60169cbb9d4aa51ac7ff1ef2139e626cd1",
    ),
    (
        "protocol-message.schema.json",
        "1f109f74cecb26ac06be93198face6c73a2b41d9df7cec1f93e3b10463ba98a3",
    ),
    (
        "cli-operation-envelope.schema.json",
        "f16bc73c52fa7614d9cd1142d759570aac6fefc75b9cc0864613c2896488af39",
    ),
];

type SchemaValidator = Arc<Validator>;

/// Immutable validators compiled only from the five pinned, package-owned resources.
#[derive(Clone)]
pub struct HostContract {
    manifest: SchemaValidator,
    request: SchemaValidator,
    response: SchemaValidator,
    probe_result: SchemaValidator,
    normalize_result: SchemaValidator,
    normalize_observation: SchemaValidator,
    encode_control_result: SchemaValidator,
    plan_config_result: SchemaValidator,
    cli: SchemaValidator,
    events: SchemaValidator,
    snapshots: SchemaValidator,
}

/// A validated manifest retains its exact source bytes and read-only projections.
#[derive(Clone, Debug)]
pub struct ValidatedManifest {
    raw: Vec<u8>,
    value: Value,
    digest: String,
    adapter_id: String,
    compatible: bool,
}

impl ValidatedManifest {
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }
    pub fn value(&self) -> &Value {
        &self.value
    }
    pub fn digest(&self) -> &str {
        &self.digest
    }
    pub fn adapter_id(&self) -> &str {
        &self.adapter_id
    }
    pub fn compatible(&self) -> bool {
        self.compatible
    }
}

impl HostContract {
    pub fn load() -> Result<Self, RegistryFailure> {
        let sources = [
            SCHEMA_EVENT,
            SCHEMA_SNAPSHOT,
            SCHEMA_MANIFEST,
            SCHEMA_PROTOCOL,
            SCHEMA_CLI,
        ];
        let names = [
            PINNED[0].0,
            PINNED[1].0,
            PINNED[2].0,
            PINNED[3].0,
            PINNED[4].0,
        ];
        let mut parsed = Vec::with_capacity(sources.len());
        for (index, source) in sources.iter().enumerate() {
            let digest = hex_digest(source.as_bytes());
            if digest != PINNED[index].1 || names[index] != PINNED[index].0 {
                return Err(RegistryFailure::new(
                    "resource_integrity",
                    "pinned schema mismatch",
                ));
            }
            let value: Value = serde_json::from_str(source).map_err(|_| {
                RegistryFailure::new("resource_integrity", "invalid bundled schema")
            })?;
            parsed.push(value);
        }

        // Probe required built-in format behavior before accepting the validator configuration.
        let datetime_probe = serde_json::json!({"type":"string","format":"date-time"});
        let datetime = jsonschema::options()
            .with_draft(Draft::Draft202012)
            .should_validate_formats(true)
            .should_ignore_unknown_formats(false)
            .offline()
            .build(&datetime_probe)
            .map_err(|_| {
                RegistryFailure::new("resource_integrity", "date-time validator unavailable")
            })?;
        if datetime.is_valid(&serde_json::json!("2026-99-99T99:99:99Z"))
            || !datetime.is_valid(&serde_json::json!("2026-10-05T12:30:00Z"))
        {
            return Err(RegistryFailure::new(
                "resource_integrity",
                "date-time validator unavailable",
            ));
        }

        let registry = Registry::new().draft(Draft::Draft202012)
            .add("https://horonomy.github.io/contracts/host-adapter/v1/canonical-host-event.schema.json", parsed[0].clone())
            .map_err(|_| RegistryFailure::new("resource_integrity", "schema registry refused resource"))?
            .add("https://horonomy.github.io/contracts/host-adapter/v1/host-capability-snapshot.schema.json", parsed[1].clone())
            .map_err(|_| RegistryFailure::new("resource_integrity", "schema registry refused resource"))?
            .add("https://horonomy.github.io/contracts/host-adapter/v1/executable-adapter-manifest.schema.json", parsed[2].clone())
            .map_err(|_| RegistryFailure::new("resource_integrity", "schema registry refused resource"))?
            .add("https://horonomy.github.io/contracts/host-adapter/v1/protocol-message.schema.json", parsed[3].clone())
            .map_err(|_| RegistryFailure::new("resource_integrity", "schema registry refused resource"))?
            .add("https://horonomy.github.io/contracts/host-adapter/v1/cli-operation-envelope.schema.json", parsed[4].clone())
            .map_err(|_| RegistryFailure::new("resource_integrity", "schema registry refused resource"))?
            .prepare().map_err(|_| RegistryFailure::new("resource_integrity", "schema registry failed"))?;
        let compile = |schema: &Value| -> Result<SchemaValidator, RegistryFailure> {
            jsonschema::options()
                .with_draft(Draft::Draft202012)
                .with_registry(&registry)
                .should_validate_formats(true)
                .should_ignore_unknown_formats(false)
                .offline()
                .build(schema)
                .map(Arc::new)
                .map_err(|_| {
                    RegistryFailure::new("resource_integrity", "schema compilation failed")
                })
        };
        let mut request_schema = parsed[3].clone();
        request_schema["oneOf"] = serde_json::json!([{"$ref":"#/$defs/request"}]);
        let mut response_schema = parsed[3].clone();
        response_schema["oneOf"] = serde_json::json!([{"$ref":"#/$defs/response"}]);
        let result_schema = |name: &str| {
            let mut schema = parsed[3].clone();
            schema
                .as_object_mut()
                .expect("bundled schema object")
                .remove("oneOf");
            schema["$ref"] = Value::String(format!("#/$defs/results/{name}"));
            schema
        };
        // Caller observations cannot supply the runtime-owned snapshot. This
        // private projection retains the pinned definition and its references
        // without registering a modified canonical resource identity.
        let mut observation_schema = parsed[3].clone();
        let object = observation_schema
            .as_object_mut()
            .expect("bundled schema object");
        object.remove("$id");
        object.remove("oneOf");
        observation_schema["$ref"] = Value::String("#/$defs/inputs/normalize".into());
        let definition = &mut observation_schema["$defs"]["inputs"]["normalize"];
        definition["required"]
            .as_array_mut()
            .expect("pinned required fields")
            .retain(|field| field != "capability_snapshot");
        definition["properties"]
            .as_object_mut()
            .expect("pinned properties")
            .remove("capability_snapshot");
        Ok(Self {
            events: compile(&parsed[0])?,
            snapshots: compile(&parsed[1])?,
            manifest: compile(&parsed[2])?,
            request: compile(&request_schema)?,
            response: compile(&response_schema)?,
            probe_result: compile(&result_schema("probe"))?,
            normalize_result: compile(&result_schema("normalize"))?,
            normalize_observation: compile(&observation_schema)?,
            encode_control_result: compile(&result_schema("encode_control"))?,
            plan_config_result: compile(&result_schema("plan_config"))?,
            cli: compile(&parsed[4])?,
        })
    }

    pub(super) fn validate_normalization_observation(
        &self,
        native_payload: &[u8],
        host_id: &str,
        observed_at: &str,
        source: Value,
    ) -> Result<Value, RegistryFailure> {
        let native = parse(
            native_payload,
            MAX_PROTOCOL_BYTES,
            MAX_PROTOCOL_DEPTH,
            MAX_PROTOCOL_NODES,
        )?;
        let input = serde_json::json!({"native_payload":native, "host_id":host_id, "observed_at":observed_at, "source":source});
        let raw =
            serde_json::to_vec(&input).map_err(|_| fail("protocol", "invalid observation"))?;
        let bounded = parse(
            &raw,
            MAX_PROTOCOL_BYTES,
            MAX_PROTOCOL_DEPTH,
            MAX_PROTOCOL_NODES,
        )?;
        validate(&self.normalize_observation, &bounded, "protocol")?;
        Ok(bounded)
    }

    pub(super) fn check_protocol_bounds(&self, raw: &[u8]) -> Result<(), RegistryFailure> {
        parse(
            raw,
            MAX_PROTOCOL_BYTES,
            MAX_PROTOCOL_DEPTH,
            MAX_PROTOCOL_NODES,
        )
        .map(|_| ())
    }

    pub fn validate_manifest(&self, raw: &[u8]) -> Result<ValidatedManifest, RegistryFailure> {
        let value = parse(raw, MAX_MANIFEST_BYTES, 32, 16_384)?;
        bounded_schema(&value["configuration_schema"])?;
        profile(&value["configuration_schema"], true, 0)?;
        validate(&self.manifest, &value, "manifest")?;
        let id = value
            .get("adapter_id")
            .and_then(Value::as_str)
            .ok_or_else(|| fail("manifest", "invalid adapter id"))?
            .to_owned();
        let mut compatible = false;
        if let (Some(versions), Some(range)) = (
            value["protocol_versions"].as_array(),
            value["contract_version_range"].as_object(),
        ) {
            let protocol_ok = versions.iter().any(|v| number_eq(v, 1));
            let min = range.get("minimum").is_some_and(|v| number_eq(v, 1));
            let max = range
                .get("maximum")
                .and_then(Value::as_f64)
                .is_some_and(|v| v >= 1.0);
            compatible = protocol_ok && min && max;
        }
        if numeric_cmp(
            &value["contract_version_range"]["minimum"],
            &value["contract_version_range"]["maximum"],
        ) == Some(Ordering::Greater)
        {
            return Err(fail("manifest", "invalid contract version range"));
        }
        let mut host_families = HashSet::new();
        for item in value["host_version_constraints"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let family = item["provider"]
                .as_str()
                .ok_or_else(|| fail("manifest", "invalid host version family"))?;
            if !host_families.insert(family) {
                return Err(fail("manifest", "duplicate host version family"));
            }
            let min = parse_optional_semver(&item["minimum"])?;
            let max = parse_optional_semver(&item["maximum"])?;
            if min
                .zip(max)
                .is_some_and(|(a, b)| a.cmp_precedence(&b) == Ordering::Greater)
            {
                return Err(fail("manifest", "invalid host version range"));
            }
        }
        Ok(ValidatedManifest {
            raw: raw.to_vec(),
            value,
            digest: format!("sha256:{}", hex_digest(raw)),
            adapter_id: id,
            compatible,
        })
    }

    pub fn validate_settings(
        &self,
        manifest: &ValidatedManifest,
        raw: &[u8],
    ) -> Result<Value, RegistryFailure> {
        let settings = parse(
            raw,
            MAX_SETTINGS_BYTES,
            MAX_SETTINGS_DEPTH,
            MAX_SETTINGS_NODES,
        )?;
        if !settings.is_object() {
            return Err(fail("settings", "settings must be an object"));
        }
        bounded_schema(&manifest.value["configuration_schema"])?;
        profile(&manifest.value["configuration_schema"], true, 0)?;
        let schema = &manifest.value["configuration_schema"];
        let validator = jsonschema::options()
            .with_draft(Draft::Draft202012)
            .should_validate_formats(true)
            .should_ignore_unknown_formats(false)
            .offline()
            .build(schema)
            .map_err(|_| fail("settings", "configuration schema invalid"))?;
        if !validator.is_valid(&settings) {
            return Err(fail("settings", "settings do not match schema"));
        }
        Ok(settings)
    }

    pub fn validate_request(&self, raw: &[u8]) -> Result<Value, RegistryFailure> {
        let value = parse(
            raw,
            MAX_PROTOCOL_BYTES,
            MAX_PROTOCOL_DEPTH,
            MAX_PROTOCOL_NODES,
        )?;
        validate(&self.request, &value, "protocol")?;
        if value.get("operation").and_then(Value::as_str) == Some("handshake") {
            if value.get("configuration").is_some() {
                return Err(fail("protocol", "handshake cannot carry configuration"));
            }
            return Ok(value);
        }
        // Schema-valid unknown future versions remain inspectable, but cannot be treated as callable.
        if !number_eq(&value["protocol_version"], 1)
            || !number_eq(&value["host_contract_version"], 1)
        {
            return Err(fail("protocol", "unsupported selected version"));
        }
        if let Some(configuration) = value.get("configuration") {
            if !configuration.is_object() {
                return Err(fail("protocol", "configuration must be an object"));
            }
            let raw_configuration = serde_json::to_vec(configuration)
                .map_err(|_| fail("protocol", "invalid configuration"))?;
            parse(
                &raw_configuration,
                MAX_SETTINGS_BYTES,
                MAX_SETTINGS_DEPTH,
                MAX_SETTINGS_NODES,
            )?;
        }
        for (path, validator) in [
            ("input.capability_snapshot", &self.snapshots),
            ("input.event", &self.events),
        ] {
            if let Some(nested) = path_value(&value, path) {
                let bytes = serde_json::to_vec(nested)
                    .map_err(|_| fail("protocol", "invalid nested object"))?;
                validate(validator, nested, "protocol")?;
                let result = if path.ends_with("event") {
                    libra_governor_protocol::host_event::validate_host_event(&bytes).map(|_| ())
                } else {
                    libra_governor_protocol::host_event::validate_host_snapshot(&bytes).map(|_| ())
                };
                result.map_err(|_| fail("protocol", "invalid nested host contract"))?;
            }
        }
        Ok(value)
    }

    pub fn validate_response(
        &self,
        expected_request: &Value,
        raw: &[u8],
    ) -> Result<Value, RegistryFailure> {
        let request_bytes = serde_json::to_vec(expected_request)
            .map_err(|_| fail("protocol", "invalid expected request"))?;
        let request = self.validate_request(&request_bytes)?;
        let response = parse(
            raw,
            MAX_PROTOCOL_BYTES,
            MAX_PROTOCOL_DEPTH,
            MAX_PROTOCOL_NODES,
        )?;
        validate(&self.response, &response, "protocol")?;
        if request.get("request_id") != response.get("request_id") {
            return Err(fail("protocol", "request id mismatch"));
        }
        if request["operation"] == "handshake" {
            if response.get("selected_version").is_some() {
                let selected = &response["selected_version"];
                if !request["offered_versions"].as_array().is_some_and(|xs| {
                    xs.iter()
                        .any(|v| number_eq(v, 1) && number_values_equal(v, selected))
                }) {
                    return Err(fail("protocol", "selected version was not offered"));
                }
            } else if response.get("error").is_none()
                || response.get("protocol_version").is_some()
                || response.get("host_contract_version").is_some()
            {
                return Err(fail("protocol", "handshake response class mismatch"));
            }
            return Ok(response);
        }
        if response.get("selected_version").is_some() {
            return Err(fail("protocol", "unexpected handshake response"));
        }
        if !number_values_equal(&response["protocol_version"], &request["protocol_version"])
            || !number_values_equal(
                &response["host_contract_version"],
                &request["host_contract_version"],
            )
        {
            return Err(fail("protocol", "selected version mismatch"));
        }
        if response.get("result").is_some() {
            let operation = request["operation"].as_str().unwrap_or_default();
            let result = &response["result"];
            let result_validator = match operation {
                "probe" => &self.probe_result,
                "normalize" => &self.normalize_result,
                "encode_control" => &self.encode_control_result,
                "plan_config" => &self.plan_config_result,
                _ => return Err(fail("protocol", "operation result mismatch")),
            };
            validate(result_validator, result, "protocol")?;
            if operation == "normalize"
                && result["events"]
                    .as_array()
                    .is_none_or(|events| events.is_empty())
            {
                return Err(fail("protocol", "operation result mismatch"));
            }
            for (field, validator) in [("snapshot", &self.snapshots), ("event", &self.events)] {
                if let Some(nested) = result.get(field) {
                    validate(validator, nested, "protocol")?;
                    let bytes = serde_json::to_vec(nested)
                        .map_err(|_| fail("protocol", "invalid nested object"))?;
                    let checked = if field == "event" {
                        libra_governor_protocol::host_event::validate_host_event(&bytes).map(|_| ())
                    } else {
                        libra_governor_protocol::host_event::validate_host_snapshot(&bytes)
                            .map(|_| ())
                    };
                    checked.map_err(|_| fail("protocol", "invalid nested host contract"))?;
                }
                if field == "event" {
                    if let Some(events) = result.get("events").and_then(Value::as_array) {
                        for nested in events {
                            validate(validator, nested, "protocol")?;
                            let bytes = serde_json::to_vec(nested)
                                .map_err(|_| fail("protocol", "invalid nested object"))?;
                            libra_governor_protocol::host_event::validate_host_event(&bytes)
                                .map_err(|_| fail("protocol", "invalid nested host contract"))?;
                        }
                    }
                }
            }
        }
        Ok(response)
    }

    pub fn validate_cli_envelope(&self, value: &Value) -> Result<(), RegistryFailure> {
        validate(&self.cli, value, "cli")
    }
}

fn parse_optional_semver(value: &Value) -> Result<Option<Version>, RegistryFailure> {
    let Some(text) = value.as_str() else {
        return Ok(None);
    };
    Version::parse(text)
        .map(Some)
        .map_err(|_| fail("manifest", "invalid semantic version"))
}

fn profile(schema: &Value, root: bool, depth: usize) -> Result<(), RegistryFailure> {
    if depth > 16 {
        return Err(fail("settings_schema", "schema profile depth exceeded"));
    }
    let obj = schema
        .as_object()
        .ok_or_else(|| fail("settings_schema", "schema node must be an object"))?;
    const ALLOWED: &[&str] = &[
        "$schema",
        "type",
        "properties",
        "required",
        "additionalProperties",
        "items",
        "enum",
        "const",
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "minLength",
        "maxLength",
        "minItems",
        "maxItems",
        "minProperties",
        "maxProperties",
        "title",
        "description",
    ];
    for key in obj.keys() {
        // '$ref' is a property name inside `properties`, never a schema keyword here.
        if !ALLOWED.contains(&key.as_str()) || (key == "$schema" && !root) {
            return Err(fail(
                "settings_schema",
                "schema keyword outside bounded profile",
            ));
        }
    }
    if root
        && obj.get("$schema").and_then(Value::as_str)
            != Some("https://json-schema.org/draft/2020-12/schema")
    {
        return Err(fail("settings_schema", "unsupported schema dialect"));
    }
    let kind = obj
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| fail("settings_schema", "one schema type required"))?;
    if !matches!(
        kind,
        "object" | "array" | "string" | "number" | "integer" | "boolean" | "null"
    ) {
        return Err(fail("settings_schema", "unsupported schema type"));
    }
    if root && kind != "object" {
        return Err(fail("settings_schema", "root schema must be object"));
    }
    for key in [
        "properties",
        "required",
        "additionalProperties",
        "minProperties",
        "maxProperties",
    ] {
        if obj.contains_key(key) && kind != "object" {
            return Err(fail(
                "settings_schema",
                "object keyword has incompatible type",
            ));
        }
    }
    for key in ["items", "minItems", "maxItems"] {
        if obj.contains_key(key) && kind != "array" {
            return Err(fail(
                "settings_schema",
                "array keyword has incompatible type",
            ));
        }
    }
    for key in ["minLength", "maxLength"] {
        if obj.contains_key(key) && kind != "string" {
            return Err(fail(
                "settings_schema",
                "string keyword has incompatible type",
            ));
        }
    }
    for key in ["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"] {
        if obj.contains_key(key) && !matches!(kind, "integer" | "number") {
            return Err(fail(
                "settings_schema",
                "numeric keyword has incompatible type",
            ));
        }
        if obj.get(key).is_some_and(|value| !value.is_number()) {
            return Err(fail("settings_schema", "numeric keyword must be a number"));
        }
    }
    for key in [
        "minLength",
        "maxLength",
        "minItems",
        "maxItems",
        "minProperties",
        "maxProperties",
    ] {
        if obj
            .get(key)
            .is_some_and(|value| !nonnegative_integer(value))
        {
            return Err(fail(
                "settings_schema",
                "cardinality keyword must be a nonnegative integer",
            ));
        }
    }
    for key in ["title", "description"] {
        if obj.get(key).is_some_and(|value| {
            value
                .as_str()
                .is_none_or(|text| text.chars().count() > 1024)
        }) {
            return Err(fail(
                "settings_schema",
                "annotation must be a bounded string",
            ));
        }
    }
    if kind == "object" {
        if !obj.contains_key("additionalProperties") {
            return Err(fail(
                "settings_schema",
                "object additionalProperties is required",
            ));
        }
        if !obj["additionalProperties"].is_boolean() {
            profile(&obj["additionalProperties"], false, depth + 1)?;
        }
        if let Some(props) = obj.get("properties") {
            let props = props
                .as_object()
                .ok_or_else(|| fail("settings_schema", "properties must be object"))?;
            for child in props.values() {
                profile(child, false, depth + 1)?;
            }
        }
    }
    if kind == "array" {
        if let Some(items) = obj.get("items") {
            profile(items, false, depth + 1)?;
        }
    }
    if let Some(values) = obj.get("enum") {
        let Some(values) = values.as_array() else {
            return Err(fail("settings_schema", "enum must be an array"));
        };
        if values.is_empty()
            || values.len() > 64
            || values.iter().any(|value| !is_scalar(value))
            || values.iter().enumerate().any(|(i, value)| {
                values[..i]
                    .iter()
                    .any(|earlier| number_values_equal(earlier, value))
            })
        {
            return Err(fail("settings_schema", "invalid or duplicate enum"));
        }
    }
    if obj.get("const").is_some_and(|value| !is_scalar(value)) {
        return Err(fail("settings_schema", "const must be scalar"));
    }
    if let Some(required) = obj.get("required") {
        if !required.as_array().is_some_and(|xs| {
            xs.iter().all(Value::is_string)
                && xs
                    .iter()
                    .enumerate()
                    .all(|(i, value)| !xs[..i].contains(value))
        }) {
            return Err(fail("settings_schema", "invalid required list"));
        }
    }
    validate_schema_bounds(obj, kind)?;
    Ok(())
}

fn is_scalar(value: &Value) -> bool {
    matches!(
        value,
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
    )
}

fn validate_schema_bounds(
    obj: &serde_json::Map<String, Value>,
    kind: &str,
) -> Result<(), RegistryFailure> {
    let lower = effective_bound(obj, "minimum", "exclusiveMinimum", true)?;
    let upper = effective_bound(obj, "maximum", "exclusiveMaximum", false)?;
    if let (Some((low, low_exclusive)), Some((high, high_exclusive))) = (lower, upper) {
        match numeric_cmp(low, high)
            .ok_or_else(|| fail("settings_schema", "invalid numeric bound"))?
        {
            Ordering::Greater => {
                return Err(fail("settings_schema", "inconsistent numeric bounds"));
            }
            Ordering::Equal if low_exclusive || high_exclusive => {
                return Err(fail("settings_schema", "empty numeric interval"));
            }
            _ => {}
        }
    }
    let (min_key, max_key) = match kind {
        "string" => ("minLength", "maxLength"),
        "array" => ("minItems", "maxItems"),
        "object" => ("minProperties", "maxProperties"),
        _ => return Ok(()),
    };
    if obj
        .get(min_key)
        .zip(obj.get(max_key))
        .is_some_and(|(min, max)| numeric_cmp(min, max) == Some(Ordering::Greater))
    {
        return Err(fail("settings_schema", "inconsistent cardinality bounds"));
    }
    Ok(())
}

fn effective_bound<'a>(
    obj: &'a serde_json::Map<String, Value>,
    inclusive_key: &str,
    exclusive_key: &str,
    choose_greater: bool,
) -> Result<Option<(&'a Value, bool)>, RegistryFailure> {
    let inclusive = obj.get(inclusive_key).map(|value| (value, false));
    let exclusive = obj.get(exclusive_key).map(|value| (value, true));
    match (inclusive, exclusive) {
        (Some(a), Some(b)) => {
            let order = numeric_cmp(a.0, b.0)
                .ok_or_else(|| fail("settings_schema", "invalid numeric bound"))?;
            let select_a = if choose_greater {
                order == Ordering::Greater
            } else {
                order == Ordering::Less
            };
            if order == Ordering::Equal {
                Ok(Some((a.0, true)))
            } else if select_a {
                Ok(Some(a))
            } else {
                Ok(Some(b))
            }
        }
        (Some(bound), None) | (None, Some(bound)) => Ok(Some(bound)),
        (None, None) => Ok(None),
    }
}

fn numeric_cmp(left: &Value, right: &Value) -> Option<Ordering> {
    fn rhs<T: NumCmp<u64> + NumCmp<i64> + NumCmp<f64>>(
        left: T,
        right: &serde_json::Number,
    ) -> Option<Ordering> {
        if let Some(value) = right.as_u64() {
            left.num_cmp(value)
        } else if let Some(value) = right.as_i64() {
            left.num_cmp(value)
        } else {
            left.num_cmp(right.as_f64()?)
        }
    }
    let (left, right) = (left.as_number()?, right.as_number()?);
    if let Some(value) = left.as_u64() {
        rhs(value, right)
    } else if let Some(value) = left.as_i64() {
        rhs(value, right)
    } else {
        rhs(left.as_f64()?, right)
    }
}

fn nonnegative_integer(value: &Value) -> bool {
    value.as_number().is_some_and(|number| {
        number.as_u64().is_some()
            || number
                .as_f64()
                .is_some_and(|n| n.is_finite() && n >= 0.0 && n.fract() == 0.0)
    })
}

fn bounded_schema(schema: &Value) -> Result<(), RegistryFailure> {
    fn walk(value: &Value, depth: usize, count: &mut usize) -> Result<(), RegistryFailure> {
        if depth > 16 {
            return Err(fail("settings_schema", "schema profile depth exceeded"));
        }
        *count += 1;
        if *count > 4_096 {
            return Err(fail(
                "settings_schema",
                "schema profile node limit exceeded",
            ));
        }
        match value {
            Value::Array(items) => {
                for item in items {
                    walk(item, depth + 1, count)?;
                }
            }
            Value::Object(items) => {
                for item in items.values() {
                    walk(item, depth + 1, count)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    let encoded = serde_json::to_vec(schema)
        .map_err(|_| fail("settings_schema", "schema encoding failed"))?;
    if encoded.len() > MAX_SETTINGS_BYTES {
        return Err(fail("settings_schema", "schema byte limit exceeded"));
    }
    let mut count = 0;
    walk(schema, 0, &mut count)
}

fn path_value<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(v, |at, key| at.get(key))
}
fn number_eq(v: &Value, n: u64) -> bool {
    number_values_equal(v, &serde_json::json!(n))
}
fn number_values_equal(a: &Value, b: &Value) -> bool {
    jsonschema::json::cmp::equal(a, b)
}
fn validate(
    validator: &Validator,
    value: &Value,
    stage: &'static str,
) -> Result<(), RegistryFailure> {
    validator
        .is_valid(value)
        .then_some(())
        .ok_or_else(|| fail(stage, "schema validation failed"))
}
fn parse(
    raw: &[u8],
    max_bytes: usize,
    depth: usize,
    nodes: usize,
) -> Result<Value, RegistryFailure> {
    libra_governor_protocol::host_event::validate_bounded_json(raw, max_bytes, depth, nodes)
        .map_err(|error| {
            let reason = match error.reason {
                libra_governor_protocol::host_event::HostBindingReason::DuplicateJsonKey => {
                    "duplicate JSON key"
                }
                libra_governor_protocol::host_event::HostBindingReason::InputTooLarge => {
                    "input too large"
                }
                libra_governor_protocol::host_event::HostBindingReason::InputTooDeep => {
                    "input too deep"
                }
                libra_governor_protocol::host_event::HostBindingReason::TooManyNodes => {
                    "too many JSON nodes"
                }
                _ => "malformed JSON",
            };
            RegistryFailure::new("json", reason)
        })
}
fn fail(stage: &'static str, reason: &'static str) -> RegistryFailure {
    RegistryFailure::new(stage, reason)
}
fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
