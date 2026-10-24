//! `<state_dir>/shared-budget.json` loader (HORO-2195, ADR-0026 D1/D3).
//!
//! The file configures the locally configured monthly USD hard cap shared
//! by every participating session and agent on this host. It is
//! deliberately SEPARATE from `config.json`: the presence of `config.json`
//! flips the ADR-0024 lifetime caps, and the shared budget must not change
//! that behaviour either way.
//!
//! # States
//!
//! - [`SharedBudgetFileState::Absent`]: no file. The shared-budget path is
//!   not consulted at all.
//! - [`SharedBudgetFileState::Valid`]: parsed and validated.
//! - [`SharedBudgetFileState::Invalid`]: a file exists but cannot be
//!   trusted. Admission fails closed (`config_invalid`); it never falls
//!   back to defaults, because a typo in a budget file must not silently
//!   become "no budget".
//!
//! The file is read once at daemon start. [`LoadedSharedBudget`] keeps the
//! path and a content hash plus mtime so `Status` can report whether the
//! file on disk differs from the configuration the daemon is running.
//!
//! # Permissions
//!
//! Nothing is refused on a loose mode, but `0600` is expected and
//! [`LoadedSharedBudget::permissions_note`] says so when it is not.
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use std::collections::BTreeMap;

use libra_governor_domain::IanaTimeZone;
use libra_governor_gateway::pricing::{ModelPricing, PricingTable};

/// File name under the daemon state directory.
pub const SHARED_BUDGET_FILE_NAME: &str = "shared-budget.json";

/// The only schema version this daemon understands.
pub const SHARED_BUDGET_SCHEMA_VERSION: u32 = 1;

/// Default per-turn hold when `turn_hold_usd_cents` is omitted.
pub const DEFAULT_TURN_HOLD_USD_CENTS: u64 = 1000;

/// Upper bound on the file size; a budget file is a few hundred bytes.
const MAX_FILE_BYTES: u64 = 64 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSoftLimits {
    per_session_usd_cents: Option<i64>,
    per_agent_usd_cents: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    schema_version: Option<u32>,
    monthly_cap_usd_cents: Option<i64>,
    time_zone: Option<String>,
    turn_hold_usd_cents: Option<i64>,
    soft_limits: Option<RawSoftLimits>,
    subagent_unmeasured_charge_usd_cents: Option<i64>,
    pricing_overrides: Option<BTreeMap<String, RawModelPricing>>,
}

/// One model's USD-per-million-token prices, the same four tiers as the
/// gateway's `ModelPricing`. All four are required: a half-specified price
/// would silently price a tier at zero.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModelPricing {
    input_usd_per_mtok: f64,
    cache_creation_input_usd_per_mtok: f64,
    cache_read_input_usd_per_mtok: f64,
    output_usd_per_mtok: f64,
}

/// Most overrides a file may carry, and the longest model id: the file is
/// operator-written and tiny, and both are sanity bounds, not limits that
/// matter in practice.
const MAX_PRICING_OVERRIDES: usize = 64;
const MAX_MODEL_ID_LEN: usize = 128;
/// No real price approaches this; it only stops an obviously mistyped unit.
const MAX_USD_PER_MTOK: f64 = 100_000.0;

/// Validated operator corrections to the pinned pricing table
/// (`pricing_overrides`, HORO-2196). Every price is finite, non-negative
/// and bounded, which is what makes the `Eq` below sound.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PricingOverrides(BTreeMap<String, ModelPricing>);

impl Eq for PricingOverrides {}

impl PricingOverrides {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The pinned table with these overrides applied on top.
    pub fn table(&self) -> PricingTable {
        if self.0.is_empty() {
            PricingTable::pinned()
        } else {
            PricingTable::with_overrides(self.0.clone())
        }
    }
}

fn price(name: &str, model: &str, value: f64) -> Result<f64, String> {
    if value.is_finite() && (0.0..=MAX_USD_PER_MTOK).contains(&value) {
        Ok(value)
    } else {
        Err(format!(
            "pricing_overrides[{model:?}].{name} must be a finite number between 0 and {MAX_USD_PER_MTOK}"
        ))
    }
}

fn parse_overrides(
    raw: Option<BTreeMap<String, RawModelPricing>>,
) -> Result<PricingOverrides, String> {
    let Some(raw) = raw else {
        return Ok(PricingOverrides::default());
    };
    if raw.len() > MAX_PRICING_OVERRIDES {
        return Err(format!(
            "pricing_overrides has {} entries; at most {MAX_PRICING_OVERRIDES} are accepted",
            raw.len()
        ));
    }
    let mut out = BTreeMap::new();
    for (model, p) in raw {
        if model.trim().is_empty() || model.len() > MAX_MODEL_ID_LEN {
            return Err(format!(
                "pricing_overrides model ids must be 1-{MAX_MODEL_ID_LEN} characters"
            ));
        }
        out.insert(
            model.clone(),
            ModelPricing {
                input_usd_per_mtok: price("input_usd_per_mtok", &model, p.input_usd_per_mtok)?,
                cache_creation_input_usd_per_mtok: price(
                    "cache_creation_input_usd_per_mtok",
                    &model,
                    p.cache_creation_input_usd_per_mtok,
                )?,
                cache_read_input_usd_per_mtok: price(
                    "cache_read_input_usd_per_mtok",
                    &model,
                    p.cache_read_input_usd_per_mtok,
                )?,
                output_usd_per_mtok: price("output_usd_per_mtok", &model, p.output_usd_per_mtok)?,
            },
        );
    }
    Ok(PricingOverrides(out))
}

/// Warn-only thresholds. They never mint capacity and never deny.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SoftLimits {
    pub per_session_usd_cents: Option<u64>,
    pub per_agent_usd_cents: Option<u64>,
}

/// A validated shared-budget configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedBudgetConfig {
    pub monthly_cap_usd_cents: u64,
    pub time_zone: IanaTimeZone,
    pub turn_hold_usd_cents: u64,
    pub soft_limits: SoftLimits,
    pub subagent_unmeasured_charge_usd_cents: Option<u64>,
    /// Operator corrections to the pinned pricing table; empty means the
    /// pinned table is used as is.
    pub pricing_overrides: PricingOverrides,
}

/// What the shared-budget file said when it was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SharedBudgetFileState {
    Absent,
    Valid(SharedBudgetConfig),
    /// A file exists but is unreadable, malformed or fails validation.
    Invalid(String),
}

fn positive(name: &str, value: i64) -> Result<u64, String> {
    // `i64` on input keeps every accepted value i64-safe, which the ledger
    // requires of every amount it stores.
    u64::try_from(value)
        .ok()
        .filter(|v| *v > 0)
        .ok_or_else(|| format!("{name} must be greater than 0 (got {value})"))
}

fn optional_positive(name: &str, value: Option<i64>) -> Result<Option<u64>, String> {
    value.map(|v| positive(name, v)).transpose()
}

/// Parses and validates file contents. Pure: no I/O.
pub fn parse(bytes: &[u8]) -> SharedBudgetFileState {
    match parse_inner(bytes) {
        Ok(config) => SharedBudgetFileState::Valid(config),
        Err(reason) => SharedBudgetFileState::Invalid(reason),
    }
}

fn parse_inner(bytes: &[u8]) -> Result<SharedBudgetConfig, String> {
    let raw: RawFile = serde_json::from_slice(bytes).map_err(|e| format!("invalid JSON: {e}"))?;
    match raw.schema_version {
        Some(SHARED_BUDGET_SCHEMA_VERSION) => {}
        Some(other) => {
            return Err(format!(
            "unsupported schema_version {other} (this daemon reads {SHARED_BUDGET_SCHEMA_VERSION})"
        ))
        }
        None => return Err("schema_version is required".to_string()),
    }
    let cap = positive(
        "monthly_cap_usd_cents",
        raw.monthly_cap_usd_cents
            .ok_or("monthly_cap_usd_cents is required")?,
    )?;
    let zone_name = raw
        .time_zone
        .ok_or("time_zone is required (an IANA name; there is no UTC default)")?;
    let time_zone = IanaTimeZone::new(zone_name.clone())
        .map_err(|_| format!("time_zone {zone_name:?} is not a known IANA zone"))?;
    let turn_hold = match raw.turn_hold_usd_cents {
        Some(v) => positive("turn_hold_usd_cents", v)?,
        None => DEFAULT_TURN_HOLD_USD_CENTS,
    };
    if turn_hold > cap {
        return Err(format!(
            "turn_hold_usd_cents ({turn_hold}) must not exceed monthly_cap_usd_cents ({cap})"
        ));
    }
    let soft_limits = match raw.soft_limits {
        Some(s) => SoftLimits {
            per_session_usd_cents: optional_positive(
                "soft_limits.per_session_usd_cents",
                s.per_session_usd_cents,
            )?,
            per_agent_usd_cents: optional_positive(
                "soft_limits.per_agent_usd_cents",
                s.per_agent_usd_cents,
            )?,
        },
        None => SoftLimits::default(),
    };
    let subagent_unmeasured_charge_usd_cents = optional_positive(
        "subagent_unmeasured_charge_usd_cents",
        raw.subagent_unmeasured_charge_usd_cents,
    )?;
    let pricing_overrides = parse_overrides(raw.pricing_overrides)?;
    Ok(SharedBudgetConfig {
        monthly_cap_usd_cents: cap,
        time_zone,
        turn_hold_usd_cents: turn_hold,
        soft_limits,
        subagent_unmeasured_charge_usd_cents,
        pricing_overrides,
    })
}

/// Identity of the file contents a daemon loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFingerprint {
    pub sha256_hex: String,
    pub mtime_unix_nanos: i128,
    pub size: u64,
    pub mode: u32,
}

/// How the file on disk compares with what the daemon is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigDrift {
    /// Same bytes (or both absent).
    None,
    /// Present at start and at check time, different contents.
    Changed,
    /// Absent at start, present now.
    Appeared,
    /// Present at start, absent now.
    Removed,
    /// The file could not be read to compare.
    Unreadable,
}

/// The loaded state together with enough identity to detect later drift.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedSharedBudget {
    /// `None` only for a runtime built without a state directory.
    pub path: Option<PathBuf>,
    pub state: SharedBudgetFileState,
    pub fingerprint: Option<FileFingerprint>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

enum Read {
    Missing,
    Bytes(Vec<u8>, FileFingerprint),
    Failed(String),
}

fn read_file(path: &Path) -> Read {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Read::Missing,
        Err(e) => return Read::Failed(format!("cannot stat: {e}")),
    };
    if !meta.is_file() {
        return Read::Failed("not a regular file".to_string());
    }
    if meta.len() > MAX_FILE_BYTES {
        return Read::Failed(format!("larger than {MAX_FILE_BYTES} bytes"));
    }
    match std::fs::read(path) {
        Ok(bytes) => {
            let fingerprint = FileFingerprint {
                sha256_hex: hex(&Sha256::digest(&bytes)),
                mtime_unix_nanos: i128::from(meta.mtime()) * 1_000_000_000
                    + i128::from(meta.mtime_nsec()),
                size: meta.len(),
                mode: meta.mode() & 0o7777,
            };
            Read::Bytes(bytes, fingerprint)
        }
        // Raced with a removal between stat and read.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Read::Missing,
        Err(e) => Read::Failed(format!("cannot read: {e}")),
    }
}

/// One fresh read of the file: its state and, when it was readable, its
/// identity. A file that cannot be read or parsed is `Invalid`, never
/// `Absent`; only a missing file is `Absent`.
pub fn read_current(path: &Path) -> (SharedBudgetFileState, Option<FileFingerprint>) {
    match read_file(path) {
        Read::Missing => (SharedBudgetFileState::Absent, None),
        Read::Bytes(bytes, fingerprint) => (parse(&bytes), Some(fingerprint)),
        Read::Failed(reason) => (SharedBudgetFileState::Invalid(reason), None),
    }
}

impl LoadedSharedBudget {
    /// A runtime with no file and no path to compare against.
    pub fn absent() -> Self {
        Self {
            path: None,
            state: SharedBudgetFileState::Absent,
            fingerprint: None,
        }
    }

    /// Builds a loaded state from in-memory contents, as if read from
    /// `path`. For tests and callers that already hold the bytes.
    pub fn from_bytes(path: Option<PathBuf>, bytes: &[u8]) -> Self {
        Self {
            path,
            state: parse(bytes),
            fingerprint: None,
        }
    }

    /// Reads `<state_dir>/shared-budget.json`.
    pub fn load(state_dir: &Path) -> Self {
        let path = state_dir.join(SHARED_BUDGET_FILE_NAME);
        let (state, fingerprint) = match read_file(&path) {
            Read::Missing => (SharedBudgetFileState::Absent, None),
            Read::Bytes(bytes, fingerprint) => (parse(&bytes), Some(fingerprint)),
            Read::Failed(reason) => (SharedBudgetFileState::Invalid(reason), None),
        };
        Self {
            path: Some(path),
            state,
            fingerprint,
        }
    }

    /// The valid configuration, when there is one.
    pub fn config(&self) -> Option<&SharedBudgetConfig> {
        match &self.state {
            SharedBudgetFileState::Valid(config) => Some(config),
            _ => None,
        }
    }

    /// Re-reads the file and compares it with what was loaded.
    pub fn drift(&self) -> ConfigDrift {
        let Some(path) = &self.path else {
            return ConfigDrift::None;
        };
        let was_present = !matches!(self.state, SharedBudgetFileState::Absent);
        match read_file(path) {
            Read::Missing => {
                if was_present {
                    ConfigDrift::Removed
                } else {
                    ConfigDrift::None
                }
            }
            Read::Failed(_) => ConfigDrift::Unreadable,
            Read::Bytes(_, now) => match (&self.fingerprint, was_present) {
                (_, false) => ConfigDrift::Appeared,
                (Some(loaded), true) if loaded.sha256_hex == now.sha256_hex => ConfigDrift::None,
                // Present at start but unreadable then: any readable file now is a change.
                _ => ConfigDrift::Changed,
            },
        }
    }

    /// A note when the file mode is looser than the expected `0600`.
    pub fn permissions_note(&self) -> Option<String> {
        let mode = self.fingerprint.as_ref()?.mode;
        (mode & 0o077 != 0).then(|| format!("file mode is {mode:04o}; 0600 is expected"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str =
        r#"{"schema_version":1,"monthly_cap_usd_cents":50000,"time_zone":"America/New_York"}"#;

    fn invalid(json: &str) -> String {
        match parse(json.as_bytes()) {
            SharedBudgetFileState::Invalid(reason) => reason,
            other => panic!("expected Invalid for {json}, got {other:?}"),
        }
    }

    #[test]
    fn minimal_file_gets_the_default_hold() {
        match parse(GOOD.as_bytes()) {
            SharedBudgetFileState::Valid(c) => {
                assert_eq!(c.monthly_cap_usd_cents, 50_000);
                assert_eq!(c.turn_hold_usd_cents, DEFAULT_TURN_HOLD_USD_CENTS);
                assert_eq!(c.time_zone.as_str(), "America/New_York");
                assert_eq!(c.soft_limits, SoftLimits::default());
                assert_eq!(c.subagent_unmeasured_charge_usd_cents, None);
            }
            other => panic!("expected Valid, got {other:?}"),
        }
    }

    #[test]
    fn full_file_round_trips() {
        let json = r#"{"schema_version":1,"monthly_cap_usd_cents":9000,"time_zone":"UTC",
            "turn_hold_usd_cents":900,
            "soft_limits":{"per_session_usd_cents":100,"per_agent_usd_cents":50},
            "subagent_unmeasured_charge_usd_cents":25}"#;
        let SharedBudgetFileState::Valid(c) = parse(json.as_bytes()) else {
            panic!("expected Valid");
        };
        assert_eq!(c.turn_hold_usd_cents, 900);
        assert_eq!(c.soft_limits.per_session_usd_cents, Some(100));
        assert_eq!(c.soft_limits.per_agent_usd_cents, Some(50));
        assert_eq!(c.subagent_unmeasured_charge_usd_cents, Some(25));
    }

    #[test]
    fn hold_equal_to_cap_is_valid_and_hold_above_cap_is_not() {
        let at = r#"{"schema_version":1,"monthly_cap_usd_cents":1000,"time_zone":"UTC","turn_hold_usd_cents":1000}"#;
        assert!(matches!(
            parse(at.as_bytes()),
            SharedBudgetFileState::Valid(_)
        ));
        let over = at.replace(
            "\"turn_hold_usd_cents\":1000",
            "\"turn_hold_usd_cents\":1001",
        );
        assert!(invalid(&over).contains("must not exceed"));
    }

    #[test]
    fn every_invalid_variant_is_rejected_with_a_reason() {
        let cases = [
            ("not json", "invalid JSON"),
            (
                r#"{"schema_version":1,"monthly_cap_usd_cents":1,"time_zone":"UTC","extra":1}"#,
                "unknown field",
            ),
            (
                r#"{"schema_version":1,"monthly_cap_usd_cents":100}"#,
                "time_zone is required",
            ),
            (
                r#"{"schema_version":1,"monthly_cap_usd_cents":100,"time_zone":"Mars/Olympus"}"#,
                "not a known IANA zone",
            ),
            (
                r#"{"schema_version":1,"monthly_cap_usd_cents":100,"time_zone":""}"#,
                "not a known IANA zone",
            ),
            (
                r#"{"schema_version":1,"monthly_cap_usd_cents":0,"time_zone":"UTC"}"#,
                "greater than 0",
            ),
            (
                r#"{"schema_version":1,"monthly_cap_usd_cents":-5,"time_zone":"UTC"}"#,
                "greater than 0",
            ),
            (
                r#"{"schema_version":1,"time_zone":"UTC"}"#,
                "monthly_cap_usd_cents is required",
            ),
            (
                r#"{"schema_version":1,"monthly_cap_usd_cents":9223372036854775808,"time_zone":"UTC"}"#,
                "invalid JSON",
            ),
            (
                r#"{"schema_version":1,"monthly_cap_usd_cents":10.5,"time_zone":"UTC"}"#,
                "invalid JSON",
            ),
            (
                r#"{"monthly_cap_usd_cents":100,"time_zone":"UTC"}"#,
                "schema_version is required",
            ),
            (
                r#"{"schema_version":2,"monthly_cap_usd_cents":100,"time_zone":"UTC"}"#,
                "unsupported schema_version",
            ),
            (
                r#"{"schema_version":1,"monthly_cap_usd_cents":100,"time_zone":"UTC","turn_hold_usd_cents":0}"#,
                "greater than 0",
            ),
            (
                r#"{"schema_version":1,"monthly_cap_usd_cents":10000,"time_zone":"UTC","soft_limits":{"per_session_usd_cents":0}}"#,
                "soft_limits.per_session",
            ),
            (
                r#"{"schema_version":1,"monthly_cap_usd_cents":10000,"time_zone":"UTC","soft_limits":{"per_agent_usd_cents":-1}}"#,
                "soft_limits.per_agent",
            ),
            (
                r#"{"schema_version":1,"monthly_cap_usd_cents":100,"time_zone":"UTC","soft_limits":{"bogus":1}}"#,
                "unknown field",
            ),
            (
                r#"{"schema_version":1,"monthly_cap_usd_cents":10000,"time_zone":"UTC","subagent_unmeasured_charge_usd_cents":0}"#,
                "subagent_unmeasured",
            ),
        ];
        for (json, needle) in cases {
            let reason = invalid(json);
            assert!(reason.contains(needle), "{json}: {reason}");
        }
    }

    #[test]
    fn load_reports_absent_valid_invalid_and_drift() {
        let dir = tempfile::tempdir().unwrap();
        let absent = LoadedSharedBudget::load(dir.path());
        assert_eq!(absent.state, SharedBudgetFileState::Absent);
        assert_eq!(absent.drift(), ConfigDrift::None);

        let path = dir.path().join(SHARED_BUDGET_FILE_NAME);
        std::fs::write(&path, GOOD).unwrap();
        assert_eq!(absent.drift(), ConfigDrift::Appeared);

        let loaded = LoadedSharedBudget::load(dir.path());
        assert!(loaded.config().is_some());
        assert_eq!(loaded.drift(), ConfigDrift::None);

        std::fs::write(&path, GOOD.replace("50000", "60000")).unwrap();
        assert_eq!(loaded.drift(), ConfigDrift::Changed);
        std::fs::write(&path, GOOD).unwrap();
        assert_eq!(loaded.drift(), ConfigDrift::None);

        std::fs::remove_file(&path).unwrap();
        assert_eq!(loaded.drift(), ConfigDrift::Removed);

        std::fs::write(&path, "{").unwrap();
        let bad = LoadedSharedBudget::load(dir.path());
        assert!(matches!(bad.state, SharedBudgetFileState::Invalid(_)));
        assert!(bad.config().is_none());
        assert_eq!(bad.drift(), ConfigDrift::None);
        std::fs::write(&path, GOOD).unwrap();
        assert_eq!(bad.drift(), ConfigDrift::Changed);
    }

    #[test]
    fn oversized_and_non_file_paths_are_invalid_not_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SHARED_BUDGET_FILE_NAME);
        std::fs::write(&path, vec![b' '; (MAX_FILE_BYTES + 1) as usize]).unwrap();
        assert!(matches!(
            LoadedSharedBudget::load(dir.path()).state,
            SharedBudgetFileState::Invalid(_)
        ));
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(matches!(
            LoadedSharedBudget::load(dir.path()).state,
            SharedBudgetFileState::Invalid(_)
        ));
    }

    #[test]
    fn loose_permissions_are_noted_not_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SHARED_BUDGET_FILE_NAME);
        std::fs::write(&path, GOOD).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            LoadedSharedBudget::load(dir.path()).permissions_note(),
            None
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let loose = LoadedSharedBudget::load(dir.path());
        assert!(
            loose.config().is_some(),
            "a loose mode is noted, never refused"
        );
        assert!(loose.permissions_note().unwrap().contains("0600"));
    }
    const FLAT_PRICE: &str = r#"{"input_usd_per_mtok":2.0,"cache_creation_input_usd_per_mtok":2.5,
        "cache_read_input_usd_per_mtok":0.2,"output_usd_per_mtok":10.0}"#;

    fn with_overrides(body: &str) -> String {
        format!(
            r#"{{"schema_version":1,"monthly_cap_usd_cents":10000,"time_zone":"UTC",
            "pricing_overrides":{body}}}"#
        )
    }

    #[test]
    fn pricing_overrides_are_optional_and_default_to_the_pinned_table() {
        let SharedBudgetFileState::Valid(c) = parse(GOOD.as_bytes()) else {
            panic!("GOOD is valid");
        };
        assert!(c.pricing_overrides.is_empty());
        assert!(c
            .pricing_overrides
            .table()
            .lookup("claude-sonnet-4-5")
            .is_some());
        assert_eq!(c.pricing_overrides.table().override_count(), 0);
    }

    #[test]
    fn valid_pricing_overrides_extend_and_replace_the_pinned_table() {
        let json = with_overrides(&format!(
            r#"{{"new-model-1":{FLAT_PRICE},"claude-sonnet-4-5":{FLAT_PRICE}}}"#
        ));
        let SharedBudgetFileState::Valid(c) = parse(json.as_bytes()) else {
            panic!("expected valid");
        };
        assert_eq!(c.pricing_overrides.len(), 2);
        let table = c.pricing_overrides.table();
        assert_eq!(
            table.lookup("new-model-1").unwrap().output_usd_per_mtok,
            10.0
        );
        assert_eq!(
            table
                .lookup("claude-sonnet-4-5")
                .unwrap()
                .input_usd_per_mtok,
            2.0
        );
    }

    #[test]
    fn invalid_pricing_overrides_make_the_whole_file_invalid() {
        let too_many = {
            let entries: Vec<String> = (0..65).map(|i| format!(r#""m{i}":{FLAT_PRICE}"#)).collect();
            with_overrides(&format!("{{{}}}", entries.join(",")))
        };
        let cases = [
            (
                with_overrides(
                    r#"{"m":{"input_usd_per_mtok":-1.0,"cache_creation_input_usd_per_mtok":1.0,
                    "cache_read_input_usd_per_mtok":1.0,"output_usd_per_mtok":1.0}}"#,
                ),
                "input_usd_per_mtok must be a finite number",
            ),
            (
                with_overrides(
                    r#"{"m":{"input_usd_per_mtok":1.0,"cache_creation_input_usd_per_mtok":1.0,
                    "cache_read_input_usd_per_mtok":1.0,"output_usd_per_mtok":1e12}}"#,
                ),
                "output_usd_per_mtok must be a finite number",
            ),
            (
                with_overrides(r#"{"m":{"input_usd_per_mtok":1.0}}"#),
                "missing field",
            ),
            (
                with_overrides(
                    r#"{"m":{"input_usd_per_mtok":1.0,"cache_creation_input_usd_per_mtok":1.0,
                    "cache_read_input_usd_per_mtok":1.0,"output_usd_per_mtok":1.0,"bogus":1}}"#,
                ),
                "unknown field",
            ),
            (
                with_overrides(&format!(r#"{{"  ":{FLAT_PRICE}}}"#)),
                "model ids must be",
            ),
            (with_overrides("[]"), "invalid JSON"),
            (too_many, "at most 64"),
        ];
        for (json, needle) in cases {
            let reason = match parse(json.as_bytes()) {
                SharedBudgetFileState::Invalid(r) => r,
                other => panic!("{json}: expected invalid, got {other:?}"),
            };
            assert!(reason.contains(needle), "{needle}: {reason}");
        }
    }
}
