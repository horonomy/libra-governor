//! `lg-raw-client <socket> <json-line> [--print-peer-uid]` — sends one
//! raw newline-delimited JSON request line to a Unix socket and prints
//! the one-line reply to stdout (ADR-0019 §5/§7, PR-2's design doc §7).
//!
//! Exists because no CLI client knows the pinned trusted-principal
//! endpoint before PR-2c wires `trusted_client.rs` — until then, the only
//! way this harness can drive the trusted socket end to end is a raw,
//! protocol-naive client, exactly as an external Outcome Provider would
//! be before that wiring exists. Originally std-only; the image has no
//! `socat` and Python is not guaranteed (design doc §7), which is still
//! why this binary exists at all.
//!
//! With `--print-peer-uid` (PR-2b addition, design doc §7's helper
//! binary row), this binary ALSO prints a `peer_uid=<uid>` line to
//! stdout immediately after `connect()` succeeds, using the same
//! kernel-attested `SO_PEERCRED`/`getpeereid` mechanism the production
//! trusted-principal client uses (design doc §2/§5) — classifying who is
//! actually listening at the other end, independent of whether that peer
//! ever sends a reply at all. This is what lets `t11a` tell apart "a
//! connect reached the real provider" from "a connect reached the
//! agent's own `lg-fake-server`" even though `lg-fake-server` never
//! speaks this wire protocol and never writes a reply: a short read/
//! write timeout is set right after the peer-uid print, so a
//! non-replying peer times this binary out instead of hanging it
//! forever. The `peer_uid=` line, once printed, is already flushed to
//! stdout and valid regardless of what happens to the exit code after.
//!
//! Exit code: `0` and the reply printed to stdout on a successful round
//! trip; non-zero (with a diagnostic on stderr) on a connect, write, or
//! read failure (including a `--print-peer-uid` read timeout against a
//! non-replying peer). This binary never retries and never spawns
//! anything — a caller that wants retry-on-connect-refused semantics
//! implements that in the test itself.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let print_peer_uid = args.get(3).map(String::as_str) == Some("--print-peer-uid");
    if args.len() < 3 || args.len() > 4 || (args.len() == 4 && !print_peer_uid) {
        eprintln!("usage: lg-raw-client <socket-path> <json-line> [--print-peer-uid]");
        std::process::exit(2);
    }
    let socket_path = &args[1];
    let json_line = &args[2];

    let mut stream = match UnixStream::connect(socket_path) {
        Ok(stream) => stream,
        Err(e) => {
            eprintln!("lg-raw-client: connect to {socket_path} failed: {e}");
            std::process::exit(1);
        }
    };

    if print_peer_uid {
        match peer_uid(&stream) {
            Ok(uid) => {
                println!("peer_uid={uid}");
                let _ = std::io::stdout().flush();
            }
            Err(e) => {
                eprintln!("lg-raw-client: peer_uid failed: {e}");
                std::process::exit(1);
            }
        }
        // A short deadline, not `None`: the peer may be a non-protocol-
        // speaking stand-in (e.g. `lg-fake-server`) that never writes a
        // reply, and this binary must still return promptly either way.
        let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
        let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
    }

    if let Err(e) = writeln!(stream, "{json_line}") {
        eprintln!("lg-raw-client: write failed: {e}");
        std::process::exit(1);
    }

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => {
            eprintln!("lg-raw-client: connection closed before a reply was received");
            std::process::exit(1);
        }
        Ok(_) => {
            print!("{line}");
        }
        Err(e) => {
            eprintln!("lg-raw-client: read failed: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn peer_uid(stream: &UnixStream) -> std::io::Result<u32> {
    use std::os::unix::io::AsRawFd;

    let fd = stream.as_raw_fd();
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `fd` is a valid, open socket fd owned by `stream` for the
    // duration of this call; `cred`/`len` are valid, correctly sized
    // out-parameters for `SO_PEERCRED` per `unix(7)`.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if len != std::mem::size_of::<libc::ucred>() as libc::socklen_t {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "SO_PEERCRED returned an unexpected credential size",
        ));
    }
    Ok(cred.uid)
}

#[cfg(target_vendor = "apple")]
fn peer_uid(stream: &UnixStream) -> std::io::Result<u32> {
    use std::os::unix::io::AsRawFd;

    let fd = stream.as_raw_fd();
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: `fd` is a valid, open socket fd owned by `stream` for the
    // duration of this call; `uid`/`gid` are valid out-parameters per
    // `getpeereid(3)`.
    let rc = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(uid)
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
fn peer_uid(_stream: &UnixStream) -> std::io::Result<u32> {
    // Compiles everywhere and fails closed: an unsupported platform
    // cannot classify a peer.
    Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
}
