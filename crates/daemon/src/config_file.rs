//! Optional, additive `config.json` overrides for the admission
//! [`Policy`] preset and the enforcement gateway (HORO-1146).
//!
//! # Why this exists
//!
//! Before this module, `libra-governor daemon run` (and, transitively,
//! `hook user-prompt-submit`'s spawn-if-absent path — see
//! `crates/cli/src/client.rs`) hardcoded `policy: default_admission_policy()`
//! (the `balanced` preset) and `gateway: None`, with no way for a real
//! user to select `deadline_first`/`cost_first`/`strict_budget`, a
//! custom resource/time target, or to turn the gateway on at all. HORO-1137's
//! presets and HORO-1144's gateway were real and tested at the code
//! level but unreachable through the shipped binary. This module reads
//! an optional file from the daemon's own state directory and produces
//! the same `(Policy, Option<GatewayConfig>)` pair `daemon_cmd::run`
//! already builds by hand — nothing here evaluates a policy or makes an
//! admission/reservation decision itself (that stays in `server.rs`, per
//! this repo's own architecture rule: admission/policy decisions belong
//! in `crates/daemon`, not the CLI or a config-parsing layer).
//!
//! # Why JSON, not TOML
//!
//! The ticket that opened this gap described the file as
//! "`config.toml` (or similarly named)". No `toml` crate is present
//! anywhere in this workspace's dependency graph (`Cargo.lock`), and
//! `crates/daemon` already depends on `serde_json` — using it here adds
//! no new third-party dependency and, unlike a bespoke parser, has
//! correct quoting/array/escaping semantics for the one field
//! (`upstream_host_allowlist`) this repo's own gateway module doctrine
//! ("the upstream is configuration, never input") insists must never be
//! mishandled.
//!
//! # Purely additive
//!
//! [`load_overrides`] returns `Ok((None, None))` when the file does not
//! exist — every existing deployment with no `config.json` gets exactly
//! today's hardcoded defaults, unchanged. A present-but-invalid file
//! does not abort daemon startup: the caller logs the error and falls
//! back to the defaults, the same "a mistyped config must not take away
//! the daemon's core function" doctrine `crates/daemon/src/server.rs`'s
//! `start_gateway` already applies to a bad `[gateway]` table.

use crate::server::{DEFAULT_RESOURCE_TARGET_TOKENS, DEFAULT_TIME_TARGET_SECS};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use libra_governor_domain::{
    preset_by_name, CompletionContract, CompletionCriterion, Policy, PolicyPresetInputs,
    PolicyValidationError, PresetError, ResourceAmount,
};
use libra_governor_gateway::config::{GatewayConfig, GatewayCredentialMode};
use libra_governor_gateway::credential::CredentialCommand;
use serde::Deserialize;

/// The file name resolved under the daemon's state directory (see
/// `crates/daemon/src/paths.rs`). Not itself TOML — see module docs.
pub const CONFIG_FILE_NAME: &str = "config.json";

#[derive(Debug, thiserror::Error)]
pub enum ConfigFileError {
    #[error("io error reading {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("could not parse {path} as JSON: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error(
        "unknown policy preset {preset:?} (expected one of: {})",
        libra_governor_domain::NAMED_PRESETS.join(", ")
    )]
    UnknownPreset { preset: String },
    #[error("[policy] preset {preset:?} rejected its inputs: {source}")]
    InvalidPolicy {
        preset: String,
        source: PolicyValidationError,
    },
    #[error(
        "[gateway].credential_mode = \"governor_held\" requires a non-empty \
         [gateway].credential_command"
    )]
    GovernorHeldMissingCredentialCommand,
    /// HORO-1766: a `[launcher].pools[].alias` may never contain `/` — it
    /// is namespaced into a ledger `PoolId` as `launch:<alias>`, and a
    /// `/` would let an alias collide with, or be confused for, a
    /// different pool-id segment boundary elsewhere in the ledger's flat
    /// `PoolId` string space.
    #[error("[launcher].pools[].alias {alias:?} must not contain '/'")]
    LaunchAliasContainsSlash { alias: String },
    /// An empty alias is never a meaningful pool name and would collide
    /// with every other empty alias in the same config.
    #[error("[launcher].pools[].alias must not be empty")]
    LaunchAliasEmpty,
    /// Two pool entries in the same `[launcher]` section named the same
    /// alias — refused rather than silently letting the later entry win,
    /// which would make the file's own meaning depend on declaration
    /// order.
    #[error("[launcher].pools[] declares alias {alias:?} more than once")]
    LaunchAliasDuplicate { alias: String },
}

#[derive(Debug, Deserialize, Default)]
struct RawConfigFile {
    policy: Option<RawPolicyConfig>,
    gateway: Option<RawGatewayConfig>,
    extensions: Option<RawExtensionsConfig>,
    launcher: Option<RawLaunchGateConfig>,
}

#[derive(Debug, Deserialize)]
struct RawPolicyConfig {
    preset: String,
    #[serde(default = "default_resource_target_tokens")]
    resource_target_tokens: u64,
    #[serde(default = "default_time_target_secs")]
    time_target_secs: u64,
}

fn default_resource_target_tokens() -> u64 {
    DEFAULT_RESOURCE_TARGET_TOKENS
}

fn default_time_target_secs() -> u64 {
    DEFAULT_TIME_TARGET_SECS
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawCredentialMode {
    GovernorHeld,
    PassThroughSubscription,
}

#[derive(Debug, Deserialize)]
struct RawGatewayConfig {
    bind_addr: SocketAddr,
    token_path: PathBuf,
    credential_mode: RawCredentialMode,
    #[serde(default)]
    credential_command: Option<String>,
    #[serde(default)]
    credential_args: Vec<String>,
    #[serde(default)]
    upstream_host_allowlist: Option<Vec<String>>,
}

/// One outbound extension surface (`business_context_provider` or
/// `policy_webhook`) as `config.json` writes it (HORO-1174). No default
/// for `timeout_ms` — the JSON-shape default/cap policy lives entirely in
/// `libra_governor_extension::config`, and a config file that omits it
/// would silently pick whichever default this module happened to
/// hardcode; requiring it explicit here keeps the one source of truth in
/// the extension crate itself, checked at [`libra_governor_extension::validate`]
/// time.
#[derive(Debug, Deserialize)]
struct RawExtensionSurface {
    url: String,
    timeout_ms: u64,
    secret_command: String,
    #[serde(default)]
    secret_args: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawExtensionsEvents {
    url: String,
    #[serde(default = "default_events_timeout_ms")]
    timeout_ms: u64,
    #[serde(default = "default_events_max_attempts")]
    max_attempts: u32,
    kinds: Vec<String>,
    secret_command: String,
    #[serde(default)]
    secret_args: Vec<String>,
}

fn default_events_timeout_ms() -> u64 {
    libra_governor_extension::EVENTS_TIMEOUT_DEFAULT_MS
}

fn default_events_max_attempts() -> u32 {
    libra_governor_extension::EVENTS_MAX_ATTEMPTS_DEFAULT
}

/// `[extensions]` — the local extension points (HORO-1174): Business
/// Context Provider, Policy Webhook, and signed event delivery. Every
/// field is optional and additive — absent (`None`) means that surface is
/// off. Actual URL/scheme/host/timeout validation happens once at daemon
/// startup via `libra_governor_extension::validate`, not here — this
/// struct is a pure parse, matching `RawGatewayConfig`'s own discipline
/// (`resolve_gateway` does not resolve credentials either).
#[derive(Debug, Deserialize, Default)]
struct RawExtensionsConfig {
    business_context_provider: Option<RawExtensionSurface>,
    policy_webhook: Option<RawExtensionSurface>,
    events: Option<RawExtensionsEvents>,
}

fn resolve_extensions(raw: RawExtensionsConfig) -> libra_governor_extension::ExtensionConfig {
    let surface = |s: RawExtensionSurface| libra_governor_extension::SurfaceConfig {
        url: s.url,
        timeout_ms: s.timeout_ms,
        secret_command: s.secret_command,
        secret_args: s.secret_args,
    };
    libra_governor_extension::ExtensionConfig {
        business_context_provider: raw.business_context_provider.map(surface),
        policy_webhook: raw.policy_webhook.map(surface),
        events: raw.events.map(|e| libra_governor_extension::EventsConfig {
            url: e.url,
            timeout_ms: e.timeout_ms,
            max_attempts: e.max_attempts,
            kinds: e.kinds,
            secret_command: e.secret_command,
            secret_args: e.secret_args,
        }),
    }
}

/// The `(Policy, GatewayConfig, ExtensionConfig, LaunchGateConfig)`
/// overrides [`load_overrides`] produces. A struct, not a bare tuple —
/// HORO-1766 is the second field added since this was a 2-tuple
/// (HORO-1174 added `extensions` to what was originally `(Policy,
/// GatewayConfig)`), and a positional tuple that keeps growing a new slot
/// per ticket is exactly the kind of "what does the 4th field mean again"
/// ambiguity a named struct exists to avoid.
#[derive(Debug, Default)]
pub struct ConfigOverrides {
    /// `true` when a `config.json` exists, even if it is empty, names no
    /// recognized key, or was rejected. Only a daemon with no configuration
    /// file at all is turn-scoped (HORO-1727 S4), so a misspelled or
    /// partial file can never silently drop the lifetime bound.
    pub config_file_present: bool,
    pub policy: Option<Policy>,
    pub gateway: Option<GatewayConfig>,
    pub extensions: Option<libra_governor_extension::ExtensionConfig>,
    /// HORO-1766's opt-in launch-gate admission primitive. `None` — the
    /// default — means no `[launcher]` section was present, which is what
    /// makes every `Admit` answer `Unavailable { reason: not_enabled }`
    /// (ADR-0020: off by default).
    pub launcher: Option<crate::launch_gate::LaunchGateConfig>,
}

impl ConfigOverrides {
    /// The lifetime scope the daemon runs with (HORO-1727 S4). Only a daemon
    /// with no `config.json` at all is turn-scoped; any config file - even
    /// an empty or misspelled one - and a rejected one (see
    /// [`Self::rejected_fallback`]) stays lifetime-bounded.
    pub fn lifetime_scope(&self) -> libra_governor_domain::LifetimeScope {
        if !self.config_file_present
            && self.policy.is_none()
            && self.gateway.is_none()
            && self.extensions.is_none()
            && self.launcher.is_none()
        {
            libra_governor_domain::LifetimeScope::TurnScoped
        } else {
            libra_governor_domain::LifetimeScope::LifetimeBounded
        }
    }

    /// What the daemon runs with when `config.json` is present but
    /// rejected. A rejected config must stay lifetime-bounded, so it names
    /// [`crate::server::legacy_bounded_policy`] explicitly instead of
    /// taking the no-config path, whose default HORO-1727 S4 may change.
    pub fn rejected_fallback() -> Self {
        Self {
            config_file_present: true,
            policy: Some(crate::server::legacy_bounded_policy()),
            ..Self::default()
        }
    }
}

/// Reads and parses `<state_dir>/config.json`, if it exists, into the
/// [`ConfigOverrides`] `daemon_cmd::run` should use in place of its own
/// hardcoded defaults. Every field defaults to `None` when the file is
/// simply absent — the normal case for every existing deployment.
pub fn load_overrides(state_dir: &Path) -> Result<ConfigOverrides, ConfigFileError> {
    let path = state_dir.join(CONFIG_FILE_NAME);
    if !path.exists() {
        return Ok(ConfigOverrides::default());
    }

    let raw_text = std::fs::read_to_string(&path).map_err(|source| ConfigFileError::Io {
        path: path.clone(),
        source,
    })?;
    let raw: RawConfigFile =
        serde_json::from_str(&raw_text).map_err(|source| ConfigFileError::Parse {
            path: path.clone(),
            source,
        })?;

    let policy = raw.policy.map(resolve_policy).transpose()?;
    let gateway = raw.gateway.map(resolve_gateway).transpose()?;
    let extensions = raw.extensions.map(resolve_extensions);
    let launcher = raw.launcher.map(resolve_launcher).transpose()?;
    Ok(ConfigOverrides {
        config_file_present: true,
        policy,
        gateway,
        extensions,
        launcher,
    })
}

/// One `[launcher].pools[]` entry as `config.json` writes it (HORO-1766).
#[derive(Debug, Deserialize)]
struct RawLaunchPoolConfig {
    alias: String,
    capacity: u64,
    #[serde(default = "default_pool_enabled")]
    enabled: bool,
}

fn default_pool_enabled() -> bool {
    true
}

fn default_hold_ttl_secs() -> u64 {
    crate::launch_gate::DEFAULT_HOLD_TTL_SECS
}

/// `[launcher]` — the opt-in launch-gate admission primitive over shared
/// slot pools (HORO-1766). Absent (`None` on [`RawConfigFile`]) means the
/// gate is off; see [`crate::launch_gate`] module docs and
/// `docs/adr/0020-launch-gate-actuator.md`.
#[derive(Debug, Deserialize)]
struct RawLaunchGateConfig {
    #[serde(default)]
    pools: Vec<RawLaunchPoolConfig>,
    #[serde(default = "default_hold_ttl_secs")]
    hold_ttl_secs: u64,
}

/// Validates parse-level shape only (alias non-empty, no `/`, no
/// in-file duplicates) — this function has no ledger access (it runs
/// before `LedgerStore::open`, and `doctor_cmd` calls it with no daemon
/// running at all), so it cannot check an alias against an
/// already-persisted pool's unit. That check instead happens lazily
/// inside `crate::launch_gate::handle_admit`, on first use of a pool,
/// which does have a ledger handle — see that function's own docs for
/// why a unit mismatch there is a per-pool "disabled, logged" outcome
/// rather than an error this function could raise.
fn resolve_launcher(
    raw: RawLaunchGateConfig,
) -> Result<crate::launch_gate::LaunchGateConfig, ConfigFileError> {
    let mut seen = std::collections::HashSet::with_capacity(raw.pools.len());
    let mut pools = Vec::with_capacity(raw.pools.len());
    for p in raw.pools {
        if p.alias.is_empty() {
            return Err(ConfigFileError::LaunchAliasEmpty);
        }
        if p.alias.contains('/') {
            return Err(ConfigFileError::LaunchAliasContainsSlash { alias: p.alias });
        }
        if !seen.insert(p.alias.clone()) {
            return Err(ConfigFileError::LaunchAliasDuplicate { alias: p.alias });
        }
        pools.push(crate::launch_gate::LaunchPoolConfig {
            alias: p.alias,
            capacity: p.capacity,
            enabled: p.enabled,
        });
    }
    Ok(crate::launch_gate::LaunchGateConfig {
        pools,
        hold_ttl_secs: raw.hold_ttl_secs,
    })
}

fn resolve_policy(raw: RawPolicyConfig) -> Result<Policy, ConfigFileError> {
    let inputs = PolicyPresetInputs {
        resource_target: ResourceAmount::Tokens(raw.resource_target_tokens),
        time_target_secs: raw.time_target_secs,
        quality_floor: CompletionContract::first(vec![CompletionCriterion::required(
            "required verification (tests/build/lint) passes",
        )]),
    };
    let preset = raw.preset.clone();
    let policy = preset_by_name(&raw.preset, inputs).map_err(|err| match err {
        PresetError::UnknownPreset { name, .. } => ConfigFileError::UnknownPreset { preset: name },
        PresetError::Invalid(source) => ConfigFileError::InvalidPolicy {
            preset: preset.clone(),
            source,
        },
    })?;
    // Attach the same default turn-scoped ceiling (HORO-1727 Decision B)
    // every `config.json` user gets, same as
    // `crates/daemon::default_admission_policy` — see
    // `crate::server::default_turn_bound` for the derivation. Can never
    // fail in practice: the default turn bound's amounts are
    // `ResourceAmount::Tokens`, matching every named preset's own
    // `resource_target` kind here (`RawPolicyConfig::resource_target_tokens`
    // has no other representable kind), and `ceiling >=
    // cold_start_request` is a fixed invariant of the constants
    // themselves.
    policy
        .with_turn_bound(crate::server::default_turn_bound())
        .map_err(|source| ConfigFileError::InvalidPolicy { preset, source })
}

fn resolve_gateway(raw: RawGatewayConfig) -> Result<GatewayConfig, ConfigFileError> {
    let credential_mode = match raw.credential_mode {
        RawCredentialMode::PassThroughSubscription => {
            GatewayCredentialMode::PassThroughSubscription
        }
        RawCredentialMode::GovernorHeld => {
            let program = raw
                .credential_command
                .filter(|c| !c.is_empty())
                .ok_or(ConfigFileError::GovernorHeldMissingCredentialCommand)?;
            GatewayCredentialMode::GovernorHeld {
                credential: CredentialCommand::new(program, raw.credential_args),
            }
        }
    };

    let mut config = GatewayConfig::new(raw.bind_addr, raw.token_path, credential_mode);
    if let Some(allowlist) = raw.upstream_host_allowlist {
        config.upstream_host_allowlist = allowlist;
    }
    Ok(config)
}

/// The gateway capability token's filename inside the state directory,
/// used when `config.json` does not name a `gateway.token_path`.
pub const DEFAULT_GATEWAY_TOKEN_FILE_NAME: &str = "gateway.token";

/// The path of the gateway capability token the daemon serves: the
/// `gateway.token_path` `config.json` names, else
/// `<state_dir>/gateway.token`. A missing or unparseable `config.json`
/// yields the default, because then the daemon runs no gateway at all.
pub fn gateway_token_path(state_dir: &Path) -> PathBuf {
    match load_overrides(state_dir) {
        Ok(ConfigOverrides {
            gateway: Some(gateway),
            ..
        }) => gateway.token_path,
        _ => state_dir.join(DEFAULT_GATEWAY_TOKEN_FILE_NAME),
    }
}

/// What `doctor` can say about a `config.json` that parses but that the
/// daemon may still not honour. Never carries a config value, only key
/// paths and the gateway's own rejection text (which may name the bind
/// address or an allowlist host, never a secret).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ConfigReview {
    /// Why the daemon's own gateway validation rejects the gateway block
    /// (it would log "gateway disabled" and start no listener).
    pub gateway_rejection: Option<String>,
    /// Keys the daemon silently ignores, as dotted paths.
    pub unknown_keys: Vec<String>,
}

// The keys each `Raw*` struct above reads. Unknown keys are tolerated, not
// rejected (`deny_unknown_fields` would make one extra key drop a whole
// config back to defaults), so `doctor` lists them instead. Keep these in
// step with the `Raw*` structs; the `review_*` tests exercise each level.
const TOP_KEYS: &[&str] = &["policy", "gateway", "extensions", "launcher"];
const POLICY_KEYS: &[&str] = &["preset", "resource_target_tokens", "time_target_secs"];
const GATEWAY_KEYS: &[&str] = &[
    "bind_addr",
    "token_path",
    "credential_mode",
    "credential_command",
    "credential_args",
    "upstream_host_allowlist",
];
const EXTENSIONS_KEYS: &[&str] = &["business_context_provider", "policy_webhook", "events"];
const SURFACE_KEYS: &[&str] = &["url", "timeout_ms", "secret_command", "secret_args"];
const EVENTS_KEYS: &[&str] = &[
    "url",
    "timeout_ms",
    "max_attempts",
    "kinds",
    "secret_command",
    "secret_args",
];
const LAUNCHER_KEYS: &[&str] = &["pools", "hold_ttl_secs"];
const POOL_KEYS: &[&str] = &["alias", "capacity", "enabled"];

/// Reviews a `config.json` that parses, using the daemon's own gateway
/// validation (`libra_governor_gateway::config::validate`, the call
/// `start_gateway` makes) rather than a copy of its rules. It does not
/// resolve credentials or run any command.
pub fn review_config(state_dir: &Path) -> Result<ConfigReview, ConfigFileError> {
    let overrides = load_overrides(state_dir)?;
    let mut review = ConfigReview::default();

    if let Some(gateway) = overrides.gateway {
        let policy_kind = overrides
            .policy
            .unwrap_or_else(crate::server::default_admission_policy)
            .resource
            .target
            .kind();
        if let Err(e) = libra_governor_gateway::config::validate(gateway, policy_kind) {
            review.gateway_rejection = Some(e.to_string());
        }
    }

    // `load_overrides` just read this file, so a failure here is a race
    // with an editor; the parse result above stands.
    if let Ok(text) = std::fs::read_to_string(state_dir.join(CONFIG_FILE_NAME)) {
        if let Ok(root) = serde_json::from_str::<serde_json::Value>(&text) {
            let mut unknown = Vec::new();
            let obj = |v: &serde_json::Value, known: &[&str], path: &str, out: &mut Vec<String>| {
                if let Some(map) = v.as_object() {
                    for key in map.keys() {
                        if !known.contains(&key.as_str()) {
                            out.push(if path.is_empty() {
                                key.clone()
                            } else {
                                format!("{path}.{key}")
                            });
                        }
                    }
                }
            };
            obj(&root, TOP_KEYS, "", &mut unknown);
            if let Some(v) = root.get("policy") {
                obj(v, POLICY_KEYS, "policy", &mut unknown);
            }
            if let Some(v) = root.get("gateway") {
                obj(v, GATEWAY_KEYS, "gateway", &mut unknown);
            }
            if let Some(ext) = root.get("extensions") {
                obj(ext, EXTENSIONS_KEYS, "extensions", &mut unknown);
                for name in ["business_context_provider", "policy_webhook"] {
                    if let Some(v) = ext.get(name) {
                        obj(v, SURFACE_KEYS, &format!("extensions.{name}"), &mut unknown);
                    }
                }
                if let Some(v) = ext.get("events") {
                    obj(v, EVENTS_KEYS, "extensions.events", &mut unknown);
                }
            }
            if let Some(launcher) = root.get("launcher") {
                obj(launcher, LAUNCHER_KEYS, "launcher", &mut unknown);
                if let Some(pools) = launcher.get("pools").and_then(|p| p.as_array()) {
                    for pool in pools {
                        obj(pool, POOL_KEYS, "launcher.pools[]", &mut unknown);
                    }
                }
            }
            unknown.sort();
            unknown.dedup();
            review.unknown_keys = unknown;
        }
    }
    Ok(review)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_config_file_produces_no_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let overrides = load_overrides(dir.path()).unwrap();
        let (policy, gateway) = (overrides.policy, overrides.gateway);
        assert!(policy.is_none());
        assert!(gateway.is_none());
    }

    #[test]
    fn a_bare_preset_name_selects_the_named_policy_with_sensible_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            r#"{"policy": {"preset": "deadline_first"}}"#,
        )
        .unwrap();

        let overrides = load_overrides(dir.path()).unwrap();
        let (policy, gateway) = (overrides.policy, overrides.gateway);
        let policy = policy.expect("policy override must be present");
        assert_eq!(policy.name, "deadline_first");
        assert!(gateway.is_none());
    }

    #[test]
    fn a_bare_balanced_preset_uses_the_legacy_bounded_targets() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            r#"{"policy": {"preset": "balanced"}}"#,
        )
        .unwrap();

        let policy = load_overrides(dir.path()).unwrap().policy.unwrap();
        let legacy = crate::server::legacy_bounded_policy();
        assert_eq!(policy.resource, legacy.resource);
        assert_eq!(policy.time, legacy.time);
    }

    #[test]
    fn a_rejected_config_falls_back_to_the_legacy_bounded_policy() {
        let overrides = ConfigOverrides::rejected_fallback();
        assert_eq!(
            overrides.policy,
            Some(crate::server::legacy_bounded_policy())
        );
        assert!(overrides.gateway.is_none());
    }

    #[test]
    fn only_a_daemon_with_no_configuration_at_all_is_turn_scoped() {
        use libra_governor_domain::LifetimeScope;
        assert_eq!(
            ConfigOverrides::default().lifetime_scope(),
            LifetimeScope::TurnScoped
        );
        let named_preset = ConfigOverrides {
            policy: Some(crate::server::legacy_bounded_policy()),
            ..ConfigOverrides::default()
        };
        assert_eq!(
            named_preset.lifetime_scope(),
            LifetimeScope::LifetimeBounded
        );
        assert_eq!(
            ConfigOverrides::rejected_fallback().lifetime_scope(),
            LifetimeScope::LifetimeBounded
        );
    }

    #[test]
    fn an_empty_or_misspelled_config_file_stays_lifetime_bounded() {
        use libra_governor_domain::LifetimeScope;
        for body in [
            "{}",
            r#"{"polcy": {"preset": "balanced"}}"#,
            r#"{"policy": null}"#,
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join(CONFIG_FILE_NAME), body).unwrap();
            let overrides = load_overrides(dir.path()).unwrap();
            assert_eq!(
                overrides.lifetime_scope(),
                LifetimeScope::LifetimeBounded,
                "config body {body} must not drop the lifetime bound"
            );
        }
        // No file at all is the one turn-scoped case.
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load_overrides(dir.path()).unwrap().lifetime_scope(),
            LifetimeScope::TurnScoped
        );
    }

    #[test]
    fn an_unknown_preset_name_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            r#"{"policy": {"preset": "not_a_real_preset"}}"#,
        )
        .unwrap();

        let err = load_overrides(dir.path()).unwrap_err();
        assert!(matches!(err, ConfigFileError::UnknownPreset { .. }));
    }

    #[test]
    fn a_full_gateway_table_produces_a_governor_held_config() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            r#"{
                "gateway": {
                    "bind_addr": "127.0.0.1:18080",
                    "token_path": "/tmp/token",
                    "credential_mode": "governor_held",
                    "credential_command": "op",
                    "credential_args": ["read", "op://vault/item"],
                    "upstream_host_allowlist": ["api.anthropic.com", "api.example.com"]
                }
            }"#,
        )
        .unwrap();

        let overrides = load_overrides(dir.path()).unwrap();
        let (policy, gateway) = (overrides.policy, overrides.gateway);
        assert!(policy.is_none());
        let gateway = gateway.expect("gateway override must be present");
        assert_eq!(gateway.bind_addr, "127.0.0.1:18080".parse().unwrap());
        assert_eq!(
            gateway.upstream_host_allowlist,
            vec![
                "api.anthropic.com".to_string(),
                "api.example.com".to_string()
            ]
        );
        assert!(matches!(
            gateway.credential_mode,
            GatewayCredentialMode::GovernorHeld { .. }
        ));
    }

    #[test]
    fn governor_held_without_a_credential_command_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            r#"{
                "gateway": {
                    "bind_addr": "127.0.0.1:18080",
                    "token_path": "/tmp/token",
                    "credential_mode": "governor_held"
                }
            }"#,
        )
        .unwrap();

        let err = load_overrides(dir.path()).unwrap_err();
        assert!(matches!(
            err,
            ConfigFileError::GovernorHeldMissingCredentialCommand
        ));
    }

    #[test]
    fn malformed_json_is_reported_as_a_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CONFIG_FILE_NAME), "{ not json").unwrap();

        let err = load_overrides(dir.path()).unwrap_err();
        assert!(matches!(err, ConfigFileError::Parse { .. }));
    }

    #[test]
    fn a_launcher_section_with_sensible_defaults_resolves() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            r#"{"launcher": {"pools": [{"alias": "agents", "capacity": 2}]}}"#,
        )
        .unwrap();

        let overrides = load_overrides(dir.path()).unwrap();
        let launcher = overrides
            .launcher
            .expect("launcher override must be present");
        assert_eq!(
            launcher.hold_ttl_secs,
            crate::launch_gate::DEFAULT_HOLD_TTL_SECS
        );
        assert_eq!(launcher.pools.len(), 1);
        assert_eq!(launcher.pools[0].alias, "agents");
        assert_eq!(launcher.pools[0].capacity, 2);
        assert!(launcher.pools[0].enabled, "enabled must default to true");
    }

    #[test]
    fn a_launcher_pool_may_be_explicitly_disabled() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            r#"{"launcher": {"pools": [{"alias": "agents", "capacity": 2, "enabled": false}], "hold_ttl_secs": 120}}"#,
        )
        .unwrap();

        let overrides = load_overrides(dir.path()).unwrap();
        let launcher = overrides.launcher.unwrap();
        assert_eq!(launcher.hold_ttl_secs, 120);
        assert!(!launcher.pools[0].enabled);
    }

    #[test]
    fn a_launcher_alias_containing_a_slash_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            r#"{"launcher": {"pools": [{"alias": "a/b", "capacity": 2}]}}"#,
        )
        .unwrap();

        let err = load_overrides(dir.path()).unwrap_err();
        assert!(matches!(
            err,
            ConfigFileError::LaunchAliasContainsSlash { alias } if alias == "a/b"
        ));
    }

    #[test]
    fn an_empty_launcher_alias_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            r#"{"launcher": {"pools": [{"alias": "", "capacity": 2}]}}"#,
        )
        .unwrap();

        let err = load_overrides(dir.path()).unwrap_err();
        assert!(matches!(err, ConfigFileError::LaunchAliasEmpty));
    }

    #[test]
    fn a_duplicate_launcher_alias_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(CONFIG_FILE_NAME),
            r#"{"launcher": {"pools": [
                {"alias": "agents", "capacity": 2},
                {"alias": "agents", "capacity": 5}
            ]}}"#,
        )
        .unwrap();

        let err = load_overrides(dir.path()).unwrap_err();
        assert!(matches!(
            err,
            ConfigFileError::LaunchAliasDuplicate { alias } if alias == "agents"
        ));
    }

    fn write_config(dir: &Path, json: &str) {
        std::fs::write(dir.join(CONFIG_FILE_NAME), json).unwrap();
    }

    #[test]
    fn gateway_token_path_is_the_configured_path_when_one_is_set() {
        let dir = tempfile::tempdir().unwrap();
        write_config(
            dir.path(),
            r#"{"gateway":{"bind_addr":"127.0.0.1:8787","token_path":"/elsewhere/g.token","credential_mode":"pass_through_subscription"}}"#,
        );
        assert_eq!(
            gateway_token_path(dir.path()),
            PathBuf::from("/elsewhere/g.token")
        );
    }

    #[test]
    fn gateway_token_path_defaults_inside_the_state_dir_without_a_gateway_block() {
        let dir = tempfile::tempdir().unwrap();
        let default = dir.path().join("gateway.token");
        assert_eq!(gateway_token_path(dir.path()), default);
        write_config(dir.path(), r#"{"policy":{"preset":"balanced"}}"#);
        assert_eq!(gateway_token_path(dir.path()), default);
        write_config(dir.path(), "{ not json");
        assert_eq!(gateway_token_path(dir.path()), default);
    }

    #[test]
    fn review_flags_a_non_loopback_bind_with_the_gateways_own_reason() {
        let dir = tempfile::tempdir().unwrap();
        write_config(
            dir.path(),
            r#"{"gateway":{"bind_addr":"0.0.0.0:8787","token_path":"/t/g.token","credential_mode":"pass_through_subscription"}}"#,
        );
        let review = review_config(dir.path()).unwrap();
        let reason = review
            .gateway_rejection
            .expect("non-loopback must be rejected");
        assert!(reason.contains("0.0.0.0:8787"), "{reason}");
        assert!(review.unknown_keys.is_empty());
    }

    #[test]
    fn review_accepts_a_loopback_gateway_and_reports_no_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        write_config(
            dir.path(),
            r#"{"gateway":{"bind_addr":"127.0.0.1:8787","token_path":"/t/g.token","credential_mode":"pass_through_subscription"}}"#,
        );
        assert_eq!(review_config(dir.path()).unwrap(), ConfigReview::default());
    }

    #[test]
    fn review_lists_unknown_keys_by_path_and_never_their_values() {
        let dir = tempfile::tempdir().unwrap();
        write_config(
            dir.path(),
            r#"{"bogus_top":"SECRET-VALUE","policy":{"preset":"balanced","nonsense":1},"gateway":{"bind_addr":"127.0.0.1:8787","token_path":"/t/g.token","credential_mode":"pass_through_subscription","session_header":"x","bogus_key":2},"extensions":{"events":{"url":"https://e.example","kinds":[],"secret_command":"c","extra":true}},"launcher":{"pools":[{"alias":"a","capacity":1,"oops":1}]}}"#,
        );
        let review = review_config(dir.path()).unwrap();
        assert_eq!(
            review.unknown_keys,
            vec![
                "bogus_top",
                "extensions.events.extra",
                "gateway.bogus_key",
                "gateway.session_header",
                "launcher.pools[].oops",
                "policy.nonsense",
            ]
        );
        assert!(!format!("{review:?}").contains("SECRET-VALUE"));
    }

    #[test]
    fn review_propagates_a_parse_failure() {
        let dir = tempfile::tempdir().unwrap();
        write_config(dir.path(), "{ not json");
        assert!(review_config(dir.path()).is_err());
    }
}
