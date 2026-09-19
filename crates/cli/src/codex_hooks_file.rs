//! Safe read-modify-write editing of Codex CLI's `hooks.json`
//! (HORO-1157) — mirrors `crates/cli/src/claude_settings.rs`'s exact
//! safety discipline for the equivalent Claude Code file:
//!
//! 1. A file that fails to parse as JSON, or whose root is not a JSON
//!    object, aborts the whole operation with an error and changes
//!    nothing.
//! 2. A backup of the original bytes is written alongside the file
//!    before any write.
//! 3. Only the specific hook groups this module owns are removed from
//!    the parsed [`serde_json::Value`] — the whole document is never
//!    replaced, only targeted removals on the existing tree.
//! 4. The new content is written to a temp file in the same directory,
//!    fsynced, then renamed over the original — never truncated in
//!    place.
//!
//! # File shape, and what is verified vs. assumed
//!
//! Codex's own `hooks.json` documented shape was not independently
//! confirmed byte-for-byte during HORO-1157 (unlike the payload field
//! names and stdout contract, which were checked against `openai/codex`'s
//! generated JSON schemas — see `docs/adr/0004-agent-adapter-contract.md`).
//! This module assumes the same event-keyed array-of-matcher-groups shape
//! Claude Code's `settings.json` `hooks` table uses
//! (`{"<Event>": [{"hooks": [{"type": "command", "command": "...", ...}]}]}`),
//! since that is the shape `libra-governor codex-hook`'s own commands need
//! to be discoverable in and is consistent with Codex's own
//! `[hooks.state]` trust-gate design (which trusts by content hash of a
//! configured hook entry, implying the same "list of command entries per
//! event" structure). If Codex's real shape differs, `install --agent
//! codex` would need a follow-up fix — this is disclosed, not silently
//! assumed correct.
//!
//! # Ownership predicate
//!
//! A hook command string is "ours" only if it both contains
//! `libra-governor` *and* ends with one of the exact `codex-hook`
//! subcommand invocations this integration documents
//! (`integrations/codex/README.md`) — see [`OWNED_COMMAND_SUFFIXES`].
//! Both conditions must hold, so a user's own differently named wrapper
//! script is never touched.
//!
//! # Trust gate is out of scope for this file
//!
//! Writing `hooks.json` is necessary but not sufficient for Codex to
//! actually run these hooks — the user must separately run `/hooks`
//! inside Codex to trust them by content hash (`[hooks.state]` in
//! `~/.codex/config.toml`). `install_cmd`'s printed next-steps and
//! `doctor_cmd`'s read-only inspection cover that; this module only ever
//! writes/removes/inspects `hooks.json` itself.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

pub const HOOKS_FILE_NAME: &str = "hooks.json";

/// The hook timeout (seconds) written for every Codex hook entry this
/// integration installs — deliberately far below Codex's own 600s
/// default. A wedged daemon must not hang a user's prompt for ten
/// minutes: this repo's own daemon client timeout is ~5s plus a ~3s
/// spawn budget, so 15s is a safe real ceiling above that, not an
/// arbitrary round number.
pub const HOOK_TIMEOUT_SECS: u64 = 15;

/// Exact trailing invocation shapes `integrations/codex/README.md`
/// documents. A command string is Governor-owned only if it ends with
/// one of these *and* contains `libra-governor` — see module docs.
const OWNED_COMMAND_SUFFIXES: [&str; 3] = [
    "codex-hook user-prompt-submit",
    "codex-hook post-tool-use",
    "codex-hook stop",
];

const HOOK_EVENTS: [&str; 3] = ["UserPromptSubmit", "PostToolUse", "Stop"];

#[derive(Debug, thiserror::Error)]
pub enum CodexHooksError {
    #[error("could not determine home directory (HOME env var unset)")]
    NoHomeDir,
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} is not valid JSON: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error(
        "{path}'s top-level JSON value is not an object; refusing to edit it \
         (nothing was changed)"
    )]
    NotAnObject { path: PathBuf },
}

/// Resolution order: `LIBRA_GOVERNOR_CODEX_HOME` (primarily for tests, so
/// this module's tests never touch a real developer's `~/.codex`), else
/// `$CODEX_HOME` (Codex's own documented override), else `$HOME/.codex`.
pub fn codex_home() -> Result<PathBuf, CodexHooksError> {
    if let Ok(dir) = std::env::var("LIBRA_GOVERNOR_CODEX_HOME") {
        return Ok(PathBuf::from(dir));
    }
    if let Ok(dir) = std::env::var("CODEX_HOME") {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var("HOME").map_err(|_| CodexHooksError::NoHomeDir)?;
    Ok(PathBuf::from(home).join(".codex"))
}

pub fn hooks_path() -> Result<PathBuf, CodexHooksError> {
    Ok(codex_home()?.join(HOOKS_FILE_NAME))
}

fn command_is_ours(command: &str) -> bool {
    command.contains("libra-governor")
        && OWNED_COMMAND_SUFFIXES
            .iter()
            .any(|suffix| command.ends_with(suffix))
}

fn read_object(path: &Path) -> Result<Option<Map<String, Value>>, CodexHooksError> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path).map_err(|source| CodexHooksError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let value: Value = serde_json::from_str(&text).map_err(|source| CodexHooksError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    match value {
        Value::Object(map) => Ok(Some(map)),
        _ => Err(CodexHooksError::NotAnObject {
            path: path.to_path_buf(),
        }),
    }
}

/// Writes `map` to `path`: a verbatim-bytes backup of any existing file
/// first, then a temp file in the same directory, fsynced, then renamed
/// over the original. Mirrors `claude_settings::write_object_atomically`
/// exactly, including its collision-safe backup naming.
fn write_object_atomically(
    path: &Path,
    map: &Map<String, Value>,
) -> Result<Option<PathBuf>, CodexHooksError> {
    let io_err = |source: std::io::Error| CodexHooksError::Io {
        path: path.to_path_buf(),
        source,
    };

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).map_err(io_err)?;

    let backup_path = if path.exists() {
        let unix_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| HOOKS_FILE_NAME.to_string());
        let pid = std::process::id();
        let mut candidate = dir.join(format!("{file_name}.libra-backup-{unix_secs}-{pid}"));
        let mut attempt = 0u32;
        while candidate.exists() {
            attempt += 1;
            candidate = dir.join(format!(
                "{file_name}.libra-backup-{unix_secs}-{pid}-{attempt}"
            ));
        }
        std::fs::copy(path, &candidate).map_err(io_err)?;
        Some(candidate)
    } else {
        None
    };

    let tmp_path = dir.join(format!(
        "{}.libra-tmp-{}",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| HOOKS_FILE_NAME.to_string()),
        std::process::id()
    ));
    let json_text =
        serde_json::to_string_pretty(&Value::Object(map.clone())).map_err(|source| {
            CodexHooksError::Parse {
                path: path.to_path_buf(),
                source,
            }
        })?;
    {
        let mut file = std::fs::File::create(&tmp_path).map_err(io_err)?;
        file.write_all(json_text.as_bytes()).map_err(io_err)?;
        file.write_all(b"\n").map_err(io_err)?;
        file.sync_all().map_err(io_err)?;
    }
    std::fs::rename(&tmp_path, path).map_err(|source| {
        let _ = std::fs::remove_file(&tmp_path);
        io_err(source)
    })?;

    Ok(backup_path)
}

/// What [`apply`] did.
#[derive(Debug, Default, PartialEq)]
pub struct Applied {
    pub hooks_added: usize,
    pub backup_path: Option<PathBuf>,
}

/// Adds this integration's `UserPromptSubmit`/`PostToolUse`/`Stop` hook
/// groups, pointing at `binary`, to `path`. Idempotent: an already-present
/// Governor-owned entry is not duplicated. Never touches any other key or
/// hook group. `binary` should be an absolute path. Writes
/// [`HOOK_TIMEOUT_SECS`] on every entry and `"async": true` on the
/// `PostToolUse` entry only (it prints nothing and must never add
/// perceptible latency) — see that constant's docs. No `matcher` field is
/// written: this integration observes every tool, not a filtered subset.
pub fn apply(path: &Path, binary: &Path) -> Result<Applied, CodexHooksError> {
    let mut root = read_object(path)?.unwrap_or_default();
    let binary = binary.display().to_string();
    let mut hooks_added = 0usize;

    for (event, subcommand, is_async) in [
        ("UserPromptSubmit", "codex-hook user-prompt-submit", false),
        ("PostToolUse", "codex-hook post-tool-use", true),
        ("Stop", "codex-hook stop", false),
    ] {
        let command = format!("{binary} {subcommand}");
        let entries = root
            .entry(event.to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        let entries_arr = as_array_or_replace(entries);

        let already_present = entries_arr.iter().any(|matcher| {
            matcher
                .get("hooks")
                .and_then(Value::as_array)
                .map(|inner| {
                    inner
                        .iter()
                        .any(|h| h.get("command").and_then(Value::as_str) == Some(command.as_str()))
                })
                .unwrap_or(false)
        });
        if !already_present {
            let mut hook_entry = serde_json::json!({
                "type": "command",
                "command": command,
                "timeout": HOOK_TIMEOUT_SECS,
            });
            if is_async {
                hook_entry["async"] = Value::Bool(true);
            }
            entries_arr.push(serde_json::json!({ "hooks": [hook_entry] }));
            hooks_added += 1;
        }
    }

    let backup_path = write_object_atomically(path, &root)?;
    Ok(Applied {
        hooks_added,
        backup_path,
    })
}

fn as_array_or_replace(value: &mut Value) -> &mut Vec<Value> {
    if !value.is_array() {
        *value = Value::Array(Vec::new());
    }
    value.as_array_mut().expect("just ensured array")
}

/// What [`remove`] did.
#[derive(Debug, Default, PartialEq)]
pub struct Removed {
    pub hook_commands_removed: usize,
    pub backup_path: Option<PathBuf>,
    /// `true` if the file did not exist at all — a safe no-op.
    pub file_absent: bool,
}

/// Removes only Governor-owned hook groups from `path` — pruning empty
/// matchers/event-arrays only when our removal is what emptied them,
/// mirroring `claude_settings::remove`'s discipline exactly. Every other
/// key, and every foreign entry inside an event's array, is left exactly
/// as it was. A missing file is a safe no-op.
pub fn remove(path: &Path) -> Result<Removed, CodexHooksError> {
    let Some(mut root) = read_object(path)? else {
        return Ok(Removed {
            file_absent: true,
            ..Default::default()
        });
    };

    let mut hook_commands_removed = 0usize;
    let mut events_to_drop = Vec::new();
    for event in HOOK_EVENTS {
        let Some(entries_value) = root.get_mut(event) else {
            continue;
        };
        let Some(entries_arr) = entries_value.as_array_mut() else {
            continue;
        };
        let mut matchers_to_drop = Vec::new();
        for (idx, matcher) in entries_arr.iter_mut().enumerate() {
            let Some(inner) = matcher.get_mut("hooks").and_then(Value::as_array_mut) else {
                continue;
            };
            let before = inner.len();
            inner.retain(|h| {
                h.get("command")
                    .and_then(Value::as_str)
                    .map(|c| !command_is_ours(c))
                    .unwrap_or(true)
            });
            hook_commands_removed += before - inner.len();
            if before > 0 && inner.is_empty() {
                matchers_to_drop.push(idx);
            }
        }
        for idx in matchers_to_drop.into_iter().rev() {
            entries_arr.remove(idx);
        }
        if entries_arr.is_empty() {
            events_to_drop.push(event.to_string());
        }
    }
    for event in events_to_drop {
        root.remove(&event);
    }

    let backup_path = write_object_atomically(path, &root)?;
    Ok(Removed {
        hook_commands_removed,
        backup_path,
        file_absent: false,
    })
}

/// A read-only summary of what's currently wired, for `libra-governor
/// doctor` — never mutates the file.
#[derive(Debug, Default, PartialEq)]
pub struct Inspection {
    pub file_present: bool,
    pub hooks_wired: [bool; 3], // [UserPromptSubmit, PostToolUse, Stop]
}

/// Read-only inspection of `path` for `doctor`. A missing or unparsable
/// file is reported as simply "not present"/"nothing wired" rather than
/// propagating a parse error, mirroring `claude_settings::inspect`.
pub fn inspect(path: &Path) -> Inspection {
    let Ok(Some(root)) = read_object(path) else {
        return Inspection {
            file_present: path.exists(),
            ..Default::default()
        };
    };

    let mut hooks_wired = [false; 3];
    for (idx, event) in HOOK_EVENTS.iter().enumerate() {
        hooks_wired[idx] = root
            .get(*event)
            .and_then(Value::as_array)
            .map(|entries| {
                entries.iter().any(|matcher| {
                    matcher
                        .get("hooks")
                        .and_then(Value::as_array)
                        .map(|inner| {
                            inner.iter().any(|h| {
                                h.get("command")
                                    .and_then(Value::as_str)
                                    .map(command_is_ours)
                                    .unwrap_or(false)
                            })
                        })
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);
    }

    Inspection {
        file_present: true,
        hooks_wired,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binary() -> PathBuf {
        PathBuf::from("/opt/libra/bin/libra-governor")
    }

    #[test]
    fn apply_on_a_missing_file_creates_it_with_only_our_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");

        let applied = apply(&path, &binary()).unwrap();
        assert_eq!(applied.hooks_added, 3);
        assert!(applied.backup_path.is_none(), "nothing existed to back up");

        let inspection = inspect(&path);
        assert_eq!(inspection.hooks_wired, [true, true, true]);
    }

    #[test]
    fn apply_writes_the_documented_timeout_and_async_flag() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        apply(&path, &binary()).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();

        assert_eq!(
            value["UserPromptSubmit"][0]["hooks"][0]["timeout"],
            HOOK_TIMEOUT_SECS
        );
        assert!(value["UserPromptSubmit"][0]["hooks"][0]
            .get("async")
            .is_none());
        assert_eq!(
            value["PostToolUse"][0]["hooks"][0]["timeout"],
            HOOK_TIMEOUT_SECS
        );
        assert_eq!(value["PostToolUse"][0]["hooks"][0]["async"], true);
        assert!(value["Stop"][0]["hooks"][0].get("async").is_none());
    }

    #[test]
    fn apply_twice_does_not_duplicate_hook_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        apply(&path, &binary()).unwrap();
        let second = apply(&path, &binary()).unwrap();
        assert_eq!(
            second.hooks_added, 0,
            "re-running apply must be a no-op on an already-wired file"
        );
    }

    #[test]
    fn apply_then_remove_returns_to_a_value_equal_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        apply(&path, &binary()).unwrap();
        let removed = remove(&path).unwrap();
        assert_eq!(removed.hook_commands_removed, 3);

        let inspection = inspect(&path);
        assert_eq!(inspection.hooks_wired, [false, false, false]);

        let text = std::fs::read_to_string(&path).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        assert!(
            value.as_object().unwrap().is_empty(),
            "an emptied file must contain no leftover event keys"
        );
    }

    #[test]
    fn remove_preserves_every_foreign_entry_in_a_mixed_hooks_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        let seed = serde_json::json!({
            "PostToolUse": [
                {
                    "hooks": [
                        { "type": "command", "command": "/opt/libra/bin/libra-governor codex-hook post-tool-use", "timeout": 15, "async": true },
                        { "type": "command", "command": "/usr/local/bin/some-other-tool notify", "timeout": 5 }
                    ]
                }
            ],
            "PreToolUse": [
                {
                    "hooks": [
                        { "type": "command", "command": "/usr/local/bin/foreign-guard check" }
                    ]
                }
            ]
        });
        std::fs::write(&path, serde_json::to_string_pretty(&seed).unwrap()).unwrap();

        let removed = remove(&path).unwrap();
        assert_eq!(
            removed.hook_commands_removed, 1,
            "only the Governor-owned PostToolUse command is ours"
        );

        let text = std::fs::read_to_string(&path).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value["PostToolUse"][0]["hooks"].as_array().unwrap().len(),
            1,
            "the foreign PostToolUse entry must survive"
        );
        assert_eq!(
            value["PostToolUse"][0]["hooks"][0]["command"],
            "/usr/local/bin/some-other-tool notify"
        );
        assert_eq!(
            value["PreToolUse"][0]["hooks"][0]["command"],
            "/usr/local/bin/foreign-guard check"
        );
    }

    #[test]
    fn remove_on_a_missing_file_is_a_safe_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        let removed = remove(&path).unwrap();
        assert!(removed.file_absent);
        assert!(!path.exists(), "a no-op must not create the file");
    }

    #[test]
    fn malformed_json_aborts_and_leaves_the_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        std::fs::write(&path, "{ not json").unwrap();

        let before = std::fs::read_to_string(&path).unwrap();
        let err = remove(&path).unwrap_err();
        assert!(matches!(err, CodexHooksError::Parse { .. }));
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(before, after, "a malformed file must never be rewritten");
    }

    #[test]
    fn a_non_object_root_aborts_and_leaves_the_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        std::fs::write(&path, "[1, 2, 3]").unwrap();

        let before = std::fs::read_to_string(&path).unwrap();
        let err = remove(&path).unwrap_err();
        assert!(matches!(err, CodexHooksError::NotAnObject { .. }));
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn a_backup_is_written_before_any_mutating_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        let seed = serde_json::json!({"UserPromptSubmit": []});
        let original_bytes = serde_json::to_string_pretty(&seed).unwrap();
        std::fs::write(&path, &original_bytes).unwrap();

        let removed = remove(&path).unwrap();
        let backup_path = removed.backup_path.expect("a backup must be written");
        let backup_bytes = std::fs::read_to_string(&backup_path).unwrap();
        assert_eq!(backup_bytes, original_bytes);
    }

    #[test]
    fn an_event_array_survives_when_a_sibling_matcher_remains_foreign() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        let seed = serde_json::json!({
            "Stop": [
                { "hooks": [{ "type": "command", "command": "/opt/libra/bin/libra-governor codex-hook stop" }] },
                { "hooks": [{ "type": "command", "command": "/usr/local/bin/other stop-notify" }] }
            ]
        });
        std::fs::write(&path, serde_json::to_string_pretty(&seed).unwrap()).unwrap();

        remove(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        let stop = value["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1, "the foreign matcher must survive");
        assert_eq!(
            stop[0]["hooks"][0]["command"],
            "/usr/local/bin/other stop-notify"
        );
    }
}
