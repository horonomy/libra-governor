//! Captures an [`ExecutionIdentity`] (HORO-1599) for a hook event, and
//! resolves this machine's stable `host_id`.
//!
//! `host_id` is a Horonom-generated opaque random identifier, persisted
//! once under the daemon's state dir — never derived from hostname, MAC
//! address, serial number, or any other machine fingerprint (see
//! `governance/product/execution-identity-contract.md`'s "Security /
//! privacy" section in `horonomy/.github`).
//!
//! Lineage is always [`LineageStatus::Unknown`] here: neither Claude
//! Code's nor Codex's verified real hook payload exposes a
//! `parent_agent_id`-equivalent field today (see `agent::payload`'s
//! module docs) — "agent_id known, parent_agent_id unavailable" is
//! preserved exactly rather than guessed.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use libra_governor_domain::{
    AgentKind, ExecutionIdentity, ExecutionIdentityBuilder, ExecutionIdentityError,
};
use time::OffsetDateTime;

const HOST_ID_FILE_NAME: &str = "host_id";

/// Resolves this machine's stable `host_id`, generating and persisting
/// one on first use. Never derived from any machine fingerprint.
pub fn resolve_host_id(state_dir: &Path) -> io::Result<String> {
    let path = state_dir.join(HOST_ID_FILE_NAME);
    if let Some(existing) = read_host_id(&path)? {
        return Ok(existing);
    }

    fs::create_dir_all(state_dir)?;

    // Publish only after the complete, private file has been written. A
    // hard link makes creation atomic and refuses to replace a concurrent
    // winner, unlike writing directly to `host_id`.
    let (temporary_path, mut temporary_file) = create_host_id_temporary(state_dir)?;
    let _temporary_file = RemoveFileOnDrop(temporary_path.clone());
    let host_id = uuid::Uuid::new_v4().to_string();
    temporary_file.write_all(host_id.as_bytes())?;
    temporary_file.sync_all()?;
    drop(temporary_file);

    match fs::hard_link(&temporary_path, &path) {
        Ok(()) => Ok(host_id),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => read_host_id(&path)?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "host_id disappeared while another process was publishing it",
                )
            }),
        Err(error) => Err(error),
    }
}

fn read_host_id(path: &Path) -> io::Result<Option<String>> {
    let existing = match fs::read_to_string(path) {
        Ok(existing) => existing,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let trimmed = existing.trim();
    if trimmed.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "existing host_id file is empty",
        ));
    }
    Ok(Some(trimmed.to_string()))
}

fn create_host_id_temporary(state_dir: &Path) -> io::Result<(std::path::PathBuf, fs::File)> {
    for _ in 0..10 {
        let path = state_dir.join(format!(".{HOST_ID_FILE_NAME}.{}.tmp", uuid::Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique temporary host_id file",
    ))
}

struct RemoveFileOnDrop(std::path::PathBuf);

impl Drop for RemoveFileOnDrop {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Builds the [`ExecutionIdentity`] for one hook event. Optional host
/// IDs are preserved exactly as supplied (see `agent::payload`);
/// lineage is always `Unknown`.
pub fn capture(
    host_id: &str,
    agent: AgentKind,
    provider_session_id: &str,
    agent_id: Option<&str>,
    turn_id: Option<&str>,
    now: OffsetDateTime,
) -> Result<ExecutionIdentity, ExecutionIdentityError> {
    let mut builder = ExecutionIdentityBuilder::new(host_id, agent.as_tool_provider())
        .provider_session_id(provider_session_id);

    if let Some(agent_id) = agent_id {
        builder = builder.agent_id(agent_id);
    }
    if let Some(turn_id) = turn_id {
        builder = builder.turn_id(turn_id);
    }

    builder.build_at(now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use libra_governor_domain::LineageStatus;

    #[test]
    fn resolve_host_id_persists_and_is_stable_across_calls() {
        let dir = tempfile::tempdir().unwrap();
        let first = resolve_host_id(dir.path()).unwrap();
        let second = resolve_host_id(dir.path()).unwrap();
        assert_eq!(first, second);
        assert!(!first.is_empty());
    }

    #[test]
    fn concurrent_first_resolution_returns_the_atomically_persisted_id() {
        use std::sync::{Arc, Barrier};

        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().to_path_buf();
        let barrier = Arc::new(Barrier::new(24));
        let callers: Vec<_> = (0..24)
            .map(|_| {
                let state_dir = state_dir.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    resolve_host_id(&state_dir).unwrap()
                })
            })
            .collect();

        let ids: Vec<_> = callers
            .into_iter()
            .map(|caller| caller.join().unwrap())
            .collect();
        let persisted = fs::read_to_string(state_dir.join(HOST_ID_FILE_NAME)).unwrap();
        assert!(ids.iter().all(|id| id == &persisted));
        assert_eq!(
            fs::read_dir(&state_dir).unwrap().count(),
            1,
            "only the published host_id file should remain"
        );
    }

    #[test]
    fn resolve_host_id_does_not_replace_an_invalid_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(HOST_ID_FILE_NAME);
        fs::write(&path, "  \n").unwrap();

        let error = resolve_host_id(dir.path()).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fs::read_to_string(path).unwrap(), "  \n");
    }

    #[test]
    fn resolve_host_id_is_not_derived_from_any_machine_fingerprint() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let a = resolve_host_id(dir_a.path()).unwrap();
        let b = resolve_host_id(dir_b.path()).unwrap();
        assert_ne!(a, b, "two fresh state dirs must get independent host_ids");
    }

    #[cfg(unix)]
    #[test]
    fn resolve_host_id_file_is_not_group_or_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        resolve_host_id(dir.path()).unwrap();
        let meta = fs::metadata(dir.path().join(HOST_ID_FILE_NAME)).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn capture_leaves_lineage_unknown_and_parent_absent() {
        let identity = capture(
            "host-1",
            AgentKind::Codex,
            "sess-1",
            Some("sub-1"),
            Some("turn-1"),
            OffsetDateTime::now_utc(),
        )
        .unwrap();
        assert_eq!(identity.host_id(), "host-1");
        assert_eq!(identity.tool_provider(), "codex");
        assert_eq!(identity.agent_id(), Some("sub-1"));
        assert_eq!(identity.turn_id(), Some("turn-1"));
        assert_eq!(identity.lineage_status(), LineageStatus::Unknown);
        assert_eq!(identity.parent_agent_id(), None);
    }

    #[test]
    fn capture_leaves_agent_and_turn_id_absent_when_the_host_sends_neither() {
        let identity = capture(
            "host-1",
            AgentKind::ClaudeCode,
            "sess-1",
            None,
            None,
            OffsetDateTime::now_utc(),
        )
        .unwrap();
        assert_eq!(identity.agent_id(), None);
        assert_eq!(identity.turn_id(), None);
    }
}
