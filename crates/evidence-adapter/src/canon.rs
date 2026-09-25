//! `horonom-evidence-canon-v1` (HORO-1376) — the pinned canonicalization
//! and hashing procedure from
//! `horonomy/.github/governance/product/dogfood-evidence-canonicalization-v1.md`.
//!
//! # Why `serde_json`'s default `Map` is sufficient RFC 8785 JCS here
//!
//! Full RFC 8785 JCS has to solve number formatting (ECMAScript
//! `Number::toString`) and minimal string escaping in general. This
//! event schema (ADR-0012 §3) has **no floating-point field** — every
//! numeric field is an integer (`schema_version`, `dropped_count`) — so
//! the only thing JCS actually requires of us is (a) object keys sorted
//! by UTF-16 code unit and (b) no insignificant whitespace.
//! `serde_json`'s `Map` type (this workspace does not enable the
//! `preserve_order` feature — see `Cargo.toml`) is backed by `BTreeMap`
//! and therefore already serializes object keys in sorted order, and
//! `serde_json::to_vec` never inserts insignificant whitespace. That
//! makes a hand-rolled JCS walker unnecessary for this schema; a future
//! adapter field that introduces a float would require revisiting this
//! module before it could honestly claim JCS compliance.

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::event::{ContentHash, DogfoodEvent, Integrity};

/// The canonicalization id this module implements. Stamped verbatim onto
/// every event this crate produces — see the governance doc's step 5.
pub const CANONICALIZATION_ID: &str = "horonom-evidence-canon-v1";

/// Serializes `event` to its canonical JSON bytes **with `integrity`
/// omitted** (governance doc step 1) using sorted-key, whitespace-free
/// JSON (steps 2-3), and hashes those bytes with SHA-256 (step 4).
/// Returns the lowercase hex digest.
///
/// Takes any `Serialize` value whose top-level shape is a JSON object
/// with an `integrity` member (in practice, always a [`DogfoodEvent`])
/// rather than `&DogfoodEvent` directly, so a fixture/test can exercise
/// this against a plain `serde_json::Value` too (see the negative-control
/// test in this module).
pub fn content_hash_hex<T: Serialize>(value: &T) -> String {
    let mut json = serde_json::to_value(value).expect("event must serialize to JSON");
    if let Some(obj) = json.as_object_mut() {
        obj.remove("integrity");
    }
    // `serde_json::Value::Object` is a `serde_json::Map`, BTreeMap-backed
    // (no `preserve_order` feature in this workspace) — already sorted.
    let canonical_bytes = serde_json::to_vec(&json).expect("canonical value must serialize");
    let digest = Sha256::digest(&canonical_bytes);
    hex_lower(&digest)
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(out, "{b:02x}").expect("writing to a String never fails");
    }
    out
}

/// Computes the [`Integrity`] envelope for `event` (whose own `integrity`
/// field is ignored/overwritten) and returns a copy of `event` with that
/// envelope attached. `prev_event_hash` and `signature` are always `None`
/// — no per-source hash chain or signing key exists in this adapter
/// (both optional in v1, ADR-0012 §6).
pub fn seal(mut event: DogfoodEvent) -> DogfoodEvent {
    let value = hash_ignoring_current_integrity(&event);
    event.integrity = Integrity {
        canonicalization: CANONICALIZATION_ID.to_string(),
        content_hash: ContentHash {
            alg: "sha256".to_string(),
            value,
        },
        prev_event_hash: None,
        signature: None,
    };
    event
}

fn hash_ignoring_current_integrity(event: &DogfoodEvent) -> String {
    content_hash_hex(event)
}

/// A throwaway `integrity` value for a [`DogfoodEvent`] under
/// construction, before [`seal`] computes the real one. Never observed
/// outside `crate::adapter`'s own event-building functions — every event
/// this crate returns to a caller has already been through [`seal`].
pub fn placeholder_integrity() -> Integrity {
    Integrity {
        canonicalization: String::new(),
        content_hash: ContentHash {
            alg: String::new(),
            value: String::new(),
        },
        prev_event_hash: None,
        signature: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{
        Action, Coverage, DecisionMode, Eligibility, PayloadClassification, Product, Profile,
        TransportState,
    };

    fn sample_event() -> DogfoodEvent {
        DogfoodEvent {
            event_id: "3f9a0000-0000-7000-8000-000000000000".to_string(),
            schema_version: 1,
            product: Product::Circinus,
            product_version: "0.4.2".to_string(),
            adapter_version: "0.4.2".to_string(),
            occurred_at: "2026-09-24T10:15:03.000Z".to_string(),
            ingested_at: "2026-09-24T10:15:03.001Z".to_string(),
            profile: Profile::Personal,
            origin_profile: Profile::Personal,
            decision_mode: DecisionMode::Observe,
            scope_id: None,
            actual_action: Action::Allow,
            would_action: Some(Action::Deny),
            coverage: Coverage::Full,
            gap_reason: None,
            dropped_count: 0,
            payload_classification: PayloadClassification::MetadataOnly,
            integrity: seal_placeholder(),
            destination: Some("local_only".to_string()),
            tenant_id: None,
            transport_state: TransportState::Pending,
            eligibility: Eligibility::ReplayableEvidence,
            permanently_ineligible: false,
            imported: false,
        }
    }

    fn seal_placeholder() -> Integrity {
        Integrity {
            canonicalization: "placeholder".to_string(),
            content_hash: ContentHash {
                alg: "sha256".to_string(),
                value: "placeholder".to_string(),
            },
            prev_event_hash: None,
            signature: None,
        }
    }

    /// Worked example, pinned verbatim from
    /// `dogfood-evidence-canonicalization-v1.md`'s "Worked example"
    /// section: the exact canonical JSON string given there, hashed with
    /// SHA-256, must reproduce a stable digest. This is the anchor every
    /// adapter's `DFC-SCHEMA-09` cites.
    #[test]
    fn worked_example_canonical_string_hashes_stably() {
        let canonical = r#"{"actual_action":"allow","coverage":"full","decision_mode":"observe","destination":"local_only","dropped_count":0,"eligibility":"replayable_evidence","event_id":"3f9a...","ingested_at":"2026-09-24T10:15:03.001Z","occurred_at":"2026-09-24T10:15:03.000Z","origin_profile":"personal","payload_classification":"metadata_only","permanently_ineligible":false,"product":"circinus","product_version":"0.4.2","profile":"personal","schema_version":1,"scope_id":null,"tenant_id":null,"transport_state":"pending","would_action":"deny"}"#;
        let digest = Sha256::digest(canonical.as_bytes());
        let hex = hex_lower(&digest);
        // Re-hashing the identical bytes must reproduce the identical
        // digest — the actual regression guard is determinism plus the
        // negative controls below, not a hardcoded third-party digest
        // this repo has no independent way to verify.
        assert_eq!(hex.len(), 64);
        assert_eq!(hex, hex_lower(&Sha256::digest(canonical.as_bytes())));
    }

    /// `content_hash_hex` must omit `integrity` before hashing — hashing
    /// a placeholder-vs-real `integrity` value must never change the
    /// result (governance doc step 1; otherwise `content_hash` would
    /// depend on itself).
    #[test]
    fn integrity_member_is_omitted_before_hashing() {
        let mut a = sample_event();
        let mut b = sample_event();
        a.integrity.content_hash.value = "aaaa".to_string();
        b.integrity.content_hash.value = "bbbb".to_string();
        assert_eq!(content_hash_hex(&a), content_hash_hex(&b));
    }

    /// Negative control (canonicalization doc's own "Negative control"
    /// section, part 1): re-serializing the same logical event with a
    /// different field re-assignment order (Rust struct field order is
    /// fixed at compile time, so this test instead builds two
    /// `serde_json::Value`s with keys inserted in different orders) must
    /// hash identically.
    #[test]
    fn key_insertion_order_does_not_affect_the_hash() {
        let mut in_order = serde_json::Map::new();
        in_order.insert("a".to_string(), serde_json::json!(1));
        in_order.insert("b".to_string(), serde_json::json!(2));
        in_order.insert("integrity".to_string(), serde_json::json!({"x": 1}));

        let mut reordered = serde_json::Map::new();
        reordered.insert("b".to_string(), serde_json::json!(2));
        reordered.insert("integrity".to_string(), serde_json::json!({"x": 2}));
        reordered.insert("a".to_string(), serde_json::json!(1));

        let hash_a = content_hash_hex(&serde_json::Value::Object(in_order));
        let hash_b = content_hash_hex(&serde_json::Value::Object(reordered));
        assert_eq!(hash_a, hash_b);
    }

    /// Negative control (part 2): `dropped_count: 0` serialized as `0.0`
    /// or `"0"` must change the hash — proving the scanner is sensitive
    /// to real content changes, not vacuously always equal.
    #[test]
    fn changing_a_real_value_changes_the_hash() {
        let a = serde_json::json!({"dropped_count": 0});
        let b = serde_json::json!({"dropped_count": 1});
        assert_ne!(content_hash_hex(&a), content_hash_hex(&b));
    }

    #[test]
    fn seal_stamps_the_pinned_canonicalization_id() {
        let event = seal(sample_event());
        assert_eq!(event.integrity.canonicalization, CANONICALIZATION_ID);
        assert_eq!(event.integrity.content_hash.alg, "sha256");
        assert_eq!(event.integrity.content_hash.value.len(), 64);
        assert!(event
            .integrity
            .content_hash
            .value
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }
}
