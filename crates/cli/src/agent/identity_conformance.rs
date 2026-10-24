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
//! HORO-2197 made `session_id` optional in the hook decoder (absent, blank
//! and non-string all read as "no session"), which brings three of the four
//! vectors #211 left deviating onto the reference: `claude-no-session`,
//! `claude-non-string-session` and `codex-heuristics-only`. They run in
//! `normalizer_vectors_hold_through_libras_payload_path` like every other
//! normalizer vector.
//!
//! One vector remains a named, justified deviation ([`JUSTIFIED_DEVIATIONS`]):
//! `claude-not-a-mapping`. See the constant for the invariant. Every other
//! requirement of the contract (no fabricated ids, blank is absent, Claude
//! `turn_id` unmapped) is asserted by the vectors that do match.

use libra_governor_domain::{AgentKind, ExecutionIdentity, Scope};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::event::NormalizedEvent;
use super::identity::capture;
use super::normalize::{normalize, EntryPoint};

const VECTORS: &str =
    include_str!("../../../domain/tests/fixtures/execution-identity-conformance-vectors.json");
const PINNED_SHA256: &str = "c6ab9b978540e723a4d338f40b5218f6e355667c35dec456e9b7a650233b5a13";

/// Vectors Libra deliberately does not match, each with the invariant that
/// forbids it. Pinned by [`justified_deviations_refuse_and_never_attribute_a_session`].
///
/// `claude-not-a-mapping` (payload `["s-1"]`): the reference emits an
/// envelope with every identity field absent. Libra refuses the whole hook
/// event instead. Invariant: a hook payload that is not a JSON object names
/// no field at all, and serde's derived struct decoders would otherwise read
/// the array *positionally*, so `["s-1"]` would become session `s-1` -- an
/// identity read out of a payload that names nothing. `normalize` therefore
/// rejects every non-object before any decoder runs, and the event produces
/// no envelope, no daemon request and no counter. This is strictly stricter
/// than the reference (the reference's envelope would also carry no session);
/// what the vector protects -- no session is ever attributed from a
/// non-mapping -- holds, and is asserted here for every entry point.
const JUSTIFIED_DEVIATIONS: [&str; 1] = ["claude-not-a-mapping"];

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
            EntryPoint::ToolRequested | EntryPoint::TurnCompleted => &[],
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
            session.as_deref(),
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
        if JUSTIFIED_DEVIATIONS.contains(&name) {
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
    // 8 vectors x 3 entry points + 'claude-main', routed to its one named entry point.
    assert_eq!(checked, 8 * 3 + 1);
}

#[test]
fn justified_deviations_refuse_and_never_attribute_a_session() {
    let doc = vectors();
    let mut refused = 0;
    for vector in normalizers(&doc) {
        let name = vector["name"].as_str().unwrap();
        if !JUSTIFIED_DEVIATIONS.contains(&name) {
            continue;
        }
        assert!(
            !vector["payload"].is_object(),
            "{name}: a deviation justified by the non-object guard must be a non-object payload"
        );
        let agent = agent_kind(vector["provider"].as_str().unwrap());
        for entry in entry_points(&vector["payload"]) {
            let identity = identity_from_payload(agent, "host-1", entry, &vector["payload"]);
            assert!(
                identity.is_none(),
                "{name}/{entry:?}: Libra now accepts this payload; move it out of \
                 JUSTIFIED_DEVIATIONS and let the exact-match test cover it"
            );
            refused += 1;
        }
        assert_eq!(
            vector["expect"]["provider_session_id"],
            Value::Null,
            "{name}: the reference attributes no session either"
        );
    }
    assert_eq!(refused, JUSTIFIED_DEVIATIONS.len() * 3);
}

/// What the three newly matching vectors must never be turned into: the
/// absent session stays absent (not `""`, not a stringified number), and
/// the agent-only identity cannot be keyed at any scope that needs a
/// session.
#[test]
fn an_absent_session_is_never_filled_in_or_widened() {
    for (provider, payload) in [
        (
            AgentKind::ClaudeCode,
            json!({"agent_id": "a-1", "cwd": "/w"}),
        ),
        (
            AgentKind::ClaudeCode,
            json!({"session_id": 123, "agent_id": "a-1"}),
        ),
        (
            AgentKind::ClaudeCode,
            json!({"session_id": "  ", "agent_id": "a-1"}),
        ),
        (
            AgentKind::Codex,
            json!({"agent_id": "a-1", "turn_id": "t-1"}),
        ),
    ] {
        for entry in entry_points(&payload) {
            let identity = identity_from_payload(provider, "host-1", entry, &payload)
                .unwrap_or_else(|| panic!("{payload}/{entry:?}: refused"));
            assert_eq!(identity.provider_session_id(), None, "{payload}");
            assert_eq!(identity.agent_id(), Some("a-1"), "{payload}");
            for scope in [Scope::TurnTask, Scope::Agent, Scope::Session] {
                assert!(
                    identity.cache_key(scope).is_err(),
                    "{payload}/{entry:?}: {scope:?} must error, not widen"
                );
            }
            assert_eq!(narrowest_attributable_scope(&identity), Scope::Unknown);
        }
    }
}

#[test]
fn a_mapping_with_the_deviating_arrays_content_is_not_a_session_either() {
    // The array form is refused; the nearest object form names no session
    // and must not borrow the array's element as one.
    let payload = json!({"0": "s-1"});
    for entry in entry_points(&payload) {
        let identity = identity_from_payload(AgentKind::ClaudeCode, "host-1", entry, &payload)
            .expect("an object with unknown keys decodes");
        assert_eq!(identity.provider_session_id(), None);
    }
}
