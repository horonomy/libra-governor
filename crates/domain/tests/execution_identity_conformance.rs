//! Runs the Host Integration execution-identity conformance vectors
//! (`horonomy/.github` `governance/product/execution-identity-conformance-vectors.json`,
//! pinned in `tests/fixtures/README.md`) against Libra's own
//! [`ExecutionIdentity`]: wire parsing, round trip, cache keys, display ids,
//! narrowest attributable scope, refusal of invalid envelopes, and
//! correlation. The hook-payload `normalizers[]` group runs in
//! `crates/cli/src/agent/identity_conformance.rs`, where the payload path
//! lives.
//!
//! Libra has no `narrowest_attributable_scope` API; the vector is checked
//! through `cache_key` using the reference ordering (turn_task, agent,
//! session, else unknown) and must never yield `Host`.

use libra_governor_domain::{ExecutionIdentity, Scope};
use serde_json::Value;
use sha2::{Digest, Sha256};

const VECTORS: &str = include_str!("fixtures/execution-identity-conformance-vectors.json");
const PINNED_SHA256: &str = "c6ab9b978540e723a4d338f40b5218f6e355667c35dec456e9b7a650233b5a13";

fn vectors() -> Value {
    serde_json::from_str(VECTORS).expect("vendored vectors are valid JSON")
}

fn group<'a>(doc: &'a Value, name: &str) -> &'a Vec<Value> {
    doc[name].as_array().unwrap_or_else(|| panic!("{name}[]"))
}

fn parse(wire: &Value) -> Result<ExecutionIdentity, serde_json::Error> {
    serde_json::from_value(wire.clone())
}

fn valid_by_name(doc: &Value, name: &str) -> ExecutionIdentity {
    let vector = group(doc, "valid")
        .iter()
        .find(|v| v["name"] == name)
        .unwrap_or_else(|| panic!("no valid vector named {name}"));
    parse(&vector["wire"]).unwrap()
}

#[test]
fn vendored_vectors_match_the_pinned_digest_and_shape() {
    let digest: String = Sha256::digest(VECTORS.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(
        digest, PINNED_SHA256,
        "vendored vectors drifted from the pinned ref; follow tests/fixtures/README.md"
    );
    let doc = vectors();
    assert_eq!(doc["vectors_version"], 1);
    let mut keys: Vec<&str> = doc
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "cache_key_null",
            "contract",
            "correlations",
            "display_id",
            "invalid",
            "normalizers",
            "valid",
            "vectors_version"
        ],
        "a new vector group needs a runner before the pin is updated"
    );
    assert_eq!(group(&doc, "valid").len(), 16);
    assert_eq!(group(&doc, "invalid").len(), 12);
    assert_eq!(group(&doc, "correlations").len(), 8);
    assert_eq!(group(&doc, "normalizers").len(), 10);
}

fn narrowest_attributable_scope(identity: &ExecutionIdentity) -> Scope {
    [Scope::TurnTask, Scope::Agent, Scope::Session]
        .into_iter()
        .find(|scope| identity.cache_key(*scope).is_ok())
        .unwrap_or(Scope::Unknown)
}

#[test]
fn valid_vectors_parse_round_trip_and_key_exactly() {
    let doc = vectors();
    let scopes = [
        ("host", Scope::Host),
        ("session", Scope::Session),
        ("agent", Scope::Agent),
        ("turn_task", Scope::TurnTask),
        ("project_worktree", Scope::ProjectWorktree),
    ];
    let mut cache_key_checks = 0;
    for vector in group(&doc, "valid") {
        let name = vector["name"].as_str().unwrap();
        let expect = &vector["expect"];
        assert_eq!(expect["valid"], true, "{name}");
        let identity = parse(&vector["wire"]).unwrap_or_else(|e| panic!("{name}: refused: {e}"));

        let round_trip = serde_json::to_value(&identity).unwrap();
        assert_eq!(round_trip, expect["round_trip"], "{name}: round_trip");

        let cache_keys = expect["cache_keys"].as_object().unwrap();
        assert_eq!(cache_keys.len(), scopes.len(), "{name}");
        for (key, scope) in scopes {
            let got = identity.cache_key(scope);
            match &cache_keys[key] {
                Value::Null => assert!(
                    got.is_err(),
                    "{name}: cache_key({key}) must refuse, never widen; got {got:?}"
                ),
                want => {
                    let want: Vec<String> = serde_json::from_value(want.clone()).unwrap();
                    assert_eq!(
                        got.unwrap_or_else(|e| panic!("{name}: cache_key({key}): {e}")),
                        want,
                        "{name}: cache_key({key})"
                    );
                }
            }
            cache_key_checks += 1;
        }
        assert!(identity.cache_key(Scope::Unknown).is_err(), "{name}");

        let scope = narrowest_attributable_scope(&identity);
        assert_ne!(scope, Scope::Host, "{name}: narrowest scope never HOST");
        assert_eq!(
            serde_json::to_value(scope).unwrap(),
            expect["narrowest_attributable_scope"],
            "{name}: narrowest_attributable_scope"
        );
    }
    assert_eq!(cache_key_checks, 16 * 5);
}

#[test]
fn display_ids_are_the_exact_dimension_prefixed_digest_tokens() {
    let doc = vectors();
    assert_eq!(
        doc["display_id"],
        "dimension + ':' + first 8 hex of sha256(utf8(dimension + ':' + value))"
    );
    let mut tokens = 0;
    for vector in group(&doc, "valid") {
        let name = vector["name"].as_str().unwrap();
        let identity = parse(&vector["wire"]).unwrap();
        let fields: [(&str, Option<&str>); 4] = [
            ("host_id", Some(identity.host_id())),
            ("provider_session_id", identity.provider_session_id()),
            ("agent_id", identity.agent_id()),
            ("turn_id", identity.turn_id()),
        ];
        let mut got = serde_json::Map::new();
        for (dimension, value) in fields {
            // An empty id has no value to redact (reference raises).
            let Some(value) = value.filter(|v| !v.is_empty()) else {
                continue;
            };
            let token = identity.display_id(dimension, value);
            let hex: String = Sha256::digest(format!("{dimension}:{value}").as_bytes())
                .iter()
                .take(4)
                .map(|b| format!("{b:02x}"))
                .collect();
            assert_eq!(token, format!("{dimension}:{hex}"), "{name}: rule");
            got.insert(dimension.to_string(), Value::String(token));
            tokens += 1;
        }
        assert_eq!(
            Value::Object(got),
            vector["expect"]["display_ids"],
            "{name}: display_ids"
        );
    }
    assert!(tokens > 0);
}

#[test]
fn invalid_vectors_are_refused_by_the_validated_wire_path() {
    let doc = vectors();
    let invalid = group(&doc, "invalid");
    for vector in invalid {
        let name = vector["name"].as_str().unwrap();
        assert_eq!(vector["expect"]["valid"], false, "{name}");
        assert!(
            parse(&vector["wire"]).is_err(),
            "{name}: must be refused, was accepted"
        );
    }
    assert_eq!(invalid.len(), 12);
}

#[test]
fn correlation_vectors_agree_in_both_directions() {
    let doc = vectors();
    let correlations = group(&doc, "correlations");
    for vector in correlations {
        let name = vector["name"].as_str().unwrap();
        let left = valid_by_name(&doc, vector["left"].as_str().unwrap());
        let right = valid_by_name(&doc, vector["right"].as_str().unwrap());
        let want = vector["expect"]["correlates"].as_bool().unwrap();
        assert_eq!(left.correlates_with(&right), want, "{name}: left->right");
        assert_eq!(right.correlates_with(&left), want, "{name}: right->left");
    }
    assert_eq!(correlations.len(), 8);
}
