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

use std::fs;
use std::io;
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
    if let Ok(existing) = fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }

    let host_id = uuid::Uuid::new_v4().to_string();
    fs::create_dir_all(state_dir)?;
    fs::write(&path, &host_id)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }

    Ok(host_id)
}

/// Builds the [`ExecutionIdentity`] for one hook event. `agent_id` and
/// `turn_id` are whatever the normalized event carried (Codex-only
/// today, see `agent::payload`); lineage is always `Unknown`.
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
