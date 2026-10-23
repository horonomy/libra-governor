//! Runs the `normalizers[]` group of the Host Integration execution-identity
//! conformance vectors (pinned in
//! `crates/domain/tests/fixtures/README.md`) through Libra's own
//! hook-payload-to-identity path: [`normalize`] then [`capture`], the same
//! two functions `run.rs` and `canonical.rs` call. The envelope-level groups
//! run in `crates/domain/tests/execution_identity_conformance.rs`.
//!
//! The vectors' payloads are bare identity fixtures, so the harness adapts
//! them to Libra's event routing without touching any identity field:
//! `hook_event_name` (when present) selects the [`EntryPoint`] it names,
//! otherwise all three entry points are exercised, and the non-identity
//! fields an entry point requires (`cwd`, `prompt`, `tool_name`) are added
//! only when absent.
//!
//! Four vectors are NOT satisfied (`REFUSED_NOT_ABSENT`): Libra refuses the
//! whole hook event when `session_id` is missing/non-string or the payload
//! is not a mapping, where the reference emits an envelope with an absent
//! session. Libra fails closed (no session-attributed identity is ever
//! produced), but making `session_id` optional is a protocol change in a
//! separate slice. They are pinned as refusals below and run exactly in an
//! ignored test.

use libra_governor_domain::{AgentKind, ExecutionIdentity, Scope};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::event::NormalizedEvent;
use super::identity::capture;
use super::normalize::{normalize, EntryPoint};

const VECTORS: &str =
    include_str!("../../../domain/tests/fixtures/execution-identity-conformance-vectors.json");
const PINNED_SHA256: &str = "c6ab9b978540e723a4d338f40b5218f6e355667c35dec456e9b7a650233b5a13";

const REFUSED_NOT_ABSENT: [&str; 4] = [
    "claude-no-session",
    "claude-non-string-session",
    "claude-not-a-mapping",
    "codex-heuristics-only",
];

fn vectors() -> Value {
    serde_json::from_str(VECTORS).expect("vendored vectors are valid JSON")
}

fn agent_kind(provider: &str) -> AgentKind {
    match provider {
        "claude_code" => AgentKind::ClaudeCode,
        "codex" => AgentKind::Codex,
        other => panic!("vector provider {other} has no Libra AgentKind"),
    }
}

fn entry_points(payload: &Value) -> Vec<EntryPoint> {
    match payload.get("hook_event_name").and_then(Value::as_str) {
        Some("UserPromptSubmit") => vec![EntryPoint::PromptSubmit],
        Some("PostToolUse") => vec![EntryPoint::ToolCompleted],
        Some("Stop") => vec![EntryPoint::TurnCompleted],
        Some(other) => panic!("vector hook_event_name {other} has no entry point"),
        None => vec![
            EntryPoint::PromptSubmit,
            EntryPoint::ToolCompleted,
            EntryPoint::TurnCompleted,
        ],
    }
}

fn adapted(payload: &Value, entry: EntryPoint) -> Value {
    let mut payload = payload.clone();
    if let Some(map) = payload.as_object_mut() {
        let required: &[(&str, &str)] = match entry {
            EntryPoint::PromptSubmit => &[("cwd", "/w"), ("prompt", "p")],
            EntryPoint::ToolCompleted => &[("tool_name", "Bash")],
            EntryPoint::TurnCompleted => &[],
        };
        for (key, value) in required {
            map.entry(*key).or_insert_with(|| json!(value));
        }
    }
    payload
}

/// Libra's real payload-to-identity path, or `None` if it refuses the event.
fn identity_from_payload(
    agent: AgentKind,
    host_id: &str,
    entry: EntryPoint,
    payload: &Value,
) -> Option<ExecutionIdentity> {
    let raw = serde_json::to_string(&adapted(payload, entry)).unwrap();
    let (session, agent_id, turn_id) = match normalize(entry, &raw).ok()? {
        NormalizedEvent::PromptSubmitted {
            session_id,
            agent_id,
            turn_id,
            ..
        }
        | NormalizedEvent::ToolCompleted {
            session_id,
            agent_id,
            turn_id,
            ..
        }
        | NormalizedEvent::TurnCompleted {
            session_id,
            agent_id,
            turn_id,
            ..
        } => (session_id, agent_id, turn_id),
        other => panic!("vector routed to a non-identity event: {other:?}"),
    };
    Some(
        capture(
            host_id,
            agent,
            &session,
            agent_id.as_deref(),
            turn_id.as_deref(),
            time::OffsetDateTime::now_utc(),
        )
        .expect("capture builds a valid envelope"),
    )
}

fn narrowest_attributable_scope(identity: &ExecutionIdentity) -> Scope {
    [Scope::TurnTask, Scope::Agent, Scope::Session]
        .into_iter()
        .find(|scope| identity.cache_key(*scope).is_ok())
        .unwrap_or(Scope::Unknown)
}

fn assert_matches_expectation(name: &str, identity: &ExecutionIdentity, expect: &Value) {
    let opt = |v: Option<&str>| v.map_or(Value::Null, |s| json!(s));
    assert_eq!(
        opt(identity.provider_session_id()),
        expect["provider_session_id"],
        "{name}"
    );
    assert_eq!(opt(identity.agent_id()), expect["agent_id"], "{name}");
    assert_eq!(opt(identity.turn_id()), expect["turn_id"], "{name}");
    assert_eq!(
        serde_json::to_value(identity.lineage_status()).unwrap(),
        expect["lineage_status"],
        "{name}"
    );
    let scope = narrowest_attributable_scope(identity);
    assert_ne!(scope, Scope::Host, "{name}: narrowest scope never HOST");
    assert_eq!(
        serde_json::to_value(scope).unwrap(),
        expect["narrowest_attributable_scope"],
        "{name}"
    );
}

fn normalizers(doc: &Value) -> &Vec<Value> {
    doc["normalizers"].as_array().unwrap()
}

#[test]
fn vendored_vectors_match_the_pinned_digest() {
    let digest: String = Sha256::digest(VECTORS.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(digest, PINNED_SHA256, "follow tests/fixtures/README.md");
    assert_eq!(normalizers(&vectors()).len(), 10);
}

#[test]
fn normalizer_vectors_hold_through_libras_payload_path() {
    let doc = vectors();
    let mut checked = 0;
    for vector in normalizers(&doc) {
        let name = vector["name"].as_str().unwrap();
        if REFUSED_NOT_ABSENT.contains(&name) {
            continue;
        }
        let agent = agent_kind(vector["provider"].as_str().unwrap());
        let host_id = vector["host_id"].as_str().unwrap();
        for entry in entry_points(&vector["payload"]) {
            let identity = identity_from_payload(agent, host_id, entry, &vector["payload"])
                .unwrap_or_else(|| panic!("{name}: {entry:?} refused the payload"));
            assert_eq!(identity.host_id(), host_id, "{name}");
            assert_eq!(identity.tool_provider(), agent.as_tool_provider(), "{name}");
            assert_matches_expectation(&format!("{name}/{entry:?}"), &identity, &vector["expect"]);
            checked += 1;
        }
    }
    // 5 vectors x 3 entry points + 'claude-main', routed to its one named entry point.
    assert_eq!(checked, 5 * 3 + 1);
}

#[test]
fn refused_normalizer_vectors_fail_closed_and_never_attribute_a_session() {
    let doc = vectors();
    let mut refused = 0;
    for vector in normalizers(&doc) {
        let name = vector["name"].as_str().unwrap();
        if !REFUSED_NOT_ABSENT.contains(&name) {
            continue;
        }
        let agent = agent_kind(vector["provider"].as_str().unwrap());
        for entry in entry_points(&vector["payload"]) {
            let identity = identity_from_payload(agent, "host-1", entry, &vector["payload"]);
            assert!(
                identity.is_none(),
                "{name}/{entry:?}: Libra now accepts this payload; move it out of REFUSED_NOT_ABSENT \
                 and drop its ignored exact-match test"
            );
            refused += 1;
        }
        assert_eq!(
            vector["expect"]["provider_session_id"],
            Value::Null,
            "{name}"
        );
    }
    assert_eq!(refused, 4 * 3);
}

#[test]
#[ignore = "known deviation: Libra refuses the whole hook event when session_id is missing/non-string or the payload is not a mapping; session-optional hook pipeline is a separate protocol slice"]
fn deviation_refused_vectors_exact_match() {
    let doc = vectors();
    for vector in normalizers(&doc) {
        let name = vector["name"].as_str().unwrap();
        if !REFUSED_NOT_ABSENT.contains(&name) {
            continue;
        }
        let agent = agent_kind(vector["provider"].as_str().unwrap());
        for entry in entry_points(&vector["payload"]) {
            let identity = identity_from_payload(
                agent,
                vector["host_id"].as_str().unwrap(),
                entry,
                &vector["payload"],
            )
            .unwrap_or_else(|| panic!("{name}/{entry:?}: refused, vector expects an envelope"));
            assert_matches_expectation(name, &identity, &vector["expect"]);
        }
    }
}
