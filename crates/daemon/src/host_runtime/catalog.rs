//! Pure parser and encoder for the private, passive adapter registry.

use std::collections::BTreeMap;

use serde_json::{Map, Number, Value};
use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use super::config_record::InstallationRecord;
use super::contract::{HostContract, ValidatedManifest, MAX_MANIFEST_BYTES};
use super::RegistryFailure;

const MAX_REGISTRY_BYTES: usize = 8 * 1024 * 1024;
// The closed v2 pending target contains nested ownership projections. V1's
// records remain flat; both versions retain the same byte/node bounds.
const MAX_REGISTRY_DEPTH: usize = 12;
const MAX_REGISTRY_NODES: usize = 8192;
const MAX_RECORDS: usize = 128;
const MAX_COUNTER: u64 = 9_007_199_254_740_991;
const IDENTITY_SCOPE: &str = "declared_launch_and_runtime_files";

// Initial registration needs no speculative generation for sizing: JSON string
// escaping uses at most six bytes per input byte; the closed one-record envelope
// (including bounded ID/digests and the real 32-byte generation) fits in 1024.
const _: () = {
    assert!(6 * MAX_MANIFEST_BYTES + 1024 <= MAX_REGISTRY_BYTES);
    assert!(MAX_RECORDS >= 1 && MAX_REGISTRY_DEPTH >= 4 && MAX_REGISTRY_NODES >= 32);
    assert!(MAX_COUNTER >= 1);
};

#[derive(Clone, Debug)]
pub struct RegistryDocument {
    pub(super) schema_version: u32,
    pub(super) registry_id: String,
    pub(super) revision: u64,
    pub(super) adapters: BTreeMap<String, RegistryRecord>,
    pub(super) installations: BTreeMap<String, InstallationRecord>,
}

#[derive(Clone, Debug)]
pub struct RegistryRecord {
    pub(super) manifest: ValidatedManifest,
    pub(super) registration_revision: u64,
    pub(super) trust_revision: u64,
    pub(super) trust: Option<CodeTrust>,
}

#[derive(Clone, Debug)]
pub struct CodeTrust {
    pub(super) manifest_digest: String,
    pub(super) implementation_digest: String,
    pub(super) confirmation_digest: String,
    pub(super) confirmed_at: String,
}

impl RegistryDocument {
    pub fn parse(raw: &[u8], contract: &HostContract) -> Result<Self, RegistryFailure> {
        let value = parse_json(raw)?;
        let schema_version = integer(&value["schema_version"], "schema version")?;
        match schema_version {
            1 => exact_keys(
                &value,
                &["schema_version", "registry_id", "revision", "adapters"],
            )?,
            2 => exact_keys(
                &value,
                &[
                    "schema_version",
                    "registry_id",
                    "revision",
                    "adapters",
                    "installations",
                ],
            )?,
            _ => return Err(fail("registry", "unsupported schema version")),
        }
        let registry_id = string(&value["registry_id"], "registry id")?.to_owned();
        if !is_lower_hex(&registry_id, 32) {
            return Err(fail("registry", "invalid registry id"));
        }
        let revision = integer(&value["revision"], "revision")?;
        if revision == 0 {
            return Err(fail("registry", "invalid revision"));
        }
        let source_records = value["adapters"]
            .as_object()
            .ok_or_else(|| fail("registry", "invalid adapters map"))?;
        if source_records.len() > MAX_RECORDS {
            return Err(fail("registry", "registration limit reached"));
        }
        let mut adapters = BTreeMap::new();
        for (key, source) in source_records {
            exact_keys(
                source,
                &[
                    "manifest_json",
                    "manifest_digest",
                    "registration_revision",
                    "trust_revision",
                    "trust",
                ],
            )?;
            let manifest_json = string(&source["manifest_json"], "manifest")?;
            let manifest_bytes = manifest_json.as_bytes();
            if manifest_bytes.len() > 65_536 {
                return Err(fail("manifest", "input too large"));
            }
            let manifest = contract.validate_manifest(manifest_bytes)?;
            let digest = string(&source["manifest_digest"], "manifest digest")?;
            if digest != manifest.digest() || !is_sha256(digest) {
                return Err(fail("registry", "manifest digest mismatch"));
            }
            if key != manifest.adapter_id() {
                return Err(fail("registry", "adapter id mismatch"));
            }
            if matches!(key.as_str(), "claude_code" | "codex") {
                return Err(fail("registry", "reserved adapter id"));
            }
            let registration_revision =
                integer(&source["registration_revision"], "registration revision")?;
            let trust_revision = integer(&source["trust_revision"], "trust revision")?;
            if registration_revision == 0
                || registration_revision > revision
                || trust_revision > revision
            {
                return Err(fail("registry", "invalid record revision"));
            }
            let trust = if source["trust"].is_null() {
                None
            } else {
                Some(parse_trust(
                    &source["trust"],
                    &registry_id,
                    registration_revision,
                    digest,
                    trust_revision,
                )?)
            };
            adapters.insert(
                key.clone(),
                RegistryRecord {
                    manifest,
                    registration_revision,
                    trust_revision,
                    trust,
                },
            );
        }
        let installations = if schema_version == 2 {
            let records = value["installations"]
                .as_object()
                .ok_or_else(|| fail("registry", "invalid installations map"))?;
            // This version's only installed consumer has exactly one logical
            // builtin/profile/user binding, including uncompleted intents.
            if records.len() > 1 {
                return Err(fail("registry", "installation conflict"));
            }
            let mut installations = BTreeMap::new();
            for (binding, value) in records {
                let record: InstallationRecord = serde_json::from_value(value.clone())
                    .map_err(|_| fail("registry", "invalid installation record"))?;
                record.validate(&registry_id, binding, revision)?;
                installations.insert(binding.clone(), record);
            }
            installations
        } else {
            BTreeMap::new()
        };
        Ok(Self {
            schema_version: schema_version as u32,
            registry_id,
            revision,
            adapters,
            installations,
        })
    }

    pub(super) fn encode(&self, contract: &HostContract) -> Result<Vec<u8>, RegistryFailure> {
        let mut adapters = Map::new();
        for (id, record) in &self.adapters {
            let trust = record
                .trust
                .as_ref()
                .map(|t| {
                    serde_json::json!({
                        "manifest_digest": t.manifest_digest,
                        "implementation_digest": t.implementation_digest,
                        "confirmation_digest": t.confirmation_digest,
                        "confirmed_at": t.confirmed_at,
                        "identity_scope": IDENTITY_SCOPE,
                    })
                })
                .unwrap_or(Value::Null);
            adapters.insert(id.clone(), serde_json::json!({
                "manifest_json": std::str::from_utf8(record.manifest.raw()).map_err(|_| fail("manifest", "invalid UTF-8"))?,
                "manifest_digest": record.manifest.digest(),
                "registration_revision": record.registration_revision,
                "trust_revision": record.trust_revision,
                "trust": trust,
            }));
        }
        let mut value = serde_json::json!({"schema_version":self.schema_version,"registry_id":self.registry_id,"revision":self.revision,"adapters":adapters});
        if self.schema_version == 2 {
            value["installations"] = serde_json::to_value(&self.installations)
                .map_err(|_| fail("registry", "serialization failed"))?;
        } else if !self.installations.is_empty() {
            return Err(fail("registry", "installation requires lifecycle schema"));
        }
        let bytes =
            serde_json::to_vec(&value).map_err(|_| fail("registry", "serialization failed"))?;
        if bytes.len() > MAX_REGISTRY_BYTES {
            return Err(fail("registry", "input too large"));
        }
        // Route all output through the same duplicate-aware and semantic reader.
        Self::parse(&bytes, contract)?;
        Ok(bytes)
    }

    pub fn registry_id(&self) -> &str {
        &self.registry_id
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn adapters(&self) -> &BTreeMap<String, RegistryRecord> {
        &self.adapters
    }
}

impl RegistryRecord {
    pub fn manifest(&self) -> &ValidatedManifest {
        &self.manifest
    }
    pub fn registration_revision(&self) -> u64 {
        self.registration_revision
    }
    pub fn trust_revision(&self) -> u64 {
        self.trust_revision
    }
    pub fn trust(&self) -> Option<&CodeTrust> {
        self.trust.as_ref()
    }
}

impl CodeTrust {
    pub fn manifest_digest(&self) -> &str {
        &self.manifest_digest
    }
    pub fn implementation_digest(&self) -> &str {
        &self.implementation_digest
    }
    pub fn confirmation_digest(&self) -> &str {
        &self.confirmation_digest
    }
    pub fn confirmed_at(&self) -> &str {
        &self.confirmed_at
    }
    pub fn identity_scope(&self) -> &'static str {
        IDENTITY_SCOPE
    }
}

pub(super) fn confirmation_digest(
    registry_id: &str,
    registration_revision: u64,
    manifest_digest: &str,
    implementation_digest: &str,
) -> String {
    let material = format!("libra-host-adapter-trust-v1\n{registry_id}\n{registration_revision}\n{manifest_digest}\n{implementation_digest}");
    format!("sha256:{}", hex(&Sha256::digest(material.as_bytes())))
}

fn parse_trust(
    value: &Value,
    registry_id: &str,
    registration_revision: u64,
    manifest_digest: &str,
    trust_revision: u64,
) -> Result<CodeTrust, RegistryFailure> {
    exact_keys(
        value,
        &[
            "manifest_digest",
            "implementation_digest",
            "confirmation_digest",
            "confirmed_at",
            "identity_scope",
        ],
    )?;
    if trust_revision == 0 {
        return Err(fail("registry", "missing trust revision"));
    }
    let stored_manifest = string(&value["manifest_digest"], "trust manifest digest")?.to_owned();
    let implementation_digest =
        string(&value["implementation_digest"], "implementation digest")?.to_owned();
    let confirmation = string(&value["confirmation_digest"], "confirmation digest")?.to_owned();
    let confirmed_at = string(&value["confirmed_at"], "confirmation timestamp")?.to_owned();
    if stored_manifest != manifest_digest
        || !is_sha256(&implementation_digest)
        || !is_sha256(&confirmation)
        || value["identity_scope"] != IDENTITY_SCOPE
    {
        return Err(fail("registry", "invalid trust correlation"));
    }
    if confirmation
        != confirmation_digest(
            registry_id,
            registration_revision,
            manifest_digest,
            &implementation_digest,
        )
    {
        return Err(fail("registry", "confirmation digest mismatch"));
    }
    if !confirmed_at.ends_with('Z') || OffsetDateTime::parse(&confirmed_at, &Rfc3339).is_err() {
        return Err(fail("registry", "invalid confirmation timestamp"));
    }
    Ok(CodeTrust {
        manifest_digest: stored_manifest,
        implementation_digest,
        confirmation_digest: confirmation,
        confirmed_at,
    })
}

fn parse_json(raw: &[u8]) -> Result<Value, RegistryFailure> {
    libra_governor_protocol::host_event::validate_bounded_json(
        raw,
        MAX_REGISTRY_BYTES,
        MAX_REGISTRY_DEPTH,
        MAX_REGISTRY_NODES,
    )
    .map_err(|error| {
        use libra_governor_protocol::host_event::HostBindingReason as R;
        match error.reason {
            R::InputTooLarge => fail("json", "input too large"),
            R::InputTooDeep => fail("json", "input too deep"),
            R::TooManyNodes => fail("json", "too many JSON nodes"),
            R::DuplicateJsonKey => fail("json", "duplicate JSON key"),
            _ => fail("json", "malformed JSON"),
        }
    })
}

fn exact_keys(value: &Value, keys: &[&str]) -> Result<(), RegistryFailure> {
    let object = value
        .as_object()
        .ok_or_else(|| fail("registry", "expected object"))?;
    if object.len() != keys.len() || keys.iter().any(|k| !object.contains_key(*k)) {
        return Err(fail("registry", "unexpected or missing field"));
    }
    Ok(())
}
fn integer(value: &Value, label: &'static str) -> Result<u64, RegistryFailure> {
    let n = value
        .as_number()
        .and_then(Number::as_u64)
        .filter(|n| *n <= MAX_COUNTER)
        .ok_or_else(|| fail("registry", label))?;
    Ok(n)
}
fn string<'a>(value: &'a Value, label: &'static str) -> Result<&'a str, RegistryFailure> {
    value.as_str().ok_or_else(|| fail("registry", label))
}
fn is_sha256(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|h| is_lower_hex(h, 64))
}
fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn fail(stage: &'static str, reason: &'static str) -> RegistryFailure {
    RegistryFailure::new(stage, reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_v2_roundtrip_preserves_pending_install_without_activating_it() {
        use super::super::config_record::{InstallationContext, ProductBinary};
        let contract = HostContract::load().unwrap();
        let registry_id = "a".repeat(32);
        let context = InstallationContext {
            scope: "user".into(),
            state_root: "/tmp/state".into(),
            target_path: "/tmp/home/.claude/settings.json".into(),
        };
        let binary = ProductBinary {
            path: "/tmp/libra-governor".into(),
            sha256: super::super::config_bundle::sha256(b"product"),
        };
        let (binding, record) =
            InstallationRecord::prepare_install(&registry_id, 1, context.clone(), binary.clone())
                .unwrap();
        let mut document = RegistryDocument {
            schema_version: 2,
            registry_id: registry_id.clone(),
            revision: 1,
            adapters: BTreeMap::new(),
            installations: BTreeMap::from([(binding.clone(), record)]),
        };
        let bytes = document.encode(&contract).unwrap();
        let decoded = RegistryDocument::parse(&bytes, &contract).unwrap();
        assert!(!decoded.installations[&binding].installed);
        assert!(!decoded.installations[&binding].desired_enabled);
        assert!(decoded.installations[&binding].pending.is_some());
        assert!(decoded.encode(&contract).unwrap() == bytes);
        let (second, record) =
            InstallationRecord::prepare_install(&registry_id, 1, context, binary).unwrap();
        document.installations.insert(second, record);
        assert!(document.encode(&contract).is_err());
    }

    #[test]
    fn counters_reject_fraction_and_overflow() {
        assert!(integer(&serde_json::json!(1.0), "n").is_err());
        assert!(integer(&serde_json::json!(9_007_199_254_740_992u64), "n").is_err());
        assert_eq!(
            integer(&serde_json::json!(9_007_199_254_740_991u64), "n").unwrap(),
            MAX_COUNTER
        );
    }

    #[test]
    fn confirmation_is_generation_correlated() {
        let d = confirmation_digest(
            "0123456789abcdef0123456789abcdef",
            7,
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        );
        assert!(is_sha256(&d));
        assert_ne!(
            d,
            confirmation_digest(
                "0123456789abcdef0123456789abcdef",
                8,
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            )
        );
    }

    #[test]
    fn bounded_parser_rejects_duplicate_unknown_and_calendar_errors() {
        assert!(parse_json(br#"{"a":1,"a":2}"#).is_err());
        let mut v = serde_json::json!({"x":1});
        assert!(exact_keys(&v, &["y"]).is_err());
        v = serde_json::json!({"identity_scope":"something_else"});
        assert_ne!(v["identity_scope"], IDENTITY_SCOPE);
        assert!(OffsetDateTime::parse("2026-99-99T99:99:99Z", &Rfc3339).is_err());
        assert!(OffsetDateTime::parse("2024-02-29T12:30:00Z", &Rfc3339).is_ok());
    }

    #[test]
    fn empty_generation_roundtrips_and_future_or_unknown_fields_refuse() {
        let contract = HostContract::load().unwrap();
        let raw = br#"{"schema_version":1,"registry_id":"0123456789abcdef0123456789abcdef","revision":9,"adapters":{}}"#;
        let document = RegistryDocument::parse(raw, &contract).unwrap();
        assert_eq!(document.revision(), 9);
        assert!(document.adapters().is_empty());
        let encoded = document.encode(&contract).unwrap();
        let decoded = RegistryDocument::parse(&encoded, &contract).unwrap();
        assert_eq!(decoded.registry_id(), document.registry_id());
        assert_eq!(decoded.revision(), document.revision());

        let future =
            String::from_utf8_lossy(raw).replace("\"schema_version\":1", "\"schema_version\":2");
        assert!(RegistryDocument::parse(future.as_bytes(), &contract).is_err());
        let unknown = String::from_utf8_lossy(raw)
            .replace("\"revision\":9", "\"revision\":9,\"future\":true");
        assert!(RegistryDocument::parse(unknown.as_bytes(), &contract).is_err());
    }

    #[test]
    fn registry_depth_and_size_limits_are_enforced() {
        assert!(parse_json(&vec![b' '; MAX_REGISTRY_BYTES + 1]).is_err());
        let nested = format!(
            "{}0{}",
            "[".repeat(MAX_REGISTRY_DEPTH + 1),
            "]".repeat(MAX_REGISTRY_DEPTH + 1)
        );
        assert!(parse_json(nested.as_bytes()).is_err());
    }

    #[test]
    fn manifest_source_bytes_are_kept_verbatim() {
        let contract = HostContract::load().unwrap();
        let fixture = include_bytes!(
            "../../../protocol/contracts/host-adapter/v1/fixtures/valid-manifest-synthetic.json"
        );
        let original = format!(" \n{}\n ", std::str::from_utf8(fixture).unwrap());
        let digest = format!("sha256:{}", hex(&Sha256::digest(original.as_bytes())));
        let raw = serde_json::to_vec(&serde_json::json!({
            "schema_version":1, "registry_id":"0123456789abcdef0123456789abcdef", "revision":1,
            "adapters":{"synthetic_external":{"manifest_json":original,"manifest_digest":digest,"registration_revision":1,"trust_revision":0,"trust":null}}
        })).unwrap();
        let document = RegistryDocument::parse(&raw, &contract).unwrap();
        assert_eq!(
            document.adapters()["synthetic_external"].manifest().raw(),
            original.as_bytes()
        );
        let encoded = document.encode(&contract).unwrap();
        let decoded = RegistryDocument::parse(&encoded, &contract).unwrap();
        assert_eq!(
            decoded.adapters()["synthetic_external"].manifest().raw(),
            original.as_bytes()
        );
    }

    #[test]
    fn confirmation_requires_digest_and_scope_correlations() {
        let registry_id = "0123456789abcdef0123456789abcdef";
        let manifest = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let implementation =
            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let confirmation = confirmation_digest(registry_id, 1, manifest, implementation);
        let trust = serde_json::json!({"manifest_digest":manifest,"implementation_digest":implementation,"confirmation_digest":confirmation,"confirmed_at":"2024-02-29T12:30:00Z","identity_scope":IDENTITY_SCOPE});
        assert!(parse_trust(&trust, registry_id, 1, manifest, 1).is_ok());
        let mut drift = trust.clone();
        drift["confirmation_digest"] = Value::String(format!("sha256:{}", "0".repeat(64)));
        assert!(parse_trust(&drift, registry_id, 1, manifest, 1).is_err());
        let mut bad_date = trust;
        bad_date["confirmed_at"] = Value::String("2026-99-99T99:99:99Z".into());
        assert!(parse_trust(&bad_date, registry_id, 1, manifest, 1).is_err());
    }
}
