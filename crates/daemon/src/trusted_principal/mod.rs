//! Trusted-principal mode plumbing (ADR-0019, PR-2a — HORO-1835). Entirely
//! `#[cfg(feature = "trusted-principal")]`, a non-default Cargo feature:
//! the release binary and every default build cannot reach any of this.
//!
//! This module tree adds no authority. It wires a pinned configuration,
//! kernel-attested peer classification, a hardened accept loop, and a
//! confused-deputy sanitizer on top of the existing protocol and daemon —
//! see PR-2's design doc (`docs/adr/0019-pr2-trusted-principal-mode-plumbing-design.md`)
//! for the full rationale. `AttestOutcome` and any authoritative
//! attestation construction remain out of scope for this PR (PR-3).
//!
//! This module never resolves an env-derived state directory and never
//! spawns a subprocess — enforced both by code review and by the static
//! scan in `tests/trusted_principal_default_off_guard.rs`.

pub mod peer;
pub mod serve;
pub mod startup;

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Compile-time pinned location the *client* reads (PR-2c). The daemon
/// takes its path from argv instead (§5 of the design doc explains why
/// the two differ: hooks cannot carry a flag, but a service manager's
/// argv is explicit and auditable).
pub const PINNED_CONFIG_PATH: &str = "/etc/libra-governor/trusted-principal.json";
pub const SOCKET_FILE_NAME: &str = "daemon.sock";
/// A memory bound on one request's wire bytes, not a traffic shaper —
/// see [`serve::serve_trusted`] docs.
pub const MAX_REQUEST_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustedPrincipalConfig {
    pub provider_uid: u32,
    pub agent_uids: Vec<u32>,
    /// Stored as the provider source_id by PR-3's AttestOutcome.
    /// Validated now so a later PR cannot discover the validation gap
    /// only after real attestations already depend on it.
    pub provider_id: String,
    /// e.g. `/var/lib/libra-governor`.
    pub state_dir: PathBuf,
    /// e.g. `/run/libra-governor`; socket = `socket_dir/daemon.sock`.
    pub socket_dir: PathBuf,
    pub connection_timeout_ms: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("io error reading trusted-principal config: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not parse trusted-principal config: {0}")]
    Parse(#[from] serde_json::Error),
    #[error(
        "trusted mode refuses to start: the config file must be owned by uid 0 and must \
         not be group- or other-writable"
    )]
    FileOwnershipOrMode,
    #[error("provider_uid must not be 0")]
    ProviderUidIsRoot,
    #[error("agent_uids must be non-empty and must not contain 0 or provider_uid")]
    AgentUidsInvalid,
    #[error(
        "provider_id must be non-empty, at most 128 bytes, and must not start with \"unverified:\""
    )]
    ProviderIdInvalid,
    #[error("state_dir and socket_dir must each be an absolute, lexically normal path")]
    PathNotAbsoluteOrNormal,
    #[error("state_dir and socket_dir must not be equal or nested inside one another")]
    PathsOverlap,
    #[error("connection_timeout_ms must be in 100..=60000")]
    ConnectionTimeoutOutOfRange,
}

impl TrustedPrincipalConfig {
    /// Opens `path` exactly once — with `O_NOFOLLOW`, refusing a symlink
    /// at the final path component — verifies ownership (uid 0) and mode
    /// (not group- or other-writable) via `fstat` on that SAME already-
    /// open handle, then reads and parses its contents from that same
    /// handle, and finally runs [`Self::validate_fields`].
    ///
    /// The ownership/mode check and the read are tied to one `fstat`+
    /// `read` pair against one open file descriptor so they can never
    /// observe two different underlying files: a path-based "verify by
    /// stat(path), then separately re-open(path) and read" sequence
    /// leaves a TOCTOU window in which an attacker with write access to
    /// a directory in `path` can swap the file between the check and
    /// the read (PR-2a/PR-2c independent security review, HORO-1835).
    /// [`startup::validate_trusted_startup`]'s own `check_path_chain`
    /// still separately verifies every ANCESTOR directory in `path`; the
    /// leaf file's own ownership/mode is verified here and only here.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let fd = rustix::fs::open(
            path,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(std::io::Error::from)?;
        let mut file = std::fs::File::from(fd);

        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err(ConfigError::FileOwnershipOrMode);
        }

        let mut contents = String::new();
        std::io::Read::read_to_string(&mut file, &mut contents)?;

        let config: Self = serde_json::from_str(&contents)?;
        config.validate_fields()?;
        Ok(config)
    }

    fn validate_fields(&self) -> Result<(), ConfigError> {
        if self.provider_uid == 0 {
            return Err(ConfigError::ProviderUidIsRoot);
        }
        if self.agent_uids.is_empty()
            || self.agent_uids.contains(&0)
            || self.agent_uids.contains(&self.provider_uid)
        {
            return Err(ConfigError::AgentUidsInvalid);
        }
        if self.provider_id.is_empty()
            || self.provider_id.len() > 128
            || self.provider_id.starts_with("unverified:")
        {
            return Err(ConfigError::ProviderIdInvalid);
        }
        for path in [&self.state_dir, &self.socket_dir] {
            if !path.is_absolute() || path.components().any(|c| !is_normal_component(&c)) {
                return Err(ConfigError::PathNotAbsoluteOrNormal);
            }
        }
        if self.state_dir == self.socket_dir
            || self.state_dir.starts_with(&self.socket_dir)
            || self.socket_dir.starts_with(&self.state_dir)
        {
            return Err(ConfigError::PathsOverlap);
        }
        if !(100..=60_000).contains(&self.connection_timeout_ms) {
            return Err(ConfigError::ConnectionTimeoutOutOfRange);
        }
        Ok(())
    }

    pub fn socket_path(&self) -> PathBuf {
        self.socket_dir.join(SOCKET_FILE_NAME)
    }

    pub fn ledger_path(&self) -> PathBuf {
        self.state_dir.join("ledger.sqlite3")
    }

    pub fn log_path(&self) -> PathBuf {
        self.state_dir.join("daemon.log")
    }

    pub fn connection_timeout(&self) -> Duration {
        Duration::from_millis(self.connection_timeout_ms)
    }
}

fn is_normal_component(component: &std::path::Component<'_>) -> bool {
    matches!(
        component,
        std::path::Component::RootDir | std::path::Component::Normal(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid() -> TrustedPrincipalConfig {
        TrustedPrincipalConfig {
            provider_uid: 10001,
            agent_uids: vec![10002],
            provider_id: "operator:lg-provider".to_string(),
            state_dir: PathBuf::from("/var/lib/libra-governor"),
            socket_dir: PathBuf::from("/run/libra-governor"),
            connection_timeout_ms: 5000,
        }
    }

    #[test]
    fn a_valid_config_passes() {
        assert!(valid().validate_fields().is_ok());
    }

    #[test]
    fn provider_uid_zero_is_rejected() {
        let mut cfg = valid();
        cfg.provider_uid = 0;
        assert!(matches!(
            cfg.validate_fields(),
            Err(ConfigError::ProviderUidIsRoot)
        ));
    }

    #[test]
    fn empty_agent_uids_is_rejected() {
        let mut cfg = valid();
        cfg.agent_uids = vec![];
        assert!(matches!(
            cfg.validate_fields(),
            Err(ConfigError::AgentUidsInvalid)
        ));
    }

    #[test]
    fn agent_uid_zero_is_rejected() {
        let mut cfg = valid();
        cfg.agent_uids = vec![0];
        assert!(matches!(
            cfg.validate_fields(),
            Err(ConfigError::AgentUidsInvalid)
        ));
    }

    #[test]
    fn agent_uid_equal_to_provider_uid_is_rejected() {
        let mut cfg = valid();
        cfg.agent_uids = vec![cfg.provider_uid];
        assert!(matches!(
            cfg.validate_fields(),
            Err(ConfigError::AgentUidsInvalid)
        ));
    }

    #[test]
    fn provider_id_starting_with_unverified_prefix_is_rejected() {
        let mut cfg = valid();
        cfg.provider_id = "unverified:agent".to_string();
        assert!(matches!(
            cfg.validate_fields(),
            Err(ConfigError::ProviderIdInvalid)
        ));
    }

    #[test]
    fn empty_provider_id_is_rejected() {
        let mut cfg = valid();
        cfg.provider_id = String::new();
        assert!(matches!(
            cfg.validate_fields(),
            Err(ConfigError::ProviderIdInvalid)
        ));
    }

    #[test]
    fn overlong_provider_id_is_rejected() {
        let mut cfg = valid();
        cfg.provider_id = "x".repeat(129);
        assert!(matches!(
            cfg.validate_fields(),
            Err(ConfigError::ProviderIdInvalid)
        ));
    }

    #[test]
    fn relative_state_dir_is_rejected() {
        let mut cfg = valid();
        cfg.state_dir = PathBuf::from("relative/path");
        assert!(matches!(
            cfg.validate_fields(),
            Err(ConfigError::PathNotAbsoluteOrNormal)
        ));
    }

    #[test]
    fn dotdot_component_is_rejected() {
        let mut cfg = valid();
        cfg.state_dir = PathBuf::from("/var/lib/../lib/libra-governor");
        assert!(matches!(
            cfg.validate_fields(),
            Err(ConfigError::PathNotAbsoluteOrNormal)
        ));
    }

    #[test]
    fn equal_state_and_socket_dirs_are_rejected() {
        let mut cfg = valid();
        cfg.socket_dir = cfg.state_dir.clone();
        assert!(matches!(
            cfg.validate_fields(),
            Err(ConfigError::PathsOverlap)
        ));
    }

    #[test]
    fn nested_state_dir_inside_socket_dir_is_rejected() {
        let mut cfg = valid();
        cfg.state_dir = cfg.socket_dir.join("nested");
        assert!(matches!(
            cfg.validate_fields(),
            Err(ConfigError::PathsOverlap)
        ));
    }

    #[test]
    fn zero_timeout_is_rejected() {
        let mut cfg = valid();
        cfg.connection_timeout_ms = 0;
        assert!(matches!(
            cfg.validate_fields(),
            Err(ConfigError::ConnectionTimeoutOutOfRange)
        ));
    }

    #[test]
    fn overlong_timeout_is_rejected() {
        let mut cfg = valid();
        cfg.connection_timeout_ms = 60_001;
        assert!(matches!(
            cfg.validate_fields(),
            Err(ConfigError::ConnectionTimeoutOutOfRange)
        ));
    }

    #[test]
    fn deny_unknown_fields_rejects_an_unrecognized_key() {
        let json = r#"{"provider_uid":10001,"agent_uids":[10002],
            "provider_id":"operator:lg-provider","state_dir":"/var/lib/libra-governor",
            "socket_dir":"/run/libra-governor","connection_timeout_ms":5000,
            "secret_command":"/bin/sh"}"#;
        let result: Result<TrustedPrincipalConfig, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn socket_ledger_and_log_paths_are_derived_from_state_and_socket_dirs() {
        let cfg = valid();
        assert_eq!(
            cfg.socket_path(),
            PathBuf::from("/run/libra-governor/daemon.sock")
        );
        assert_eq!(
            cfg.ledger_path(),
            PathBuf::from("/var/lib/libra-governor/ledger.sqlite3")
        );
        assert_eq!(
            cfg.log_path(),
            PathBuf::from("/var/lib/libra-governor/daemon.log")
        );
    }

    #[test]
    fn connection_timeout_converts_millis_to_a_duration() {
        let cfg = valid();
        assert_eq!(cfg.connection_timeout(), Duration::from_millis(5000));
    }

    // `load`'s own ownership/mode/symlink checks (HORO-1835 security
    // review, fix 3): these run as whatever uid the test process has —
    // never root in CI — so they can only exercise the "not owned by
    // uid 0" and "final component is a symlink" rejections; a root-owned
    // happy path is covered instead by the two-principal-harness's
    // disposable container (`x_startup_checks.rs`), which really does
    // run as root there.

    #[test]
    fn load_rejects_a_config_file_not_owned_by_uid_zero() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trusted-principal.json");
        std::fs::write(
            &path,
            r#"{"provider_uid":10001,"agent_uids":[10002],
               "provider_id":"operator:lg-provider","state_dir":"/var/lib/libra-governor",
               "socket_dir":"/run/libra-governor","connection_timeout_ms":5000}"#,
        )
        .unwrap();

        // The test process is never uid 0 in CI, so this file is already
        // owned by the "wrong" uid — exactly the condition `load` must
        // refuse via the `fstat` it performs on its own already-open
        // handle, not a separate path-based stat.
        assert!(matches!(
            TrustedPrincipalConfig::load(&path),
            Err(ConfigError::FileOwnershipOrMode)
        ));
    }

    #[test]
    fn load_rejects_a_symlink_at_the_final_path_component() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real-trusted-principal.json");
        std::fs::write(&real, "{}").unwrap();
        let link = dir.path().join("trusted-principal.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        // `load` opens with `O_NOFOLLOW`: a symlinked final component
        // must be refused outright (an `Io` error from the open itself),
        // never silently followed and then separately checked — that
        // "check the resolved target, then re-open by the original
        // path" shape is exactly the TOCTOU this fix closes.
        assert!(matches!(
            TrustedPrincipalConfig::load(&link),
            Err(ConfigError::Io(_))
        ));
    }
}
