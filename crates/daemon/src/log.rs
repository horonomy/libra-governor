//! Minimal append-only file logger for the daemon and CLI client.
//!
//! stdout is reserved for the hook protocol response (the
//! `hookSpecificOutput` JSON Claude Code reads), so every diagnostic
//! message — malformed payloads, daemon-unreachable events, internal
//! errors caught rather than allowed to panic — goes here instead.
//! Callers must never pass raw prompt text or raw tool output content;
//! this module does not filter or redact for them (see the privacy
//! invariant documented on `libra-governor-domain`).

use std::io::Write;
use std::path::Path;

/// Appends one timestamped line to the log file at `path`. Best-effort:
/// a logging failure (e.g. an unwritable disk) is swallowed rather than
/// propagated, since failing to log must never be why a hook or the
/// daemon itself fails.
pub fn append_line(path: &Path, message: &str) {
    let now = time::OffsetDateTime::now_utc();
    let formatted = now
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "unknown-time".to_string());
    let line = format!("[{formatted}] {message}\n");

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = file.write_all(line.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_line_creates_file_and_writes_content() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("nested").join("daemon.log");
        append_line(&log_path, "daemon started");
        append_line(&log_path, "second line");

        let contents = std::fs::read_to_string(&log_path).unwrap();
        assert!(contents.contains("daemon started"));
        assert!(contents.contains("second line"));
    }

    #[test]
    fn append_line_never_panics_on_unwritable_path() {
        // Parent path component is a regular file, not a directory, so
        // neither create_dir_all nor the file open can succeed — best
        // effort: this must not panic even though it cannot write.
        let dir = tempfile::tempdir().unwrap();
        let not_a_dir = dir.path().join("not-a-directory");
        std::fs::write(&not_a_dir, "x").unwrap();
        append_line(&not_a_dir.join("daemon.log"), "x");
    }
}
