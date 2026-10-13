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
use libra_governor_daemon::trusted_principal::{
    ConfigError, TrustedPrincipalConfig, PINNED_CONFIG_PATH,
};

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
/// Validates the config file's own identity, then its directory chain,
/// then parses and field-validates it. It does **not** walk the
/// configured socket directory — the server's own startup sequence
/// already does that on its side, and the client's authentication of
/// the server is the peer-identity check in [`connect_verified`], not a
/// filesystem check on a directory the client does not own.
///
/// Order matters here (HORO-1835 security review, fix 3 client-side
/// half): [`TrustedPrincipalConfig::load`] is tried FIRST, not after a
/// separate path-based existence/ownership probe. `load` opens the file
/// exactly once (`O_NOFOLLOW`) and verifies ownership/mode via `fstat`
/// on that SAME already-open handle before reading its contents — an
/// earlier version of this function called `check_path_chain` (a
/// separate, path-based stat of this same leaf file) BEFORE `load`,
/// which reintroduced on the client side exactly the check-then-reopen
/// TOCTOU window `load`'s own atomic open was built to close
/// server-side: an attacker with write access to a directory in `path`
/// could swap the file between the two independent lookups. Trying
/// `load` first means the content this function ultimately trusts is
/// provably the same file whose ownership `load` just verified.
/// `check_path_chain` still runs afterward, for defense-in-depth on
/// every ANCESTOR directory (and, redundantly but harmlessly, the leaf
/// again by path) — by that point `load` has already proven what was
/// read is what was verified, so a later, path-based re-check cannot
/// un-read already-trusted content; it can only refuse to proceed
/// further.
pub(crate) fn pinned_endpoint() -> Result<Option<PinnedEndpoint>, ClientError> {
    pinned_endpoint_at(Path::new(PINNED_CONFIG_PATH))
}

/// The actual resolution logic behind [`pinned_endpoint`], parameterized
/// on the path so unit tests exercise this exact function against a
/// tempdir rather than a hand-kept copy of it — the production entry
/// point always reads the compile-time-pinned `PINNED_CONFIG_PATH`,
/// which a non-root test process cannot write to, so the path is the
/// only thing that needs to vary between the real call and a test.
fn pinned_endpoint_at(path: &Path) -> Result<Option<PinnedEndpoint>, ClientError> {
    let cfg = match TrustedPrincipalConfig::load(path) {
        Ok(cfg) => cfg,
        // The pinned config simply being absent is not an error — default
        // (non-trusted) behavior is unchanged in that case. `load`'s open
        // call itself reports this (`ENOENT` via `rustix::fs::open`),
        // rather than a separate preliminary `symlink_metadata` probe —
        // one path lookup for "does it exist", not two.
        Err(ConfigError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(e) => {
            return Err(ClientError::TrustedConfig(format!(
                "invalid pinned trusted-principal config: {e}"
            )));
        }
    };

    check_path_chain(path, &[0]).map_err(|e| {
        ClientError::TrustedConfig(format!(
            "pinned trusted-principal config path rejected: {e}"
        ))
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
        // test's own uid, not root — `TrustedPrincipalConfig::load`'s own
        // `fstat`-based ownership check (tried first; see this function's
        // doc comment on ordering) must reject it, regardless of mode.
        // This exercises the real `pinned_endpoint_at`, the same function
        // `pinned_endpoint` (the production entry point) calls — not a
        // copy of it.
        let result = pinned_endpoint_at(&path);
        assert!(matches!(result, Err(ClientError::TrustedConfig(_))));
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
