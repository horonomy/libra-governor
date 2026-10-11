//! Trusted-principal mode's client-side plumbing (ADR-0019, PR-2c —
//! HORO-1835). Entirely `#[cfg(feature = "trusted-principal")]`, a
//! non-default Cargo feature: a default build never compiles this file
//! in, and never resolves a pinned trusted-mode endpoint.
//!
//! See PR-2's design doc
//! (`docs/adr/0019-pr2-trusted-principal-mode-plumbing-design.md`) §5
//! for the full rationale. Two entry points:
//!
//! - [`pinned_endpoint`] resolves the compile-time pinned config, if one
//!   is present, into a validated [`PinnedEndpoint`].
//! - [`connect_verified`] connects to it and refuses to hand back the
//!   stream unless the peer's kernel-attested identity matches.
//!
//! Hooked into `crate::client`'s existing connect functions as an early
//! return — see that module. This file never resolves a derived state
//! directory and never starts another process: the socket path and
//! provider identity come only from the pinned, root-owned
//! configuration file, read once per call.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use libra_governor_daemon::trusted_principal::peer::peer_uid;
use libra_governor_daemon::trusted_principal::startup::check_path_chain;
use libra_governor_daemon::trusted_principal::{TrustedPrincipalConfig, PINNED_CONFIG_PATH};

use crate::client::ClientError;

/// A validated trusted-mode connection target: the socket path and the
/// provider uid the client must observe at `connect()` before it will
/// hand the stream back to any caller.
pub(crate) struct PinnedEndpoint {
    socket_path: PathBuf,
    provider_uid: u32,
}

/// Resolves the pinned trusted-principal configuration, if one is
/// present.
///
/// `Ok(None)` iff the pinned config path is simply absent (checked with
/// `symlink_metadata`, not a dereferencing stat) — default (non-trusted)
/// behavior is unchanged in that case. `Err(..)` for any other failure:
/// unreadable, an untrustworthy owner or mode anywhere in its directory
/// chain, unparsable, or invalid fields. Fails closed: once the file
/// exists, this function never returns `Ok(None)` again, so a caller
/// cannot silently fall back to default-mode connect behavior on a
/// validation failure.
///
/// Validates the config file's own identity only: canonicalizes it,
/// then walks its directory chain requiring root ownership all the way
/// up (the same check the server's own startup sequence runs on its
/// pinned paths), then parses and field-validates it. It does **not**
/// walk the configured socket directory — the server's own startup
/// sequence already does that on its side, and the client's
/// authentication of the server is the peer-identity check in
/// [`connect_verified`], not a filesystem check on a directory the
/// client does not own.
pub(crate) fn pinned_endpoint() -> Result<Option<PinnedEndpoint>, ClientError> {
    let path = Path::new(PINNED_CONFIG_PATH);

    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(ClientError::TrustedConfig(format!(
                "could not stat the pinned trusted-principal config: {e}"
            )))
        }
    }

    check_path_chain(path, &[0]).map_err(|e| {
        ClientError::TrustedConfig(format!(
            "pinned trusted-principal config path rejected: {e}"
        ))
    })?;

    let cfg = TrustedPrincipalConfig::load(path).map_err(|e| {
        ClientError::TrustedConfig(format!("invalid pinned trusted-principal config: {e}"))
    })?;

    Ok(Some(PinnedEndpoint {
        socket_path: cfg.socket_path(),
        provider_uid: cfg.provider_uid,
    }))
}

/// Connects to `ep`'s socket, then verifies the peer's kernel-attested
/// uid equals `ep`'s configured provider uid **before** the stream is
/// returned to any caller — so no caller can write a byte to an
/// unverified peer first. Only then are the read/write timeouts set to
/// `io_budget`.
///
/// Never spawns anything: a trusted-mode client either finds the real
/// provider already listening, or it refuses. There is no fallback and
/// no retry in this function; a caller that wants retry semantics (as
/// `ensure_daemon_connection` does in default mode for its own spawn
/// wait) implements that around this call.
pub(crate) fn connect_verified(
    ep: &PinnedEndpoint,
    io_budget: Duration,
) -> Result<UnixStream, ClientError> {
    let stream = UnixStream::connect(&ep.socket_path)
        .map_err(|e| ClientError::DaemonUnavailable(ep.socket_path.clone(), e.to_string()))?;

    let uid = peer_uid(&stream).map_err(|e| {
        ClientError::DaemonUnavailable(
            ep.socket_path.clone(),
            format!("could not read the peer's kernel-attested identity: {e}"),
        )
    })?;
    if uid != ep.provider_uid {
        // Deliberately no uid in this error's Display — see
        // `ClientError::ServerIdentityMismatch`'s own doc comment.
        return Err(ClientError::ServerIdentityMismatch);
    }

    stream.set_read_timeout(Some(io_budget))?;
    stream.set_write_timeout(Some(io_budget))?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn pinned_endpoint_returns_none_when_the_config_is_simply_absent() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.json");
        let result = pinned_endpoint_at(&missing);
        assert!(matches!(result, Ok(None)));
    }

    #[test]
    fn pinned_endpoint_refuses_a_non_root_owned_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trusted-principal.json");
        std::fs::write(
            &path,
            r#"{"provider_uid":10001,"agent_uids":[10002],"provider_id":"operator:lg-provider",
               "state_dir":"/var/lib/libra-governor","socket_dir":"/run/libra-governor",
               "connection_timeout_ms":5000}"#,
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        // In this test process (not uid 0), the file is owned by the
        // test's own uid, not root — `check_path_chain(path, &[0])` must
        // reject it regardless of its mode.
        let result = pinned_endpoint_at(&path);
        assert!(matches!(result, Err(ClientError::TrustedConfig(_))));
    }

    /// Test-only seam: the production entry point always reads
    /// `PINNED_CONFIG_PATH`, which this process cannot write to as a
    /// non-root user. This mirrors [`pinned_endpoint`] exactly, against
    /// an arbitrary path, so the absent/invalid branches are reachable
    /// without root.
    fn pinned_endpoint_at(path: &Path) -> Result<Option<PinnedEndpoint>, ClientError> {
        match std::fs::symlink_metadata(path) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(ClientError::TrustedConfig(format!(
                    "could not stat the pinned trusted-principal config: {e}"
                )))
            }
        }
        check_path_chain(path, &[0]).map_err(|e| {
            ClientError::TrustedConfig(format!(
                "pinned trusted-principal config path rejected: {e}"
            ))
        })?;
        let cfg = TrustedPrincipalConfig::load(path).map_err(|e| {
            ClientError::TrustedConfig(format!("invalid pinned trusted-principal config: {e}"))
        })?;
        Ok(Some(PinnedEndpoint {
            socket_path: cfg.socket_path(),
            provider_uid: cfg.provider_uid,
        }))
    }

    #[test]
    fn connect_verified_succeeds_against_a_listener_whose_peer_uid_matches() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("trusted.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();

        let own_uid = rustix::process::getuid().as_raw();
        let ep = PinnedEndpoint {
            socket_path: socket_path.clone(),
            provider_uid: own_uid,
        };

        let accept_thread = std::thread::spawn(move || {
            let _ = listener.accept();
        });

        let result = connect_verified(&ep, Duration::from_secs(1));
        assert!(result.is_ok(), "expected success, got {result:?}");
        accept_thread.join().unwrap();
    }

    #[test]
    fn connect_verified_refuses_a_listener_whose_peer_uid_does_not_match() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("trusted.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();

        let own_uid = rustix::process::getuid().as_raw();
        let ep = PinnedEndpoint {
            socket_path: socket_path.clone(),
            // One off from this test process's own uid: never matches.
            provider_uid: own_uid + 1,
        };

        let accept_thread = std::thread::spawn(move || {
            let _ = listener.accept();
        });

        let result = connect_verified(&ep, Duration::from_secs(1));
        assert!(matches!(result, Err(ClientError::ServerIdentityMismatch)));
        accept_thread.join().unwrap();
    }
}
