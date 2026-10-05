use libra_governor_daemon::host_runtime::contract::HostContract;
use serde_json::{json, Value};

const MANIFEST: &str =
    include_str!("../../protocol/contracts/host-adapter/v1/fixtures/valid-manifest-synthetic.json");
const PROTOCOL: &str =
    include_str!("../../protocol/contracts/host-adapter/v1/fixtures/valid-protocol.json");

fn contract() -> HostContract {
    HostContract::load().expect("bundled pinned contracts compile")
}

#[test]
fn pinned_manifest_cli_and_protocol_vectors_validate_and_correlate() {
    let c = contract();
    let manifest = c.validate_manifest(MANIFEST.as_bytes()).unwrap();
    assert_eq!(manifest.adapter_id(), "synthetic_external");
    assert!(manifest.compatible());
    assert_eq!(manifest.raw(), MANIFEST.as_bytes());
    assert!(manifest.digest().starts_with("sha256:"));

    let cli: Value = serde_json::from_str(include_str!(
        "../../protocol/contracts/host-adapter/v1/fixtures/valid-cli-envelope.json"
    ))
    .unwrap();
    c.validate_cli_envelope(&cli).unwrap();

    let vectors: Value = serde_json::from_str(PROTOCOL).unwrap();
    let request = serde_json::to_vec(&vectors["operation_request"]).unwrap();
    let response = serde_json::to_vec(&vectors["operation_response"]).unwrap();
    let parsed = c.validate_request(&request).unwrap();
    let correlated = c.validate_response(&parsed, &response).unwrap();
    assert_eq!(correlated["request_id"], "req-2");
    let mut mismatched = vectors["operation_response"].clone();
    mismatched["request_id"] = json!("other");
    assert!(c
        .validate_response(&parsed, &serde_json::to_vec(&mismatched).unwrap())
        .is_err());
}

#[test]
fn manifest_future_versions_remain_inspectable_but_incompatible() {
    let c = contract();
    let mut value: Value = serde_json::from_str(MANIFEST).unwrap();
    value["protocol_versions"] = json!([2]);
    value["contract_version_range"] = json!({"minimum":2,"maximum":3});
    let raw = serde_json::to_vec(&value).unwrap();
    let manifest = c.validate_manifest(&raw).unwrap();
    assert!(!manifest.compatible());
    assert!(manifest.value().is_object());
}

#[test]
fn manifest_host_ranges_use_semver_precedence_and_reject_duplicate_families() {
    let c = contract();
    let mut manifest: Value = serde_json::from_str(MANIFEST).unwrap();
    manifest["host_version_constraints"][0]["minimum"] = json!("1.0.0+z");
    manifest["host_version_constraints"][0]["maximum"] = json!("1.0.0+a");
    assert!(c
        .validate_manifest(&serde_json::to_vec(&manifest).unwrap())
        .is_ok());
    manifest["host_version_constraints"]
        .as_array_mut()
        .unwrap()
        .push(json!({"provider":"synthetic_host","minimum":null,"maximum":null}));
    assert!(c
        .validate_manifest(&serde_json::to_vec(&manifest).unwrap())
        .is_err());
}

#[test]
fn manifest_rejects_schema_mutations_duplicate_json_and_numeric_boolean() {
    let c = contract();
    let mut value: Value = serde_json::from_str(MANIFEST).unwrap();
    value["launch"]["unexpected"] = json!(true);
    assert!(c
        .validate_manifest(&serde_json::to_vec(&value).unwrap())
        .is_err());
    let mut value: Value = serde_json::from_str(MANIFEST).unwrap();
    value["manifest_version"] = json!(true);
    assert!(c
        .validate_manifest(&serde_json::to_vec(&value).unwrap())
        .is_err());
    assert!(c.validate_manifest(br#"{"a":1,"a":2}"#).is_err());
    assert!(c.validate_manifest(&vec![b' '; 65_537]).is_err());
    let numeric_one = MANIFEST.replacen("\"manifest_version\":1", "\"manifest_version\":1.0", 1);
    assert!(c.validate_manifest(numeric_one.as_bytes()).is_ok());
}

#[test]
fn settings_profile_is_checked_then_real_schema_validation_runs_without_defaults() {
    let c = contract();
    let mut value: Value = serde_json::from_str(MANIFEST).unwrap();
    value["configuration_schema"]["properties"] = json!({"$ref":{"type":"string"}});
    value["configuration_schema"]["additionalProperties"] = json!(false);
    let manifest = c
        .validate_manifest(&serde_json::to_vec(&value).unwrap())
        .unwrap();
    assert_eq!(
        c.validate_settings(&manifest, br#"{"$ref":"data"}"#)
            .unwrap(),
        json!({"$ref":"data"})
    );
    assert!(c
        .validate_settings(&manifest, br#"{"other":true}"#)
        .is_err());

    value["configuration_schema"]["properties"]["$ref"]["pattern"] = json!(".*");
    assert!(c
        .validate_manifest(&serde_json::to_vec(&value).unwrap())
        .is_err());
    value["configuration_schema"]["properties"]["$ref"]
        .as_object_mut()
        .unwrap()
        .remove("pattern");
    value["configuration_schema"]["properties"]["$ref"]["default"] = json!("implicit");
    assert!(c
        .validate_manifest(&serde_json::to_vec(&value).unwrap())
        .is_err());
    value["configuration_schema"]["properties"]["$ref"] = json!({"type":"number","enum":[1,1.0]});
    assert!(c
        .validate_manifest(&serde_json::to_vec(&value).unwrap())
        .is_err());
}

#[test]
fn manifest_ref_keywords_fail_before_any_external_schema_retrieval() {
    let c = contract();
    let temp = tempfile::tempdir().unwrap();
    let sentinel = temp.path().join("must-not-be-read.json");
    std::fs::write(&sentinel, br#"{"type":"string"}"#).unwrap();
    let local_ref = format!("file://{}", sentinel.display());

    for reference in [local_ref, "https://invalid.invalid/schema.json".to_owned()] {
        let raw = manifest_with_property_schema(&format!("{{\"$ref\":\"{reference}\"}}"));
        let failure = c.validate_manifest(raw.as_bytes()).unwrap_err();
        assert_eq!(failure.stage, "settings_schema");
        assert_eq!(failure.reason, "schema keyword outside bounded profile");
        assert_eq!(std::fs::read(&sentinel).unwrap(), br#"{"type":"string"}"#);
    }
}

#[test]
fn embedded_configuration_has_independent_byte_depth_and_node_limits() {
    let c = contract();
    let config = |value: Value| {
        json!({"protocol":"horonom.host-adapter","protocol_version":1,"host_contract_version":1,
            "request_id":"embedded-limits","operation":"probe","input":{"context":{"scope":"session"}},
            "configuration":value})
    };
    let request = |value: Value| serde_json::to_vec(&config(value)).unwrap();

    let accepted_bytes = request(json!({"data":"x".repeat(65_525)}));
    assert!(c.validate_request(&accepted_bytes).is_ok());
    let rejected_bytes = request(json!({"data":"x".repeat(65_526)}));
    assert!(c.validate_request(&rejected_bytes).is_err());

    let nested = |levels: usize| {
        let mut value = Value::Null;
        for _ in 0..levels {
            value = json!([value]);
        }
        json!({"nested":value})
    };
    assert!(c.validate_request(&request(nested(15))).is_ok());
    assert!(c.validate_request(&request(nested(16))).is_err());

    assert!(c
        .validate_request(&request(json!({"values":vec![0; 4_093]})))
        .is_ok());
    assert!(c
        .validate_request(&request(json!({"values":vec![0; 4_094]})))
        .is_err());
}

#[test]
fn cardinality_keywords_accept_mathematical_integers_and_reject_invalid_numbers() {
    let c = contract();
    for (schema, settings) in [
        (r#"{"type":"string","minLength":1e0}"#, r#"{"setting":"x"}"#),
        (r#"{"type":"array","maxItems":1.0}"#, r#"{"setting":[1]}"#),
        (
            r#"{"type":"object","minProperties":1e0,"additionalProperties":true}"#,
            r#"{"setting":{"x":1}}"#,
        ),
        (
            r#"{"type":"object","maxProperties":1.0,"additionalProperties":true}"#,
            r#"{"setting":{"x":1}}"#,
        ),
    ] {
        let raw = manifest_with_property_schema(schema);
        let manifest = c.validate_manifest(raw.as_bytes()).unwrap();
        c.validate_settings(&manifest, settings.as_bytes()).unwrap();
    }

    for cardinality in ["-1", "0.5", "true"] {
        let schema = format!(r#"{{"type":"array","minItems":{cardinality}}}"#);
        let raw = manifest_with_property_schema(&schema);
        assert!(
            c.validate_manifest(raw.as_bytes()).is_err(),
            "{cardinality}"
        );
    }
    let contradictory =
        manifest_with_property_schema(r#"{"type":"array","minItems":1e0,"maxItems":0.0}"#);
    assert!(c.validate_manifest(contradictory.as_bytes()).is_err());
}

fn manifest_with_property_schema(property_schema: &str) -> String {
    let source = MANIFEST;
    let marker = "\"configuration_schema\":{\"$schema\":\"https://json-schema.org/draft/2020-12/schema\",\"type\":\"object\",\"properties\":{},\"additionalProperties\":false}";
    let replacement = format!(
        "\"configuration_schema\":{{\"$schema\":\"https://json-schema.org/draft/2020-12/schema\",\"type\":\"object\",\"properties\":{{\"setting\":{property_schema}}},\"additionalProperties\":false}}"
    );
    assert!(
        source.contains(marker),
        "fixture schema marker must remain stable"
    );
    source.replacen(marker, &replacement, 1)
}

#[test]
fn settings_profile_rejects_misplaced_or_inconsistent_bounds_before_manifest_projection() {
    let c = contract();
    for invalid_node in [
        json!({"type":"string","minimum":1}),
        json!({"type":"string","minLength":8,"maxLength":2}),
        json!({"type":"number","minimum":3,"exclusiveMaximum":2}),
        json!({"type":"number","exclusiveMinimum":2,"exclusiveMaximum":2}),
        json!({"type":"number","minimum":9007199254740993u64,"maximum":9007199254740992u64}),
        json!({"type":"number","enum":[1,1.0]}),
        json!({"type":"number","enum":[]}),
        json!({"type":"object","additionalProperties":false,"required":["x","x"]}),
        json!({"type":"object","additionalProperties":false,"const":{"x":1}}),
    ] {
        let mut manifest: Value = serde_json::from_str(MANIFEST).unwrap();
        manifest["configuration_schema"]["properties"] = json!({"x":invalid_node});
        assert!(c
            .validate_manifest(&serde_json::to_vec(&manifest).unwrap())
            .is_err());
    }
    let mut manifest: Value = serde_json::from_str(MANIFEST).unwrap();
    manifest["configuration_schema"]["type"] = json!("string");
    assert!(c
        .validate_manifest(&serde_json::to_vec(&manifest).unwrap())
        .is_err());
}

#[test]
fn request_and_response_enforce_bounds_class_and_selected_versions() {
    let c = contract();
    let deep = format!("{}0{}", "[".repeat(33), "]".repeat(33));
    assert!(c.validate_request(deep.as_bytes()).is_err());

    let request = json!({"protocol":"horonom.host-adapter","protocol_version":1.0,"host_contract_version":1.0,
        "request_id":"n1","operation":"probe","input":{"context":{"scope":"session"}}});
    assert!(c
        .validate_request(&serde_json::to_vec(&request).unwrap())
        .is_ok());
    let oversized_configuration = json!({"protocol":"horonom.host-adapter","protocol_version":1,"host_contract_version":1,
        "request_id":"large","operation":"probe","input":{"context":{"scope":"session"}},
        "configuration":{"large":"x".repeat(70_000)}});
    assert!(c
        .validate_request(&serde_json::to_vec(&oversized_configuration).unwrap())
        .is_err());
    let valid_response = response("n1", json!({"snapshot":snapshot()}));
    c.validate_response(&request, &serde_json::to_vec(&valid_response).unwrap())
        .unwrap();
    let mut wrong_version = valid_response.clone();
    wrong_version["protocol_version"] = json!(true);
    assert!(c
        .validate_response(&request, &serde_json::to_vec(&wrong_version).unwrap())
        .is_err());
    let mismatch = normalize_request("n1");
    c.validate_request(&serde_json::to_vec(&mismatch).unwrap())
        .unwrap();
    c.validate_response(
        &mismatch,
        &serde_json::to_vec(&response("n1", json!({"events":[event()]}))).unwrap(),
    )
    .unwrap();
    assert!(c
        .validate_response(&mismatch, &serde_json::to_vec(&valid_response).unwrap())
        .is_err());
    let request_bytes = serde_json::to_vec(&request).unwrap();
    assert!(c.validate_response(&request, &request_bytes).is_err());
    assert!(c
        .validate_request(&serde_json::to_vec(&valid_response).unwrap())
        .is_err());

    let handshake = json!({"protocol":"horonom.host-adapter","offered_versions":[1],"request_id":"hs","operation":"handshake"});
    c.validate_request(&serde_json::to_vec(&handshake).unwrap())
        .unwrap();
    let operation_response = response("hs", json!({"snapshot":snapshot()}));
    let mut ordinary = request.clone();
    ordinary["request_id"] = json!("hs");
    c.validate_response(&ordinary, &serde_json::to_vec(&operation_response).unwrap())
        .unwrap();
    assert!(c
        .validate_response(
            &handshake,
            &serde_json::to_vec(&operation_response).unwrap()
        )
        .is_err());
    let valid_handshake_error = json!({"protocol":"horonom.host-adapter","request_id":"hs",
        "error":{"code":"failed","message":"failed"}});
    assert!(c
        .validate_response(
            &handshake,
            &serde_json::to_vec(&valid_handshake_error).unwrap()
        )
        .is_ok());
    let valid_handshake_success =
        json!({"protocol":"horonom.host-adapter","request_id":"hs","selected_version":1});
    c.validate_response(
        &handshake,
        &serde_json::to_vec(&valid_handshake_success).unwrap(),
    )
    .unwrap();

    let operation_error = json!({"protocol":"horonom.host-adapter","protocol_version":1,"host_contract_version":1,
        "request_id":"hs","error":{"code":"unsupported","message":"no capability"}});
    c.validate_response(&ordinary, &serde_json::to_vec(&operation_error).unwrap())
        .unwrap();
    assert!(c
        .validate_response(&handshake, &serde_json::to_vec(&operation_error).unwrap())
        .is_err());
    assert!(c
        .validate_response(
            &ordinary,
            &serde_json::to_vec(&valid_handshake_error).unwrap()
        )
        .is_err());
    assert!(c
        .validate_response(
            &ordinary,
            &serde_json::to_vec(&valid_handshake_success).unwrap()
        )
        .is_err());
}

#[test]
fn protocol_schema_rejects_invalid_calendar_timestamp_in_nested_contract() {
    let c = contract();
    let mut request = normalize_request("bad-time");
    c.validate_request(&serde_json::to_vec(&request).unwrap())
        .unwrap();
    request["input"]["observed_at"] = json!("2026-99-99T99:99:99Z");
    assert!(c
        .validate_request(&serde_json::to_vec(&request).unwrap())
        .is_err());
}

fn snapshot() -> Value {
    serde_json::from_str(include_str!("../../protocol/contracts/host-adapter/v1/fixtures/valid-snapshot-codex-shape-only-unknown.json")).unwrap()
}
fn event() -> Value {
    serde_json::from_str(include_str!(
        "../../protocol/contracts/host-adapter/v1/fixtures/valid-event-record-only.json"
    ))
    .unwrap()
}
fn normalize_request(id: &str) -> Value {
    json!({"protocol":"horonom.host-adapter","protocol_version":1,"host_contract_version":1,
        "request_id":id,"operation":"normalize","input":{"native_payload":{},"source":{"kind":"hook","native_event_name":"PreToolUse"},
        "host_id":"h","observed_at":"2026-10-05T00:00:00Z","capability_snapshot":snapshot()}})
}
fn response(id: &str, result: Value) -> Value {
    json!({"protocol":"horonom.host-adapter","protocol_version":1,"host_contract_version":1,"request_id":id,"result":result})
}

#[test]
fn protocol_nested_snapshot_and_event_use_their_existing_semantic_validators() {
    let c = contract();
    let snapshot: Value = serde_json::from_str(include_str!(
        "../../protocol/contracts/host-adapter/v1/fixtures/valid-snapshot-codex-shape-only-unknown.json"
    ))
    .unwrap();
    let request = json!({"protocol":"horonom.host-adapter","protocol_version":1,"host_contract_version":1,
        "request_id":"snapshot","operation":"normalize","input":{"native_payload":{},"source":{"kind":"hook","native_event_name":"PreToolUse"},
        "host_id":"h","observed_at":"2026-10-05T00:00:00Z","capability_snapshot":snapshot}});
    assert!(c
        .validate_request(&serde_json::to_vec(&request).unwrap())
        .is_ok());
    let mut invalid = request.clone();
    invalid["input"]["capability_snapshot"]["observed_at"] = json!("2026-99-99T99:99:99Z");
    assert!(c
        .validate_request(&serde_json::to_vec(&invalid).unwrap())
        .is_err());

    let event: Value = serde_json::from_str(include_str!(
        "../../protocol/contracts/host-adapter/v1/fixtures/valid-event-record-only.json"
    ))
    .unwrap();
    let response = json!({"protocol":"horonom.host-adapter","protocol_version":1,"host_contract_version":1,
        "request_id":"events","result":{"events":[event]}});
    let event_request = json!({"protocol":"horonom.host-adapter","protocol_version":1,"host_contract_version":1,
        "request_id":"events","operation":"normalize","input":{"native_payload":{},"source":{"kind":"hook","native_event_name":"PreToolUse"},
        "host_id":"h","observed_at":"2026-10-05T00:00:00Z","capability_snapshot":snapshot}});
    assert!(c
        .validate_response(&event_request, &serde_json::to_vec(&response).unwrap())
        .is_ok());
    let mut invalid_response = response;
    invalid_response["result"]["events"][0]["observed_at"] = json!("2026-99-99T99:99:99Z");
    assert!(c
        .validate_response(
            &event_request,
            &serde_json::to_vec(&invalid_response).unwrap()
        )
        .is_err());
}

#[test]
fn response_result_uses_the_pinned_operation_result_schema() {
    let c = contract();
    let request = normalize_request("normalize-result");
    c.validate_request(&serde_json::to_vec(&request).unwrap())
        .unwrap();
    c.validate_response(
        &request,
        &serde_json::to_vec(&response("normalize-result", json!({"events":[event()]}))).unwrap(),
    )
    .unwrap();
    let wrong = response("normalize-result", json!({"snapshot":snapshot()}));
    assert!(c
        .validate_response(&request, &serde_json::to_vec(&wrong).unwrap())
        .is_err());

    let event = event();
    let encode = json!({"protocol":"horonom.host-adapter","protocol_version":1,"host_contract_version":1,"request_id":"encode-result","operation":"encode_control",
        "input":{"intent":{"schema_version":1,"request_id":"encode-result","event_id":event["event_id"],"product_id":"libra","decision_ref":"decision1","action":"allow","required_capability_keys":["tool.boundary"],"required_failure_class":"policy.unavailable"},"event":event,"capability_snapshot":snapshot()}});
    c.validate_request(&serde_json::to_vec(&encode).unwrap())
        .unwrap();
    let good = response(
        "encode-result",
        json!({"schema_version":1,"status":"encoded","event_id":event["event_id"],"action":"allow","native_response":{},"delivery":"encoded"}),
    );
    c.validate_response(&encode, &serde_json::to_vec(&good).unwrap())
        .unwrap();
    for (field, bad) in [
        ("event_id", json!(false)),
        ("action", json!("bogus")),
        ("native_response", Value::Null),
    ] {
        let mut value = good.clone();
        value["result"][field] = bad;
        assert!(c
            .validate_response(&encode, &serde_json::to_vec(&value).unwrap())
            .is_err());
    }
    let validator =
        json!({"id":"libra.config","version":1,"digest":format!("sha256:{}","a".repeat(64))});
    let plan = json!({"protocol":"horonom.host-adapter","protocol_version":1,"host_contract_version":1,"request_id":"plan-result","operation":"plan_config",
        "input":{"product_id":"libra","validator_ref":validator,"context":{"scope":"host"},"request":{}}});
    c.validate_request(&serde_json::to_vec(&plan).unwrap())
        .unwrap();
    let good = response(
        "plan-result",
        json!({"product_id":"libra","validator_ref":validator,"plan":{}}),
    );
    c.validate_response(&plan, &serde_json::to_vec(&good).unwrap())
        .unwrap();
    for (field, bad) in [
        ("product_id", json!("")),
        ("validator_ref", json!({})),
        ("plan", Value::Null),
    ] {
        let mut value = good.clone();
        value["result"][field] = bad;
        assert!(c
            .validate_response(&plan, &serde_json::to_vec(&value).unwrap())
            .is_err());
    }
}

#[test]
fn numeric_profile_orders_zero_fractions_exact_integers_and_mathematical_cardinalities() {
    let c = contract();
    let source: Value = serde_json::from_str(MANIFEST).unwrap();
    for (minimum, maximum, valid) in [
        (json!(0), json!(0.1), true),
        (json!(0.1), json!(0), false),
        (json!(-0.0), json!(0.1), true),
        (json!(-0.1), json!(-0.0), true),
        (json!(-0.0), json!(-0.1), false),
        (
            json!(9007199254740993u64),
            json!(9007199254740992u64),
            false,
        ),
        (json!(9007199254740992u64), json!(9007199254740993u64), true),
    ] {
        let mut value = source.clone();
        value["configuration_schema"]["properties"] =
            json!({"amount":{"type":"number","minimum":minimum,"maximum":maximum}});
        assert_eq!(
            c.validate_manifest(&serde_json::to_vec(&value).unwrap())
                .is_ok(),
            valid
        );
    }
    for keyword in ["minLength", "maxLength"] {
        let mut value = source.clone();
        value["configuration_schema"]["properties"] = json!({"text":{"type":"string",keyword:1.0}});
        let manifest = c
            .validate_manifest(&serde_json::to_vec(&value).unwrap())
            .unwrap();
        c.validate_settings(&manifest, br#"{"text":"x"}"#).unwrap();
    }
    let mut value = source.clone();
    value["configuration_schema"]["properties"] =
        json!({"amount":{"type":"number","exclusiveMinimum":0.0,"maximum":0}});
    assert!(c
        .validate_manifest(&serde_json::to_vec(&value).unwrap())
        .is_err());
    value["configuration_schema"]["properties"] =
        json!({"text":{"type":"string","minLength":2.0,"maxLength":1.0}});
    assert!(c
        .validate_manifest(&serde_json::to_vec(&value).unwrap())
        .is_err());
}

#[test]
fn contract_version_ranges_preserve_order_at_the_pinned_upper_boundary() {
    let c = contract();
    let mut value: Value = serde_json::from_str(MANIFEST).unwrap();
    value["protocol_versions"] = json!([2]);
    value["contract_version_range"] =
        json!({"minimum":2_147_483_646_u64,"maximum":2_147_483_647_u64});
    let valid = c
        .validate_manifest(&serde_json::to_vec(&value).unwrap())
        .unwrap();
    assert!(!valid.compatible());
    value["contract_version_range"] =
        json!({"minimum":2_147_483_647_u64,"maximum":2_147_483_646_u64});
    let error = c
        .validate_manifest(&serde_json::to_vec(&value).unwrap())
        .unwrap_err();
    assert_eq!(error.reason, "invalid contract version range");
}
