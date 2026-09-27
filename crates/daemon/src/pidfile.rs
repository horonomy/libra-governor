//! The daemon's pid record (HORO-1380): `<state_dir>/daemon.pid`, a
//! sibling of `daemon.sock`.
//!
//! Exists so `daemon stop` can identify *exactly* which process to
//! signal without ever falling back to a name-matching `pkill`, which
//! matches every Governor daemon on the host across every state dir —
//! the acceptance-criterion violation HORO-1380 exists to close (see
//! `crates/cli/src/doctor_cmd.rs`'s prior guidance text, now removed).
//!
//! Deliberately **not** wired into the wire protocol: adding a
//! `Request`/`Response` variant would bump `PROTOCOL_VERSION`, and this
//! repo's own history records that every protocol bump has previously
//! left operators needing to hand-kill an old daemon that no longer
//! understands the new wire format — exactly the chicken-and-egg this
//! file's fix must not itself depend on. A pid-file lookup works even
//! against a daemon speaking a protocol `daemon stop`'s own binary has
//! never heard of.
//!
//! Write path: the daemon overwrites this file, unconditionally,
//! immediately after it wins `bind_or_detect_running` — at that point it
//! holds the socket exclusively, so any prior record here (even a
//! genuinely stale one left by a daemon that was killed rather than
//! shut down cleanly) is safely superseded. There is deliberately no
//! read-modify-write: the file is either absent, or it describes
//! whichever daemon currently holds the socket.
//!
//! No corresponding "remove on clean exit": `serve` runs forever, and
//! the intended shutdown path (`daemon stop`, below) delivers `SIGTERM`,
//! which has no default Rust-level unwind to run cleanup in. A stale
//! record left behind after a signalled exit is harmless — the next
//! `daemon run` overwrites it the same way it already tolerates and
//! recovers a stale `daemon.sock` (see `server::bind_or_detect_running`).

use std::path::{Path, PathBuf};

/// One daemon's identity, as recorded at the moment it started serving.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PidRecord {
    pub pid: u32,
    pub exe_path: PathBuf,
    /// RFC 3339, best-effort — `"unknown-time"` if the clock could not be
    /// formatted (mirrors `crate::log`'s own fallback). Never used for
    /// comparison logic, only surfaced to an operator for context.
    pub started_at: String,
}

/// Writes `record` to `<state_dir>/daemon.pid`, replacing whatever was
/// there. A temp file in the same directory, then a rename over the
/// target — never a truncate-in-place — so a concurrent reader (a
/// `daemon stop` racing this write) always sees either the old complete
/// record or the new one, never a half-written one.
pub fn write(state_dir: &Path, record: &PidRecord) -> std::io::Result<()> {
    let path = state_dir.join("daemon.pid");
    let tmp_path = state_dir.join(format!("daemon.pid.tmp-{}", record.pid));
    let body = serde_json::to_vec_pretty(record)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&tmp_path, body)?;
    std::fs::rename(&tmp_path, &path)?;
    Ok(())
}

/// Reads and parses `<state_dir>/daemon.pid`. `None` for any reason at
/// all — absent, unreadable, or malformed — since every caller treats
/// "no usable record" identically: refuse to signal anything. This is
/// deliberately not a `Result`; there is no recovery action that differs
/// by failure cause.
pub fn read(state_dir: &Path) -> Option<PidRecord> {
    let bytes = std::fs::read(state_dir.join("daemon.pid")).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_write_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let record = PidRecord {
            pid: 12345,
            exe_path: PathBuf::from("/usr/local/bin/libra-governor"),
            started_at: "2026-09-27T00:00:00Z".to_string(),
        };
        write(dir.path(), &record).unwrap();
        assert_eq!(read(dir.path()), Some(record));
    }

    #[test]
    fn read_of_missing_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read(dir.path()), None);
    }

    #[test]
    fn read_of_malformed_file_is_none_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("daemon.pid"), b"not json").unwrap();
        assert_eq!(read(dir.path()), None);
    }

    #[test]
    fn write_replaces_a_prior_record_rather_than_merging() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            &PidRecord {
                pid: 1,
                exe_path: PathBuf::from("/old/path"),
                started_at: "2020-01-01T00:00:00Z".to_string(),
            },
        )
        .unwrap();
        let fresh = PidRecord {
            pid: 2,
            exe_path: PathBuf::from("/new/path"),
            started_at: "2026-09-27T00:00:00Z".to_string(),
        };
        write(dir.path(), &fresh).unwrap();
        assert_eq!(read(dir.path()), Some(fresh));
    }

    #[test]
    fn no_leftover_tmp_file_after_a_successful_write() {
        let dir = tempfile::tempdir().unwrap();
        let record = PidRecord {
            pid: 99,
            exe_path: PathBuf::from("/bin/x"),
            started_at: "2026-09-27T00:00:00Z".to_string(),
        };
        write(dir.path(), &record).unwrap();
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["daemon.pid".to_string()]);
    }
}
