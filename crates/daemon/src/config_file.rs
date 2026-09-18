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

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use libra_governor_domain::{
    CompletionContract, CompletionCriterion, Policy, PolicyPresetInputs, PolicyValidationError,
    ResourceAmount,
};
use libra_governor_gateway::config::{GatewayConfig, GatewayCredentialMode};
use libra_governor_gateway::credential::CredentialCommand;
use serde::Deserialize;

/// The file name resolved under the daemon's state directory (see
/// `crates/daemon/src/paths.rs`). Not itself TOML — see module docs.
pub const CONFIG_FILE_NAME: &str = "config.json";

/// The four preset names [`Policy`] exposes, exactly as spelled in
/// `crates/domain/src/policy.rs`'s doc comments and `Policy::name`.
const KNOWN_PRESETS: [&str; 4] = ["balanced", "deadline_first", "cost_first", "strict_budget"];

/// Same resource/time targets `crates/daemon::default_admission_policy`
/// hardcodes, reused here as this config surface's own defaults so a
/// `[policy]` table that names only `preset` still produces a sensible
/// policy.
const DEFAULT_RESOURCE_TARGET_TOKENS: u64 = 100_000;
const DEFAULT_TIME_TARGET_SECS: u64 = 3600;

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
        KNOWN_PRESETS.join(", ")
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
}

#[derive(Debug, Deserialize, Default)]
struct RawConfigFile {
    policy: Option<RawPolicyConfig>,
    gateway: Option<RawGatewayConfig>,
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

/// Reads and parses `<state_dir>/config.json`, if it exists, into the
/// `(Policy, GatewayConfig)` overrides `daemon_cmd::run` should use in
/// place of its own hardcoded defaults. `Ok((None, None))` when the file
/// is simply absent — the normal case for every existing deployment.
pub fn load_overrides(
    state_dir: &Path,
) -> Result<(Option<Policy>, Option<GatewayConfig>), ConfigFileError> {
    let path = state_dir.join(CONFIG_FILE_NAME);
    if !path.exists() {
        return Ok((None, None));
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
    Ok((policy, gateway))
}

fn resolve_policy(raw: RawPolicyConfig) -> Result<Policy, ConfigFileError> {
    let inputs = PolicyPresetInputs {
        resource_target: ResourceAmount::Tokens(raw.resource_target_tokens),
        time_target_secs: raw.time_target_secs,
        quality_floor: CompletionContract::first(vec![CompletionCriterion::required(
            "required verification (tests/build/lint) passes",
        )]),
    };
    let build = |source: Result<Policy, PolicyValidationError>| {
        source.map_err(|source| ConfigFileError::InvalidPolicy {
            preset: raw.preset.clone(),
            source,
        })
    };
    match raw.preset.as_str() {
        "balanced" => build(Policy::balanced(inputs)),
        "deadline_first" => build(Policy::deadline_first(inputs)),
        "cost_first" => build(Policy::cost_first(inputs)),
        "strict_budget" => build(Policy::strict_budget(inputs)),
        other => Err(ConfigFileError::UnknownPreset {
            preset: other.to_string(),
        }),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_config_file_produces_no_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let (policy, gateway) = load_overrides(dir.path()).unwrap();
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

        let (policy, gateway) = load_overrides(dir.path()).unwrap();
        let policy = policy.expect("policy override must be present");
        assert_eq!(policy.name, "deadline_first");
        assert!(gateway.is_none());
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

        let (policy, gateway) = load_overrides(dir.path()).unwrap();
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
}
