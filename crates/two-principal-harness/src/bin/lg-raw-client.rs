//! `lg-raw-client <socket> <json-line>` — sends one raw newline-delimited
//! JSON request line to a Unix socket and prints the one-line reply to
//! stdout (ADR-0019 §5/§7, PR-2's design doc §7).
//!
//! Exists because no CLI client knows the pinned trusted-principal
//! endpoint before PR-2c wires `trusted_client.rs` — until then, the only
//! way this harness can drive the trusted socket end to end is a raw,
//! protocol-naive client, exactly as an external Outcome Provider would
//! be before that wiring exists. Std-only, no `libc` dependency: the
//! image has no `socat` and Python is not guaranteed (design doc §7).
//!
//! Exit code: `0` and the reply printed to stdout on a successful
//! round trip; non-zero (with a diagnostic on stderr) on a connect,
//! write, or read failure. This binary never retries and never spawns
//! anything — a caller that wants retry-on-connect-refused semantics
//! implements that in the test itself.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: lg-raw-client <socket-path> <json-line>");
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
