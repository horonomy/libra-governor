//! Minimal JSON-RPC-over-stdio client for Codex's `app-server`
//! (HORO-1906, ADR-0015 Option A).
//!
//! This is a *generic* transport: it spawns an arbitrary program/argv,
//! speaks newline-delimited JSON-RPC over its stdin/stdout, and bounds
//! every call by a caller-supplied deadline. It knows nothing about
//! hooks, trust, or Codex's specific method names — that correlation
//! lives in `codex_verify_trust_cmd`, which is also the only caller of
//! [`AppServerClient::spawn`] in this crate (see that module's own docs
//! for why — ADR-0015 requirement 1/2: no implicit host launch, explicit
//! operator action only).
//!
//! # Fail-closed by construction
//!
//! Every error variant here is handed back to the caller as an
//! `Err(AppServerError)`; there is no code path that can produce a
//! `"trusted"`-shaped success out of a timeout, a malformed line, or an
//! unreachable process. The caller (`codex_verify_trust_cmd`) maps every
//! `Err` to an untrusted/unknown report — never to trusted — but that
//! mapping is enforced by review of the one small call site, not by
//! anything this module can itself guarantee at the type level.
//!
//! # Why a reader thread, not a blocking `read_line`
//!
//! `BufReader::read_line` has no timeout. A wedged or hostile
//! `app-server` that writes a partial line and then hangs would block
//! this process forever. Mirrors the bounded-read pattern
//! `libra_governor_quota_observation::runner` already uses for its
//! one-shot subprocess transport (deadline + background reader thread +
//! `recv_timeout`), simplified for a persistent (not one-shot)
//! connection: one reader thread lives for the client's whole lifetime
//! and forwards each decoded line to the caller's bounded `call()` loop
//! over a channel.
//!
//! # Process cleanup
//!
//! The spawned child is killed (best-effort, never panics) when the
//! client is dropped — a verify-trust command must never leave an
//! orphaned `app-server` running after it exits, success or failure.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Caps how many bytes a single `read_line` call will consume before a
/// `\n` is found — see the "Why a reader thread" module docs and
/// `AppServerClient::spawn`'s reader-thread loop. Generous for any real
/// JSON-RPC frame this protocol actually sends (Codex's own `hooks/list`
/// response for a handful of hooks is a few KB at most), but finite, so
/// a hostile or badly-behaved process that writes bytes with no
/// terminator cannot make this thread buffer without bound.
const MAX_LINE_BYTES: u64 = 8 * 1024 * 1024;

/// One line of app-server stdout, as seen by the reader thread.
enum ReaderEvent {
    Line(String),
    /// stdout closed (process exited or closed its write end).
    Eof,
    /// The underlying read itself failed (rare — a real I/O error, not a
    /// parse error on the line's content).
    Io(String),
}

#[derive(Debug, thiserror::Error)]
pub enum AppServerError {
    #[error("failed to spawn {0:?}: {1}")]
    Spawn(String, String),
    #[error("deadline of {0:?} exceeded waiting for a response to {1:?}")]
    Timeout(Duration, &'static str),
    #[error("app-server closed its output before responding to {0:?}")]
    UnexpectedEof(&'static str),
    #[error("malformed/truncated JSON-RPC line while waiting for {0:?}: {1}")]
    MalformedLine(&'static str, String),
    #[error("app-server returned a JSON-RPC error for {0:?}: {1}")]
    RpcError(&'static str, String),
    #[error("i/o error talking to app-server: {0}")]
    Io(String),
}

/// Wall-clock cost of each phase of one client session — required
/// evidence per ADR-0015's "Not decided here" section (app-server
/// startup latency was explicitly left unmeasured by the ADR itself).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AppServerLatency {
    pub spawn: Duration,
    pub initialize: Duration,
    pub hooks_list: Duration,
}

impl AppServerLatency {
    pub fn total(&self) -> Duration {
        self.spawn + self.initialize + self.hooks_list
    }
}

#[derive(Debug)]
pub struct AppServerClient {
    child: Child,
    stdin: std::process::ChildStdin,
    rx: mpsc::Receiver<ReaderEvent>,
    next_id: i64,
    killed: bool,
}

impl AppServerClient {
    /// Spawns `program` with `args`, wiring a background reader thread
    /// over its stdout. `extra_env` is applied on top of this process's
    /// own environment (e.g. `CODEX_HOME`) — never cleared, unlike
    /// `quota_observation::runner`'s one-shot transport: `app-server`
    /// needs its ordinary environment (`PATH`, `HOME`, etc.) to find and
    /// use the operator's real Codex installation, which is the whole
    /// point of this ticket (ADR-0015 requirement 3).
    pub fn spawn(
        program: &str,
        args: &[&str],
        extra_env: &[(&str, &std::ffi::OsStr)],
    ) -> Result<(Self, Duration), AppServerError> {
        let start = Instant::now();
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (key, value) in extra_env {
            command.env(key, value);
        }
        let mut child = command
            .spawn()
            .map_err(|e| AppServerError::Spawn(program.to_owned(), e.to_string()))?;
        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");
        let spawn_elapsed = start.elapsed();

        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                // Bounded per line (security review, HORO-1906): a
                // hostile or badly-behaved app-server that writes bytes
                // with no `\n` would otherwise make `read_line` grow
                // `line` without bound, independent of the caller's
                // `call()` deadline (which only bounds how long the
                // *channel recv* waits, not how much this thread buffers
                // before ever posting anything). `Take` caps how many
                // bytes this one `read_line` call will consume; a fresh
                // `Take` is created each iteration so the cap applies
                // per line, not cumulatively across the connection's
                // whole lifetime.
                let mut limited = (&mut reader).take(MAX_LINE_BYTES);
                match limited.read_line(&mut line) {
                    Ok(0) => {
                        let _ = tx.send(ReaderEvent::Eof);
                        break;
                    }
                    Ok(_) if !line.ends_with('\n') && line.len() as u64 >= MAX_LINE_BYTES => {
                        let _ = tx.send(ReaderEvent::Io(format!(
                            "a single line exceeded the {MAX_LINE_BYTES}-byte cap without a \
                             newline"
                        )));
                        break;
                    }
                    Ok(_) => {
                        if tx.send(ReaderEvent::Line(line)).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(ReaderEvent::Io(e.to_string()));
                        break;
                    }
                }
            }
        });

        Ok((
            Self {
                child,
                stdin,
                rx,
                next_id: 1,
                killed: false,
            },
            spawn_elapsed,
        ))
    }

    /// Sends one JSON-RPC request and waits (bounded by `deadline`) for
    /// the response whose `id` matches, skipping any notifications (no
    /// `id`) or stray responses to a different id that arrive first —
    /// Codex's app-server is known to emit asynchronous notifications
    /// (e.g. `remoteControl/status/changed`) interleaved with request
    /// responses (observed empirically against a real local Codex
    /// install during this ticket's own evidence-gathering, both
    /// against an isolated synthetic `$CODEX_HOME` and the operator's
    /// real one).
    pub fn call(
        &mut self,
        method: &'static str,
        params: serde_json::Value,
        deadline: Duration,
    ) -> Result<(serde_json::Value, Duration), AppServerError> {
        let start = Instant::now();
        let id = self.next_id;
        self.next_id += 1;

        let request = serde_json::json!({"id": id, "method": method, "params": params});
        let mut line =
            serde_json::to_string(&request).map_err(|e| AppServerError::Io(e.to_string()))?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .map_err(|e| AppServerError::Io(e.to_string()))?;
        self.stdin
            .flush()
            .map_err(|e| AppServerError::Io(e.to_string()))?;

        let deadline_at = Instant::now() + deadline;
        loop {
            let remaining = deadline_at.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(AppServerError::Timeout(deadline, method));
            }
            match self.rx.recv_timeout(remaining) {
                Ok(ReaderEvent::Line(raw)) => {
                    let trimmed = raw.trim_end_matches(['\n', '\r']);
                    if trimmed.is_empty() {
                        continue;
                    }
                    let value: serde_json::Value = match serde_json::from_str(trimmed) {
                        Ok(v) => v,
                        Err(e) => return Err(AppServerError::MalformedLine(method, e.to_string())),
                    };
                    let response_id = value.get("id").and_then(|v| v.as_i64());
                    if response_id != Some(id) {
                        // A notification, or a response to an id we
                        // already consumed — ignore and keep waiting.
                        continue;
                    }
                    if let Some(error) = value.get("error") {
                        return Err(AppServerError::RpcError(method, error.to_string()));
                    }
                    let result = value
                        .get("result")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    return Ok((result, start.elapsed()));
                }
                Ok(ReaderEvent::Eof) => return Err(AppServerError::UnexpectedEof(method)),
                Ok(ReaderEvent::Io(e)) => return Err(AppServerError::Io(e)),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    return Err(AppServerError::Timeout(deadline, method))
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(AppServerError::UnexpectedEof(method))
                }
            }
        }
    }

    /// Best-effort kill, idempotent. Called explicitly by
    /// `codex_verify_trust_cmd` after the round trip completes (success
    /// or failure) and again from `Drop`, so a panic unwinding through
    /// this client still cannot leak the child process.
    pub fn kill(&mut self) {
        if self.killed {
            return;
        }
        self.killed = true;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for AppServerClient {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Writes a POSIX `sh` fixture script that speaks a tiny, scripted
    /// JSON-RPC session: it reads exactly as many input lines as the
    /// `mode` requires and prints a fixed, mode-dependent output
    /// sequence to stdout. No real `codex` binary involved — this tests
    /// the transport, not Codex's own protocol semantics (that
    /// correlation is exercised in `codex_verify_trust_cmd`'s own tests
    /// against real recorded fixture shapes).
    fn fixture_script(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        let path = dir.join("fixture.sh");
        let mut file = std::fs::File::create(&path).unwrap();
        write!(file, "#!/bin/sh\n{body}").unwrap();
        drop(file);
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[test]
    fn happy_path_call_returns_the_matching_response_skipping_notifications() {
        let dir = tempfile::tempdir().unwrap();
        let script = fixture_script(
            dir.path(),
            r#"
read -r _req1
printf '{"method":"remoteControl/status/changed","params":{}}\n'
printf '{"id":1,"result":{"ok":true}}\n'
"#,
        );
        let (mut client, _spawn_dur) =
            AppServerClient::spawn(script.to_str().unwrap(), &[], &[]).unwrap();
        let (result, _call_dur) = client
            .call("initialize", serde_json::json!({}), Duration::from_secs(5))
            .unwrap();
        assert_eq!(result, serde_json::json!({"ok": true}));
    }

    #[test]
    fn spawn_of_a_nonexistent_binary_is_an_error_not_a_panic() {
        let err =
            AppServerClient::spawn("/nonexistent/not-a-real-binary-xyz", &[], &[]).unwrap_err();
        assert!(matches!(err, AppServerError::Spawn(_, _)));
    }

    #[test]
    fn rpc_timeout_is_reported_not_hung_forever() {
        let dir = tempfile::tempdir().unwrap();
        // Never writes a response at all — the request line is read and
        // discarded, then the script sleeps well past our deadline.
        let script = fixture_script(dir.path(), "read -r _req1\nsleep 30\n");
        let (mut client, _spawn_dur) =
            AppServerClient::spawn(script.to_str().unwrap(), &[], &[]).unwrap();
        let start = Instant::now();
        let err = client
            .call(
                "hooks/list",
                serde_json::json!({}),
                Duration::from_millis(200),
            )
            .unwrap_err();
        assert!(matches!(err, AppServerError::Timeout(_, "hooks/list")));
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "must return promptly once the deadline is hit, not wait for the sleeping child"
        );
        client.kill();
    }

    #[test]
    fn malformed_json_line_is_reported_not_panicked_on() {
        let dir = tempfile::tempdir().unwrap();
        let script = fixture_script(dir.path(), "read -r _req1\nprintf 'not-json-at-all\\n'\n");
        let (mut client, _spawn_dur) =
            AppServerClient::spawn(script.to_str().unwrap(), &[], &[]).unwrap();
        let err = client
            .call("hooks/list", serde_json::json!({}), Duration::from_secs(5))
            .unwrap_err();
        assert!(matches!(
            err,
            AppServerError::MalformedLine("hooks/list", _)
        ));
    }

    #[test]
    fn truncated_response_then_immediate_close_is_unexpected_eof() {
        let dir = tempfile::tempdir().unwrap();
        // Writes a truncated (incomplete) JSON fragment with no trailing
        // newline, then exits — read_line returns the partial fragment
        // as its final (non-newline-terminated) "line", which fails to
        // parse as JSON, so this exercises MalformedLine; a process that
        // closes stdout with *zero* bytes written exercises
        // UnexpectedEof instead (see the next test).
        let script = fixture_script(dir.path(), "read -r _req1\nprintf '{\"id\":1,\"resul'\n");
        let (mut client, _spawn_dur) =
            AppServerClient::spawn(script.to_str().unwrap(), &[], &[]).unwrap();
        let err = client
            .call("hooks/list", serde_json::json!({}), Duration::from_secs(5))
            .unwrap_err();
        assert!(matches!(
            err,
            AppServerError::MalformedLine("hooks/list", _)
        ));
    }

    #[test]
    fn process_exiting_with_no_output_is_unexpected_eof() {
        let dir = tempfile::tempdir().unwrap();
        let script = fixture_script(dir.path(), "read -r _req1\nexit 0\n");
        let (mut client, _spawn_dur) =
            AppServerClient::spawn(script.to_str().unwrap(), &[], &[]).unwrap();
        let err = client
            .call("hooks/list", serde_json::json!({}), Duration::from_secs(5))
            .unwrap_err();
        assert!(matches!(err, AppServerError::UnexpectedEof("hooks/list")));
    }

    #[test]
    fn rpc_error_response_is_surfaced_distinctly() {
        let dir = tempfile::tempdir().unwrap();
        let script = fixture_script(
            dir.path(),
            r#"read -r _req1
printf '{"id":1,"error":{"code":-32601,"message":"method not found"}}\n'
"#,
        );
        let (mut client, _spawn_dur) =
            AppServerClient::spawn(script.to_str().unwrap(), &[], &[]).unwrap();
        let err = client
            .call("hooks/list", serde_json::json!({}), Duration::from_secs(5))
            .unwrap_err();
        assert!(matches!(err, AppServerError::RpcError("hooks/list", _)));
    }

    // Regression test (security review, HORO-1906): a line with no
    // terminating newline must not make the reader thread buffer
    // without bound — it must be reported as an error (`UnexpectedEof`
    // or `Io`, depending on exactly when the cap trips relative to the
    // child's own exit) promptly, not hang, and not consume unbounded
    // memory.
    #[test]
    fn a_line_with_no_terminator_past_the_cap_is_reported_not_hung_or_unbounded() {
        let dir = tempfile::tempdir().unwrap();
        // Writes well past MAX_LINE_BYTES with NO newline at all, then
        // exits — `dd ... | tr '\0' 'x'` produces an exact, bounded byte
        // count of the single byte 'x' repeated, fast and
        // newline-free (unlike `yes`, which emits a `\n` after every
        // short line and would never actually exercise the cap).
        let script = fixture_script(
            dir.path(),
            "read -r _req1\n/bin/dd if=/dev/zero bs=1000000 count=9 2>/dev/null | /usr/bin/tr \
             '\\0' 'x'\n",
        );
        let (mut client, _spawn_dur) =
            AppServerClient::spawn(script.to_str().unwrap(), &[], &[]).unwrap();
        let start = Instant::now();
        let err = client
            .call("hooks/list", serde_json::json!({}), Duration::from_secs(10))
            .unwrap_err();
        assert!(
            matches!(
                err,
                AppServerError::Io(_) | AppServerError::UnexpectedEof(_)
            ),
            "{err:?}"
        );
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "must not hang waiting for the full write to complete"
        );
        client.kill();
    }

    #[test]
    fn kill_is_idempotent_and_drop_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let script = fixture_script(dir.path(), "read -r _req1\nsleep 30\n");
        let (mut client, _spawn_dur) =
            AppServerClient::spawn(script.to_str().unwrap(), &[], &[]).unwrap();
        client.kill();
        client.kill(); // idempotent, must not panic
        drop(client); // Drop's own kill() must also be a safe no-op here
    }
}
