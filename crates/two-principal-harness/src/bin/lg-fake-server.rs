//! `lg-fake-server <socket-path> <count-file>` — binds `socket-path`
//! (removing any stale file there first, exactly like production
//! `bind_or_detect_running`'s own stale-socket recovery), then accepts
//! connections in an infinite loop, writing the cumulative total bytes
//! received across all connections to `count-file` after every read.
//! PR-2's design doc §7 "Helper binaries": "binds, accepts in a loop, and
//! records total bytes received".
//!
//! `count-file` is also written once, with `0`, immediately after a
//! successful bind — before the first `accept` — as a ready signal a
//! caller can poll for without needing to land a real connection first.
//! This is what lets `t10c`/`t11a` tell "this attempt's bind landed"
//! apart from "this attempt is still trying" without blocking on the
//! accept loop, which by design never returns on its own.
//!
//! Exit code: `1`, with a diagnostic on stderr, if the bind itself fails
//! — this binary never retries a failed bind. It never exits on its own
//! once bound; a caller controls its lifetime via the OS process (kill).
//! Std-only, no `libc` dependency (the image has no `socat` and Python is
//! not guaranteed — design doc §7).

use std::io::Read;
use std::os::unix::net::UnixListener;

/// Best-effort: a caller polling for the ready signal only cares that
/// the file exists and (eventually) holds the latest total; a lost write
/// here would at worst be observed as "count not yet updated", never as
/// a false bind success. Writes via a temp file plus rename so a
/// concurrent reader never observes a partially written count.
fn write_count(count_file: &str, total: u64) {
    let tmp = format!("{count_file}.tmp");
    if std::fs::write(&tmp, total.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, count_file);
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: lg-fake-server <socket-path> <count-file>");
        std::process::exit(2);
    }
    let socket_path = &args[1];
    let count_file = &args[2];

    // Best-effort, mirroring production `bind_or_detect_running`'s own
    // stale-socket `remove_file`: this only ever succeeds here if this
    // process's own UID already has write access to the containing
    // directory. If it lacks that permission, this fails (ignored) and
    // the `bind` call below then fails too — but not necessarily for the
    // identical reason: when no entry already exists at `socket_path`,
    // `bind` fails with the directory's own permission error
    // (`EACCES`/`EPERM`); when a stale entry is ALREADY there (as after a
    // `kill -9`) and this process cannot remove it, `bind` instead fails
    // with `EADDRINUSE` — the kernel's path lookup finds the existing
    // entry before the parent directory's write permission is even
    // checked. Either way, no new bind ever lands without directory
    // write access; callers that need to distinguish the two should
    // check for an existing entry at `socket_path` themselves rather
    // than relying on this error text.
    let _ = std::fs::remove_file(socket_path);

    let listener = match UnixListener::bind(socket_path) {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("lg-fake-server: bind {socket_path} failed: {e}");
            std::process::exit(1);
        }
    };

    // Ready signal: the bind itself succeeded. Written before the first
    // `accept`, so a caller never needs to land a real connection just to
    // learn whether a bind race was won.
    write_count(count_file, 0);

    let mut total: u64 = 0;
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(stream) => stream,
            Err(_) => continue,
        };
        let mut buf = [0u8; 4096];
        loop {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => total += n as u64,
            }
        }
        write_count(count_file, total);
    }
}
