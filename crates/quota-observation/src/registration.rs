//! Host-local registration config (NOT part of the wire protocol —
//! HORO-1853 design-v2 is explicit that this is operator-maintained
//! state, never sent to or received from an extension).
//!
//! The load-bearing invariant this module exists to enforce is
//! [`PoolId`]: `PoolId = "{account_fingerprint}/{pool_alias}"`, and
//! **nothing else** — never `provider`, `registration_id`,
//! `window_uuid`, or `binding_revision`. This is what keeps one external
//! allowance recognized as the same pool across re-registration,
//! multiple Libra sessions, and multiple worktrees. [`PoolId::new`]
//! takes only those two fields so a caller cannot accidentally fold a
//! third field into pool identity.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::wire::{Metric, Scope, Unit};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PoolIdError {
    #[error("account_fingerprint must not be empty")]
    EmptyAccountFingerprint,
    #[error("pool_alias must not be empty")]
    EmptyPoolAlias,
    #[error("account_fingerprint must not contain '/'")]
    AccountFingerprintContainsSlash,
    #[error("pool_alias must not contain '/'")]
    PoolAliasContainsSlash,
}

/// The sole account-identity axis for pool uniqueness, along with
/// [`PoolId`]'s own `pool_alias` component. Deliberately excludes
/// `provider` (display-only), `registration_id` (a local alias, no
/// identity role), `window_uuid` (a local-only window handle, no
/// identity role), and `binding_revision` — none of those fields may
/// ever affect which pool a reading belongs to, which is the exact
/// relabeling attack this type's uniqueness check (see
/// [`check_pool_uniqueness`]) is designed to close.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PoolId {
    account_fingerprint: String,
    pool_alias: String,
}

impl PoolId {
    pub fn new(
        account_fingerprint: impl Into<String>,
        pool_alias: impl Into<String>,
    ) -> Result<Self, PoolIdError> {
        let account_fingerprint = account_fingerprint.into();
        let pool_alias = pool_alias.into();
        if account_fingerprint.is_empty() {
            return Err(PoolIdError::EmptyAccountFingerprint);
        }
        if pool_alias.is_empty() {
            return Err(PoolIdError::EmptyPoolAlias);
        }
        if account_fingerprint.contains('/') {
            return Err(PoolIdError::AccountFingerprintContainsSlash);
        }
        if pool_alias.contains('/') {
            return Err(PoolIdError::PoolAliasContainsSlash);
        }
        Ok(PoolId {
            account_fingerprint,
            pool_alias,
        })
    }

    pub fn account_fingerprint(&self) -> &str {
        &self.account_fingerprint
    }

    pub fn pool_alias(&self) -> &str {
        &self.pool_alias
    }
}

impl fmt::Display for PoolId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.account_fingerprint, self.pool_alias)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    OneshotStdio,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixedEnvironment {
    #[serde(rename = "LANG")]
    pub lang: String,
    #[serde(rename = "LC_ALL")]
    pub lc_all: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transport {
    pub kind: TransportKind,
    pub executable: String,
    pub args: Vec<String>,
    pub environment: FixedEnvironment,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisteredContext {
    pub selector: String,
    pub handle: String,
    pub principal: String,
    pub authority: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapacityCeiling {
    pub value: crate::wire::Quantity,
    pub unit: Unit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedConstraint {
    pub id: String,
    pub scope: Scope,
    pub metric: Metric,
    pub unit: Unit,
    /// Local-only window handle. No identity role — must never
    /// participate in [`PoolId`] computation.
    pub window_uuid: String,
    /// Operator display alias. No identity role — must never
    /// participate in [`PoolId`] computation.
    pub provider: String,
    pub account_fingerprint: String,
    pub pool_alias: String,
    pub max_staleness_secs: u64,
    pub window_length_secs: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity_ceiling: Option<CapacityCeiling>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustMode {
    ObservationOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationConfidence {
    Low,
    Medium,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeFileDigest {
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Consent {
    pub consented_by: String,
    pub recorded_at_reference: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Approval {
    pub approved: bool,
    pub revision: String,
    pub executable_sha256: String,
    pub manifest_sha256: String,
    pub launch_tuple_sha256: String,
    pub runtime_files: Vec<RuntimeFileDigest>,
    /// Non-empty — feeds future live-source governance.
    pub endpoint_owner: String,
    pub consent: Consent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registration {
    pub registration_id: String,
    pub extension_id: String,
    pub extension_version: String,
    #[serde(default)]
    pub enabled: bool,
    pub transport: Transport,
    pub context: RegisteredContext,
    pub expected: Vec<ExpectedConstraint>,
    pub binding_revision: String,
    pub binding_verified_at: String,
    pub scope_configured_at: String,
    pub timeout_ms: u64,
    pub trust_mode: TrustMode,
    pub observation_confidence: ObservationConfidence,
    pub approval: Approval,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistrationError {
    #[error("transport.environment must be exactly LANG=C.UTF-8, LC_ALL=C.UTF-8")]
    InvalidEnvironment,
    #[error("transport.executable must be an absolute path")]
    ExecutableNotAbsolute,
    #[error("timeout_ms must be within 100..=10000")]
    TimeoutOutOfRange,
    #[error("expected[{0}].max_staleness_secs must be within 1..=86400")]
    MaxStalenessOutOfRange(usize),
    #[error("expected[{0}].window_length_secs must be within 1..=31622400")]
    WindowLengthOutOfRange(usize),
    #[error("expected[{0}].max_staleness_secs must be <= window_length_secs")]
    MaxStalenessExceedsWindowLength(usize),
    #[error("expected[{0}].capacity_ceiling.unit must equal expected[{0}].unit")]
    CapacityCeilingUnitMismatch(usize),
    #[error("expected must not be empty")]
    EmptyExpected(),
    #[error("expected[{0}].id {1:?} duplicates an earlier entry in the same registration")]
    DuplicateExpectedId(usize, String),
    #[error("endpoint_owner must not be empty")]
    EmptyEndpointOwner,
    #[error("pool id error: {0}")]
    PoolId(#[from] PoolIdError),
    #[error(
        "duplicate enabled pool binding for {pool_id}: {first_registration}/{first_expected_id} \
         collides with {second_registration}/{second_expected_id}"
    )]
    DuplicatePoolBinding {
        pool_id: String,
        first_registration: String,
        first_expected_id: String,
        second_registration: String,
        second_expected_id: String,
    },
    #[error("failed to read registration directory {0:?}: {1}")]
    DirectoryRead(PathBuf, String),
    #[error("failed to read registration file {0:?}: {1}")]
    FileRead(PathBuf, String),
    #[error("failed to parse registration file {0:?}: {1}")]
    Parse(PathBuf, String),
}

const MAX_STALENESS_RANGE: std::ops::RangeInclusive<u64> = 1..=86_400;
const WINDOW_LENGTH_RANGE: std::ops::RangeInclusive<u64> = 1..=31_622_400;
const TIMEOUT_MS_RANGE: std::ops::RangeInclusive<u64> = 100..=10_000;

impl Registration {
    /// Structural/semantic checks the JSON Schema itself can't express:
    /// cross-field staleness/window/ceiling-unit invariants, exact
    /// environment, absolute executable path, numeric ranges.
    pub fn validate(&self) -> Result<(), RegistrationError> {
        if self.transport.environment.lang != "C.UTF-8"
            || self.transport.environment.lc_all != "C.UTF-8"
        {
            return Err(RegistrationError::InvalidEnvironment);
        }
        if !self.transport.executable.starts_with('/') {
            return Err(RegistrationError::ExecutableNotAbsolute);
        }
        if !TIMEOUT_MS_RANGE.contains(&self.timeout_ms) {
            return Err(RegistrationError::TimeoutOutOfRange);
        }
        if self.approval.endpoint_owner.is_empty() {
            return Err(RegistrationError::EmptyEndpointOwner);
        }
        if self.expected.is_empty() {
            return Err(RegistrationError::EmptyExpected());
        }
        // `expected[].id` is the key both `apply_account_binding` and
        // `compute_max_age_seconds` (crate::freshness) index by via a
        // `HashMap`. A duplicate id would let the later entry silently
        // win, so a wire constraint could be checked for freshness or
        // account-fingerprint binding against the wrong entry's
        // expectations. Reject it here instead of leaving it to those
        // call sites to notice.
        let mut seen_ids = std::collections::HashSet::new();
        for (index, entry) in self.expected.iter().enumerate() {
            if !seen_ids.insert(entry.id.as_str()) {
                return Err(RegistrationError::DuplicateExpectedId(
                    index,
                    entry.id.clone(),
                ));
            }
        }
        for (index, entry) in self.expected.iter().enumerate() {
            if !MAX_STALENESS_RANGE.contains(&entry.max_staleness_secs) {
                return Err(RegistrationError::MaxStalenessOutOfRange(index));
            }
            if !WINDOW_LENGTH_RANGE.contains(&entry.window_length_secs) {
                return Err(RegistrationError::WindowLengthOutOfRange(index));
            }
            if entry.max_staleness_secs > entry.window_length_secs {
                return Err(RegistrationError::MaxStalenessExceedsWindowLength(index));
            }
            if let Some(ceiling) = &entry.capacity_ceiling {
                if ceiling.unit != entry.unit {
                    return Err(RegistrationError::CapacityCeilingUnitMismatch(index));
                }
            }
            // PoolId construction validates the account_fingerprint/pool_alias
            // shape unconditionally, even for a disabled registration — a
            // malformed pool identity is still a loader error (fail closed).
            PoolId::new(entry.account_fingerprint.clone(), entry.pool_alias.clone())?;
        }
        Ok(())
    }
}

/// One `(registration_id, expected[].id)` site that produced a given
/// [`PoolId`], used only for error reporting.
struct Binding {
    registration_id: String,
    expected_id: String,
}

/// At most one **enabled** binding may exist for a given
/// `(account_fingerprint, pool_alias)` pair. Scope is user-global by
/// construction: this scans every loaded registration, from every file
/// in the directory passed to [`load_registrations`] — never just the
/// current process's view. Uniqueness deliberately does not constrain
/// metric/unit: two distinct `pool_alias` values against the same
/// account for two genuinely separate limits are allowed.
fn check_pool_uniqueness(
    registrations: &[(PathBuf, Registration)],
) -> Result<(), RegistrationError> {
    let mut seen: HashMap<(String, String), Binding> = HashMap::new();
    for (_, registration) in registrations {
        if !registration.enabled {
            continue;
        }
        for entry in &registration.expected {
            let pool_id = PoolId::new(entry.account_fingerprint.clone(), entry.pool_alias.clone())?;
            let key = (
                pool_id.account_fingerprint().to_owned(),
                pool_id.pool_alias().to_owned(),
            );
            if let Some(existing) = seen.get(&key) {
                return Err(RegistrationError::DuplicatePoolBinding {
                    pool_id: pool_id.to_string(),
                    first_registration: existing.registration_id.clone(),
                    first_expected_id: existing.expected_id.clone(),
                    second_registration: registration.registration_id.clone(),
                    second_expected_id: entry.id.clone(),
                });
            }
            seen.insert(
                key,
                Binding {
                    registration_id: registration.registration_id.clone(),
                    expected_id: entry.id.clone(),
                },
            );
        }
    }
    Ok(())
}

/// Loads and validates every `*.json` registration file in `directory`,
/// fail-closed: any unparseable or semantically invalid file is a
/// loader error, never silently skipped (a skipped file could hide a
/// duplicate enabled binding). Directory entries are sorted by filename
/// before processing, so loader output and error ordering are
/// deterministic.
///
/// `directory` is a parameter, not a hardcoded path — resolving the
/// actual operator config directory is an integration concern for
/// whatever CLI command wires this crate up later.
pub fn load_registrations(directory: &Path) -> Result<Vec<Registration>, RegistrationError> {
    let entries = fs::read_dir(directory).map_err(|error| {
        RegistrationError::DirectoryRead(directory.to_owned(), error.to_string())
    })?;
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| {
            RegistrationError::DirectoryRead(directory.to_owned(), error.to_string())
        })?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
            paths.push(path);
        }
    }
    paths.sort();

    let mut loaded: Vec<(PathBuf, Registration)> = Vec::with_capacity(paths.len());
    for path in paths {
        let raw = fs::read_to_string(&path)
            .map_err(|error| RegistrationError::FileRead(path.clone(), error.to_string()))?;
        let registration: Registration = serde_json::from_str(&raw)
            .map_err(|error| RegistrationError::Parse(path.clone(), error.to_string()))?;
        registration.validate()?;
        loaded.push((path, registration));
    }

    check_pool_uniqueness(&loaded)?;

    Ok(loaded
        .into_iter()
        .map(|(_, registration)| registration)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::Quantity;

    fn sample_expected(id: &str, fingerprint: &str, alias: &str) -> ExpectedConstraint {
        ExpectedConstraint {
            id: id.into(),
            scope: Scope::Account {
                handle: "acct".into(),
            },
            metric: Metric::TotalTokens,
            unit: Unit::Tokens,
            window_uuid: "win-1".into(),
            provider: "anthropic".into(),
            account_fingerprint: fingerprint.into(),
            pool_alias: alias.into(),
            max_staleness_secs: 60,
            window_length_secs: 3600,
            capacity_ceiling: None,
        }
    }

    fn sample_registration(
        id: &str,
        enabled: bool,
        expected: Vec<ExpectedConstraint>,
    ) -> Registration {
        Registration {
            registration_id: id.into(),
            extension_id: "ext.example".into(),
            extension_version: "1.0.0".into(),
            enabled,
            transport: Transport {
                kind: TransportKind::OneshotStdio,
                executable: "/usr/local/bin/ext".into(),
                args: vec![],
                environment: FixedEnvironment {
                    lang: "C.UTF-8".into(),
                    lc_all: "C.UTF-8".into(),
                },
            },
            context: RegisteredContext {
                selector: "sel".into(),
                handle: "handle".into(),
                principal: "principal".into(),
                authority: "authority".into(),
            },
            expected,
            binding_revision: "1".into(),
            binding_verified_at: "2026-10-12T00:00:00Z".into(),
            scope_configured_at: "2026-10-12T00:00:00Z".into(),
            timeout_ms: 5_000,
            trust_mode: TrustMode::ObservationOnly,
            observation_confidence: ObservationConfidence::Low,
            approval: Approval {
                approved: true,
                revision: "1".into(),
                executable_sha256: "deadbeef".into(),
                manifest_sha256: "deadbeef".into(),
                launch_tuple_sha256: "deadbeef".into(),
                runtime_files: vec![],
                endpoint_owner: "owner".into(),
                consent: Consent {
                    consented_by: "operator".into(),
                    recorded_at_reference: "ref-1".into(),
                },
            },
        }
    }

    #[test]
    fn pool_id_rejects_empty_and_slash_components() {
        assert_eq!(
            PoolId::new("", "alias").unwrap_err(),
            PoolIdError::EmptyAccountFingerprint
        );
        assert_eq!(
            PoolId::new("fp", "").unwrap_err(),
            PoolIdError::EmptyPoolAlias
        );
        assert_eq!(
            PoolId::new("a/b", "c").unwrap_err(),
            PoolIdError::AccountFingerprintContainsSlash
        );
        assert_eq!(
            PoolId::new("a", "b/c").unwrap_err(),
            PoolIdError::PoolAliasContainsSlash
        );
    }

    #[test]
    fn pool_id_display_is_fingerprint_slash_alias() {
        let pool_id = PoolId::new("fp1", "alias1").unwrap();
        assert_eq!(pool_id.to_string(), "fp1/alias1");
    }

    // Regression test for the relabeling attack this fix was designed to
    // close: two registrations with completely different
    // registration_id/provider/window_uuid/binding_revision, but the
    // same (account_fingerprint, pool_alias), must still collide as one
    // pool — never fork into two pools just because the operator
    // re-registered under a new alias or a different display provider
    // label.
    #[test]
    fn same_fingerprint_and_alias_collide_even_with_different_provider_label_and_identifiers() {
        let mut first_expected = sample_expected("c1", "fp-shared", "daily-cap");
        first_expected.provider = "openai-display-label".into();
        first_expected.window_uuid = "win-aaaa".into();
        let mut first = sample_registration("reg-a", true, vec![first_expected]);
        first.binding_revision = "rev-1".into();

        let mut second_expected = sample_expected("c1", "fp-shared", "daily-cap");
        second_expected.provider = "a-totally-different-provider-string".into();
        second_expected.window_uuid = "win-zzzz".into();
        let mut second = sample_registration("reg-b", true, vec![second_expected]);
        second.binding_revision = "rev-99".into();

        let loaded = vec![
            (PathBuf::from("a.json"), first),
            (PathBuf::from("b.json"), second),
        ];
        let err = check_pool_uniqueness(&loaded).unwrap_err();
        assert!(matches!(
            err,
            RegistrationError::DuplicatePoolBinding { .. }
        ));
    }

    // Narrower regression test than the one above: changes *only*
    // `provider` (the operator display alias) between the two
    // `expected[]` entries — `registration_id`, `window_uuid`, and
    // `binding_revision` are all identical. `provider` has explicitly
    // no identity role — this isolates it as the sole difference and
    // confirms it alone cannot fork one pool into two.
    #[test]
    fn provider_label_alone_never_affects_pool_identity() {
        let mut first_expected = sample_expected("c1", "fp-shared", "daily-cap");
        first_expected.provider = "openai-display-label".into();
        let first = sample_registration("reg-same", true, vec![first_expected]);

        let mut second_expected = sample_expected("c1", "fp-shared", "daily-cap");
        second_expected.provider = "a-totally-different-provider-string".into();
        let second = sample_registration("reg-same", true, vec![second_expected]);

        let loaded = vec![
            (PathBuf::from("a.json"), first),
            (PathBuf::from("b.json"), second),
        ];
        let err = check_pool_uniqueness(&loaded).unwrap_err();
        assert!(matches!(
            err,
            RegistrationError::DuplicatePoolBinding { .. }
        ));
    }

    #[test]
    fn disabled_duplicate_is_allowed() {
        let first = sample_registration("reg-a", false, vec![sample_expected("c1", "fp", "alias")]);
        let second =
            sample_registration("reg-b", false, vec![sample_expected("c1", "fp", "alias")]);
        let loaded = vec![
            (PathBuf::from("a.json"), first),
            (PathBuf::from("b.json"), second),
        ];
        assert!(check_pool_uniqueness(&loaded).is_ok());
    }

    #[test]
    fn different_alias_same_account_is_allowed_for_two_genuinely_separate_limits() {
        let first = sample_registration(
            "reg-a",
            true,
            vec![sample_expected("c1", "fp", "hourly-requests")],
        );
        let second = sample_registration(
            "reg-b",
            true,
            vec![sample_expected("c1", "fp", "daily-spend")],
        );
        let loaded = vec![
            (PathBuf::from("a.json"), first),
            (PathBuf::from("b.json"), second),
        ];
        assert!(check_pool_uniqueness(&loaded).is_ok());
    }

    #[test]
    fn collision_within_one_file_two_expected_entries_is_rejected() {
        let registration = sample_registration(
            "reg-a",
            true,
            vec![
                sample_expected("c1", "fp", "alias"),
                sample_expected("c2", "fp", "alias"),
            ],
        );
        let loaded = vec![(PathBuf::from("a.json"), registration)];
        let err = check_pool_uniqueness(&loaded).unwrap_err();
        assert!(matches!(
            err,
            RegistrationError::DuplicatePoolBinding { .. }
        ));
    }

    #[test]
    fn validate_rejects_wrong_environment_and_relative_executable() {
        let mut registration =
            sample_registration("reg-a", true, vec![sample_expected("c1", "fp", "alias")]);
        registration.transport.environment.lang = "en_US.UTF-8".into();
        assert_eq!(
            registration.validate().unwrap_err(),
            RegistrationError::InvalidEnvironment
        );
        let mut registration =
            sample_registration("reg-a", true, vec![sample_expected("c1", "fp", "alias")]);
        registration.transport.executable = "relative/bin".into();
        assert_eq!(
            registration.validate().unwrap_err(),
            RegistrationError::ExecutableNotAbsolute
        );
    }

    #[test]
    fn validate_rejects_staleness_exceeding_window_length_and_out_of_range_values() {
        let mut expected = sample_expected("c1", "fp", "alias");
        expected.max_staleness_secs = 100;
        expected.window_length_secs = 50;
        let registration = sample_registration("reg-a", true, vec![expected]);
        assert_eq!(
            registration.validate().unwrap_err(),
            RegistrationError::MaxStalenessExceedsWindowLength(0)
        );

        let mut expected = sample_expected("c1", "fp", "alias");
        expected.max_staleness_secs = 0;
        let registration = sample_registration("reg-a", true, vec![expected]);
        assert_eq!(
            registration.validate().unwrap_err(),
            RegistrationError::MaxStalenessOutOfRange(0)
        );

        let mut expected = sample_expected("c1", "fp", "alias");
        expected.window_length_secs = 31_622_401;
        let registration = sample_registration("reg-a", true, vec![expected]);
        assert_eq!(
            registration.validate().unwrap_err(),
            RegistrationError::WindowLengthOutOfRange(0)
        );
    }

    #[test]
    fn validate_rejects_capacity_ceiling_unit_mismatch() {
        let mut expected = sample_expected("c1", "fp", "alias");
        expected.capacity_ceiling = Some(CapacityCeiling {
            value: Quantity::parse("100").unwrap(),
            unit: Unit::Requests,
        });
        let registration = sample_registration("reg-a", true, vec![expected]);
        assert_eq!(
            registration.validate().unwrap_err(),
            RegistrationError::CapacityCeilingUnitMismatch(0)
        );
    }

    #[test]
    fn validate_rejects_duplicate_expected_id_within_one_registration() {
        // crate::freshness indexes `expected[]` by `id` via a `HashMap`;
        // a duplicate id would let the later entry silently win there,
        // so this must be rejected at load time instead.
        let registration = sample_registration(
            "reg-a",
            true,
            vec![
                sample_expected("c1", "fp", "alias-one"),
                sample_expected("c1", "fp", "alias-two"),
            ],
        );
        assert_eq!(
            registration.validate().unwrap_err(),
            RegistrationError::DuplicateExpectedId(1, "c1".into())
        );
    }

    #[test]
    fn load_registrations_is_fail_closed_on_one_unparseable_file() {
        let dir = tempfile::tempdir().unwrap();
        let good = sample_registration("reg-a", true, vec![sample_expected("c1", "fp", "alias")]);
        fs::write(
            dir.path().join("a.json"),
            serde_json::to_string(&good).unwrap(),
        )
        .unwrap();
        fs::write(dir.path().join("b.json"), "{ not json").unwrap();
        let err = load_registrations(dir.path()).unwrap_err();
        assert!(matches!(err, RegistrationError::Parse(_, _)));
    }

    #[test]
    fn load_registrations_detects_cross_file_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let first = sample_registration("reg-a", true, vec![sample_expected("c1", "fp", "alias")]);
        let second = sample_registration("reg-b", true, vec![sample_expected("c1", "fp", "alias")]);
        fs::write(
            dir.path().join("a.json"),
            serde_json::to_string(&first).unwrap(),
        )
        .unwrap();
        fs::write(
            dir.path().join("b.json"),
            serde_json::to_string(&second).unwrap(),
        )
        .unwrap();
        let err = load_registrations(dir.path()).unwrap_err();
        assert!(matches!(
            err,
            RegistrationError::DuplicatePoolBinding { .. }
        ));
    }

    #[test]
    fn load_registrations_accepts_a_single_valid_registration() {
        let dir = tempfile::tempdir().unwrap();
        let registration =
            sample_registration("reg-a", true, vec![sample_expected("c1", "fp", "alias")]);
        fs::write(
            dir.path().join("a.json"),
            serde_json::to_string(&registration).unwrap(),
        )
        .unwrap();
        let loaded = load_registrations(dir.path()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].registration_id, "reg-a");
    }
}
