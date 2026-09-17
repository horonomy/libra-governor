//! Client-side socket handling shared by the `hook` and `statusline`
//! subcommands: connecting to the daemon, optionally spawning it if
//! absent, and the request/response round trip.

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use libra_governor_protocol::{
    wire, Request, RequestEnvelope, Response, ResponseEnvelope, PROTOCOL_VERSION,
};

/// How long a single request/response round trip may block once
/// connected. `std::os::unix::net::UnixStream::connect` has no
/// connect-timeout knob (unlike `TcpStream`), but a local Unix domain
/// socket connect either resolves near-instantly or fails immediately
/// with `ENOENT`/`ECONNREFUSED` — there is no network round trip to time
/// out, so only the read/write side needs an explicit budget.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Total time budget for spawning a fresh daemon and waiting for it to
/// start accepting connections, before the hook gives up and degrades
/// gracefully. Comfortably inside Claude Code's own hook timeout.
const SPAWN_WAIT_BUDGET: Duration = Duration::from_secs(3);
const SPAWN_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("daemon unavailable at {0}: {1}")]
    DaemonUnavailable(PathBuf, String),
    #[error("io error talking to daemon: {0}")]
    Io(#[from] std::io::Error),
    #[error("wire error talking to daemon: {0}")]
    Wire(#[from] wire::WireError),
    #[error("protocol version mismatch: client speaks {0}, daemon responded with {1}")]
    ProtocolMismatch(u32, u32),
    #[error("daemon returned an error: {0}")]
    DaemonError(String),
}

fn connect_once(socket_path: &Path) -> std::io::Result<UnixStream> {
    let stream = UnixStream::connect(socket_path)?;
    stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
    stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
    Ok(stream)
}

/// Connects to an already-running daemon. Never spawns one — used by the
/// statusline subcommand, which must not become a daemon-spawning race
/// factory on every refresh tick.
pub fn connect_only(socket_path: &Path) -> Result<UnixStream, ClientError> {
    connect_once(socket_path)
        .map_err(|e| ClientError::DaemonUnavailable(socket_path.to_path_buf(), e.to_string()))
}

/// Connects to the daemon, spawning it (detached, `daemon run`) if a
/// connection attempt fails, then polling for readiness up to
/// [`SPAWN_WAIT_BUDGET`]. Used by the `hook` subcommand only.
pub fn ensure_daemon_connection(socket_path: &Path) -> Result<UnixStream, ClientError> {
    if let Ok(stream) = connect_once(socket_path) {
        return Ok(stream);
    }

    spawn_daemon_detached().map_err(|e| {
        ClientError::DaemonUnavailable(socket_path.to_path_buf(), format!("spawn failed: {e}"))
    })?;

    let deadline = Instant::now() + SPAWN_WAIT_BUDGET;
    loop {
        if let Ok(stream) = connect_once(socket_path) {
            return Ok(stream);
        }
        if Instant::now() >= deadline {
            return Err(ClientError::DaemonUnavailable(
                socket_path.to_path_buf(),
                "daemon did not become ready within the spawn wait budget".to_string(),
            ));
        }
        std::thread::sleep(SPAWN_POLL_INTERVAL);
    }
}

fn spawn_daemon_detached() -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    Command::new(exe)
        .args(["daemon", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

/// Sends `request` over `stream` and returns the daemon's [`Response`],
/// after validating the protocol version on the reply.
pub fn roundtrip(stream: &UnixStream, request: Request) -> Result<Response, ClientError> {
    let envelope = RequestEnvelope {
        protocol_version: PROTOCOL_VERSION,
        request,
    };
    wire::write_message(stream, &envelope)?;

    let response: ResponseEnvelope = wire::read_message(BufReader::new(stream.try_clone()?))?;
    if response.protocol_version != PROTOCOL_VERSION {
        return Err(ClientError::ProtocolMismatch(
            PROTOCOL_VERSION,
            response.protocol_version,
        ));
    }
    if let Response::Error { message } = &response.response {
        return Err(ClientError::DaemonError(message.clone()));
    }
    Ok(response.response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_only_fails_fast_when_nothing_is_listening() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("nothing-here.sock");
        let start = Instant::now();
        let result = connect_only(&socket_path);
        assert!(result.is_err());
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "connect_only must fail fast, not hang"
        );
    }
}
