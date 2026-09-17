//! The "daemon unreachable" failure path: connecting to a socket path
//! with nothing listening must fail fast, not hang — the property the
//! `hook` and `statusline` CLI subcommands rely on to degrade
//! gracefully within their own timeout budgets (see
//! `crates/cli/src/client.rs`).

use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

#[test]
fn connecting_to_a_socket_path_with_nothing_listening_fails_fast() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("nobody-home.sock");

    let start = Instant::now();
    let result = UnixStream::connect(&socket_path);
    let elapsed = start.elapsed();

    assert!(
        result.is_err(),
        "connecting to a nonexistent socket must fail"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "a local Unix socket connect to a nonexistent path must fail near-instantly \
         (ENOENT), not hang: took {elapsed:?}"
    );
}

#[test]
fn connecting_to_a_stale_socket_file_with_no_listener_fails_fast() {
    // A daemon that bound the socket and then crashed leaves the socket
    // file behind (see libra_governor_daemon::bind_or_detect_running's
    // docs on stale-socket detection) — connecting to that stale file
    // must also fail fast, not hang, even though the path exists.
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("stale.sock");
    {
        let _listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        // Dropped without accepting: simulates a crashed daemon. The
        // socket file remains on disk (Unix does not auto-unlink it).
    }
    assert!(socket_path.exists(), "test setup: stale file must remain");

    let start = Instant::now();
    let result = UnixStream::connect(&socket_path);
    let elapsed = start.elapsed();

    assert!(result.is_err(), "connecting to a stale socket must fail");
    assert!(
        elapsed < Duration::from_secs(1),
        "connecting to a stale socket (ECONNREFUSED) must fail near-instantly, not hang: \
         took {elapsed:?}"
    );
}
