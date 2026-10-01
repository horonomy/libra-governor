//! Safe read-modify-write editing of Claude Code's `settings.json`
//! (HORO-1150).
//!
//! `libra-governor install`/`uninstall` need to add/remove exactly the
//! keys this integration owns in a file that may already hold a user's
//! own hooks, statusline, environment variables, and `apiKeyHelper` —
//! from other tools or hand-edited. The single highest-risk failure mode
//! in this ticket is a broken edit here corrupting or truncating that
//! file. This module's whole job is to make that structurally hard:
//!
//! 1. A file that fails to parse as JSON, or whose root is not a JSON
//!    object, aborts the whole operation with an error and changes
//!    nothing — there is deliberately no fallback that writes a fresh
//!    file over a file we could not understand.
//! 2. A backup of the original bytes is written alongside the file
//!    before any write.
//! 3. Only the specific keys/array-entries this module owns are removed
//!    from the parsed [`serde_json::Value`] — the whole document is never
//!    replaced, only targeted removals on the existing tree.
//! 4. The new content is written to a temp file in the same directory,
//!    fsynced, then renamed over the original — never truncated in
//!    place, so a crash mid-write cannot leave a half-written file.
//! 5. Immediately before that write, the file is re-read and its bytes
//!    re-hashed, then compared against what this process read at the
//!    start of the operation. If they differ, some other writer changed
//!    the file in between and the whole operation aborts with
//!    [`SettingsError::ConcurrentModification`] — no backup, no temp
//!    file, no write at all, so the racing writer's content survives
//!    exactly as it is rather than being silently clobbered by an edit
//!    computed from stale content (HORO-1380 S2b / HORO-998
//!    `CONCURRENT_CHANGE_DOES_NOT_CLOBBER`). This narrows the lost-update
//!    window; it does not close it. Closing it would need an advisory
//!    lock that the other writers of this file (Claude Code itself, a
//!    user's `$EDITOR`) do not take, so a check-then-rename is the
//!    strongest honest guarantee available here.
//!
//! # Ownership predicate
//!
//! A hook/statusline/`apiKeyHelper` command string is "ours" only if it
//! both contains `libra-governor` *and* ends with one of the exact
//! subcommand invocations this integration documents
//! (`integrations/claude-code/README.md`'s "Setup" and "Enforcement
//! gateway" sections) — see [`OWNED_COMMAND_SUFFIXES`]. Both conditions
//! must hold, so a user's own differently named wrapper script is never
//! touched.
//!
//! `env.ANTHROPIC_BASE_URL` is only ours when its host is a loopback
//! literal (`127.0.0.1`, `localhost`, `::1`) — a corporate proxy URL a
//! user configured for an unrelated reason is left alone.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

pub const SETTINGS_FILE_NAME: &str = "settings.json";

/// Exact trailing invocation shapes `integrations/claude-code/README.md`
/// documents. A command string is Governor-owned only if it ends with
/// one of these *and* contains `libra-governor` — see module docs.
const OWNED_COMMAND_SUFFIXES: [&str; 5] = [
    "hook user-prompt-submit",
    "hook post-tool-use",
    "hook stop",
    "statusline",
    "gateway token",
];

const HOOK_EVENTS: [&str; 3] = ["UserPromptSubmit", "PostToolUse", "Stop"];

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
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
    #[error("{path}'s top-level \"hooks\" field is present but is not a JSON object")]
    HooksFieldNotAnObject { path: PathBuf },
    #[error("{path}'s \"hooks.{event}\" field is present but is not a JSON array")]
    HookEventNotAnArray { path: PathBuf, event: String },
    #[error(
        "{path} changed on disk after it was read and before this edit could be written; \
         refusing to overwrite another writer's change (nothing was changed) — re-run to \
         apply this edit on top of the new content"
    )]
    ConcurrentModification { path: PathBuf },
}

/// Resolution order: `LIBRA_GOVERNOR_CLAUDE_DIR` (primarily for tests, so
/// this module's tests never touch a real developer's `~/.claude`),
/// else `$HOME/.claude` — Claude Code's own user-level settings
/// directory.
pub fn claude_dir() -> Result<PathBuf, SettingsError> {
    if let Ok(dir) = std::env::var("LIBRA_GOVERNOR_CLAUDE_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var("HOME").map_err(|_| SettingsError::NoHomeDir)?;
    Ok(PathBuf::from(home).join(".claude"))
}

pub fn settings_path() -> Result<PathBuf, SettingsError> {
    Ok(claude_dir()?.join(SETTINGS_FILE_NAME))
}

fn command_is_ours(command: &str) -> bool {
    command.contains("libra-governor")
        && OWNED_COMMAND_SUFFIXES
            .iter()
            .any(|suffix| command.ends_with(suffix))
}

/// `true` only for a loopback host — see module docs on why
/// `ANTHROPIC_BASE_URL` removal is scoped this way.
fn is_loopback_base_url(url: &str) -> bool {
    let without_scheme = url.split("://").nth(1).unwrap_or(url);
    let host = without_scheme
        .trim_start_matches('[')
        .split(['/', ':', ']'])
        .next()
        .unwrap_or("");
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// A sha256 over a config file's exact bytes at one instant, used only
/// to answer "is this still the file I read?" immediately before a write
/// (see module docs property 5). A plain byte comparison would be
/// equivalent here; sha256 keeps this module's check identical to the
/// reference implementation of the same HORO-998 property in
/// `circinus`'s `claude_code/installer.py::_atomic_replace`, and keeps
/// the retained value fixed-size rather than growing with the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileDigest([u8; 32]);

impl FileDigest {
    fn of(bytes: &[u8]) -> Self {
        let mut out = [0u8; 32];
        out.copy_from_slice(&Sha256::digest(bytes));
        Self(out)
    }

    /// The digest of a file that is not there. A file created by another
    /// writer between an absent read and the write therefore fails the
    /// check too, rather than being overwritten.
    fn absent() -> Self {
        Self::of(&[])
    }
}

/// The file's bytes, or `None` if it does not exist.
fn read_bytes_if_present(path: &Path) -> Result<Option<Vec<u8>>, SettingsError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(SettingsError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Reads and parses `path` into a JSON object, or `None` if the file does
/// not exist yet. Errors (never falls back) on malformed JSON or a
/// non-object root. The returned [`FileDigest`] is of the exact bytes
/// read and must be handed to [`write_object_atomically`] so the write
/// can refuse to clobber a change made in between.
fn read_object(path: &Path) -> Result<(Option<Map<String, Value>>, FileDigest), SettingsError> {
    let Some(raw) = read_bytes_if_present(path)? else {
        return Ok((None, FileDigest::absent()));
    };
    let digest = FileDigest::of(&raw);
    let text = std::str::from_utf8(&raw).map_err(|_| SettingsError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "stream did not contain valid UTF-8",
        ),
    })?;
    let value: Value = serde_json::from_str(text).map_err(|source| SettingsError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    match value {
        Value::Object(map) => Ok((Some(map), digest)),
        _ => Err(SettingsError::NotAnObject {
            path: path.to_path_buf(),
        }),
    }
}

/// Writes `map` to `path`, but only if `expected` still matches what is
/// on disk: the file is re-read and re-hashed first and a mismatch
/// aborts with [`SettingsError::ConcurrentModification`] before anything
/// at all is written (module docs property 5). Otherwise a
/// verbatim-bytes backup of any existing file is taken, then a temp file
/// in the same directory, fsynced, then renamed over the original. Never
/// truncates `path` in place. Returns the backup path, when one was
/// written.
///
/// The concurrency check and the backup live in this one function on
/// purpose rather than being split across caller and callee: a stray
/// backup left behind by an aborted write is only structurally
/// impossible if nothing can take a backup before the check has passed.
fn write_object_atomically(
    path: &Path,
    map: &Map<String, Value>,
    expected: FileDigest,
) -> Result<Option<PathBuf>, SettingsError> {
    let io_err = |source: std::io::Error| SettingsError::Io {
        path: path.to_path_buf(),
        source,
    };

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).map_err(io_err)?;

    // The first thing this function observes about the file itself, and
    // the gate on every mutating step below it.
    let now_raw = read_bytes_if_present(path)?.unwrap_or_default();
    if FileDigest::of(&now_raw) != expected {
        return Err(SettingsError::ConcurrentModification {
            path: path.to_path_buf(),
        });
    }

    // Serialized before the backup is taken so that even a (practically
    // impossible) serialization failure cannot leave a backup behind for
    // a write that never happened.
    let json_text =
        serde_json::to_string_pretty(&Value::Object(map.clone())).map_err(|source| {
            SettingsError::Parse {
                path: path.to_path_buf(),
                source,
            }
        })?;

    let backup_path = if !now_raw.is_empty() {
        let unix_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| SETTINGS_FILE_NAME.to_string());
        // Two calls within the same wall-clock second (e.g. `apply`
        // immediately followed by `remove` in a test, or two rapid CLI
        // invocations) must never share a backup filename — a second
        // write would silently overwrite the first backup, destroying
        // the only copy of the user's pre-edit file. Include the pid
        // (distinguishes concurrent processes) and then probe for the
        // first unused suffix (distinguishes same-process, same-second,
        // same-pid calls) rather than trusting either alone to be unique.
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
            .unwrap_or_else(|| SETTINGS_FILE_NAME.to_string()),
        std::process::id()
    ));
    let write_result = (|| -> Result<(), std::io::Error> {
        let mut file = std::fs::File::create(&tmp_path)?;
        file.write_all(json_text.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp_path, path)
    })();
    if let Err(source) = write_result {
        // The write itself failed (disk full, a permission race): leave
        // nothing at all behind from this attempt — not just an
        // untouched original, but no leaked temp file and no backup of a
        // write that never happened.
        let _ = std::fs::remove_file(&tmp_path);
        if let Some(backup) = &backup_path {
            let _ = std::fs::remove_file(backup);
        }
        return Err(io_err(source));
    }

    Ok(backup_path)
}

/// What `apply` did.
#[derive(Debug, Default, PartialEq)]
pub struct Applied {
    pub hooks_added: usize,
    pub statusline_added: bool,
    /// `true` when `statusLine` was already present and set to something
    /// other than this integration's own command. Left untouched rather
    /// than overwritten — see `apply`'s doc comment.
    pub statusline_conflict: bool,
    pub backup_path: Option<PathBuf>,
}

/// Adds this integration's `UserPromptSubmit`/`PostToolUse`/`Stop` hooks
/// and `statusLine`, pointing at `binary`, to `path`. Idempotent: an
/// already-present Governor-owned entry is not duplicated. Never touches
/// any other key. `binary` should be an absolute path (a relative path
/// in `settings.json` would only work when Claude Code happens to be
/// launched from the right working directory).
pub fn apply(path: &Path, binary: &Path) -> Result<Applied, SettingsError> {
    let (existing, digest) = read_object(path)?;
    let mut root = existing.unwrap_or_default();
    let binary = binary.display().to_string();
    let mut hooks_added = 0usize;

    let hooks_obj = hooks_object_mut(&mut root, path)?;

    for (event, subcommand) in
        HOOK_EVENTS
            .iter()
            .zip(["hook user-prompt-submit", "hook post-tool-use", "hook stop"])
    {
        let command = format!("{binary} {subcommand}");
        let entries = hooks_obj
            .entry((*event).to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        let entries_arr =
            entries
                .as_array_mut()
                .ok_or_else(|| SettingsError::HookEventNotAnArray {
                    path: path.to_path_buf(),
                    event: (*event).to_string(),
                })?;

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
            entries_arr.push(serde_json::json!({
                "hooks": [{ "type": "command", "command": command }]
            }));
            hooks_added += 1;
        }
    }

    let statusline_command = format!("{binary} statusline");
    let existing_statusline_command = root
        .get("statusLine")
        .and_then(|v| v.get("command"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let statusline_already_ours =
        existing_statusline_command.as_deref() == Some(statusline_command.as_str());
    // A foreign `statusLine` (any value not already ours) is left
    // untouched — overwriting it would silently destroy a user's own
    // statusline integration, which is exactly the kind of "conflicting
    // config overwritten blindly" this module exists to avoid. Only an
    // absent or already-ours `statusLine` is written.
    let statusline_conflict = existing_statusline_command.is_some() && !statusline_already_ours;
    let statusline_added = !statusline_already_ours && !statusline_conflict;
    if statusline_added {
        root.insert(
            "statusLine".to_string(),
            serde_json::json!({ "type": "command", "command": statusline_command }),
        );
    }

    let backup_path = write_object_atomically(path, &root, digest)?;
    Ok(Applied {
        hooks_added,
        statusline_added,
        statusline_conflict,
        backup_path,
    })
}

/// Returns the `root["hooks"]` object, creating an empty one if absent.
/// Errors (never silently replaces) if `root["hooks"]` exists but is not
/// itself a JSON object — that would mean either a hand-edited file in a
/// shape this module cannot safely reason about, or a newer Claude Code
/// version changing the field's type; either way, refuse rather than
/// discard whatever is actually there (HORO-1380 S2: this function used
/// to silently replace a non-object `hooks` field with `{}`, clobbering
/// it on a single invocation with no concurrency required — the
/// HORO-998 `UNKNOWN_FUTURE_FIELDS_ARE_PRESERVED` property this module's
/// own docs already claim to uphold). Mirrors
/// `codex_hooks_file::hooks_object_mut` exactly.
fn hooks_object_mut<'a>(
    root: &'a mut Map<String, Value>,
    path: &Path,
) -> Result<&'a mut Map<String, Value>, SettingsError> {
    let entry = root
        .entry("hooks".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    entry
        .as_object_mut()
        .ok_or_else(|| SettingsError::HooksFieldNotAnObject {
            path: path.to_path_buf(),
        })
}

/// What `remove` did — every field a count so a caller/test can assert
/// exactly what was touched without re-parsing the file.
#[derive(Debug, Default, PartialEq)]
pub struct Removed {
    pub hook_commands_removed: usize,
    pub statusline_removed: bool,
    pub api_key_helper_removed: bool,
    pub base_url_removed: bool,
    pub backup_path: Option<PathBuf>,
    /// `true` if the file did not exist at all — a safe no-op.
    pub file_absent: bool,
}

/// Removes only Governor-owned keys from `path`: hook commands matching
/// [`command_is_ours`] (pruning empty matchers/event-arrays/the `hooks`
/// key itself only when our removal is what emptied them — see module
/// docs), `statusLine` if it is ours, `apiKeyHelper` if it is ours, and
/// `env.ANTHROPIC_BASE_URL` if it points at a loopback host. Every other
/// key, and every foreign entry inside `hooks`, is left exactly as it
/// was. A missing file is a safe no-op, not an error.
pub fn remove(path: &Path) -> Result<Removed, SettingsError> {
    let (existing, digest) = read_object(path)?;
    let Some(mut root) = existing else {
        return Ok(Removed {
            file_absent: true,
            ..Default::default()
        });
    };

    let mut hook_commands_removed = 0usize;
    if let Some(hooks_value) = root.get_mut("hooks") {
        if let Some(hooks_obj) = hooks_value.as_object_mut() {
            let mut events_to_drop = Vec::new();
            for (event, entries_value) in hooks_obj.iter_mut() {
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
                    // Only drop this matcher if OUR removal emptied an
                    // inner array that had entries before — never a
                    // matcher whose inner array was already empty.
                    if before > 0 && inner.is_empty() {
                        matchers_to_drop.push(idx);
                    }
                }
                for idx in matchers_to_drop.into_iter().rev() {
                    entries_arr.remove(idx);
                }
                if entries_arr.is_empty() {
                    events_to_drop.push(event.clone());
                }
            }
            for event in events_to_drop {
                hooks_obj.remove(&event);
            }
            if hooks_obj.is_empty() {
                root.remove("hooks");
            }
        }
    }

    let statusline_removed = root
        .get("statusLine")
        .and_then(|v| v.get("command"))
        .and_then(Value::as_str)
        .map(command_is_ours)
        .unwrap_or(false);
    if statusline_removed {
        root.remove("statusLine");
    }

    let api_key_helper_removed = root
        .get("apiKeyHelper")
        .and_then(Value::as_str)
        .map(command_is_ours)
        .unwrap_or(false);
    if api_key_helper_removed {
        root.remove("apiKeyHelper");
    }

    let mut base_url_removed = false;
    if let Some(env_obj) = root.get_mut("env").and_then(Value::as_object_mut) {
        let is_ours = env_obj
            .get("ANTHROPIC_BASE_URL")
            .and_then(Value::as_str)
            .map(is_loopback_base_url)
            .unwrap_or(false);
        if is_ours {
            env_obj.remove("ANTHROPIC_BASE_URL");
            base_url_removed = true;
        }
    }

    let backup_path = write_object_atomically(path, &root, digest)?;
    Ok(Removed {
        hook_commands_removed,
        statusline_removed,
        api_key_helper_removed,
        base_url_removed,
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
    pub statusline_wired: bool,
    pub api_key_helper_wired: bool,
    pub base_url_present: bool,
    pub base_url_is_loopback: bool,
}

/// Read-only inspection of `path` for `doctor`. A missing or unparsable
/// file is reported as simply "not present"/"nothing wired" rather than
/// propagating a parse error — `doctor` has its own, separate
/// config.json-style validity check for the daemon's `config.json`; a
/// malformed `settings.json` is Claude Code's own concern, and `doctor`
/// degrading to "nothing wired" here is more useful than crashing.
pub fn inspect(path: &Path) -> Inspection {
    let Ok((Some(root), _)) = read_object(path) else {
        return Inspection {
            file_present: path.exists(),
            ..Default::default()
        };
    };

    let mut hooks_wired = [false; 3];
    if let Some(hooks_obj) = root.get("hooks").and_then(Value::as_object) {
        for (idx, event) in HOOK_EVENTS.iter().enumerate() {
            hooks_wired[idx] = hooks_obj
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
    }

    let statusline_wired = root
        .get("statusLine")
        .and_then(|v| v.get("command"))
        .and_then(Value::as_str)
        .map(command_is_ours)
        .unwrap_or(false);

    let api_key_helper_wired = root
        .get("apiKeyHelper")
        .and_then(Value::as_str)
        .map(command_is_ours)
        .unwrap_or(false);

    let base_url = root
        .get("env")
        .and_then(|v| v.get("ANTHROPIC_BASE_URL"))
        .and_then(Value::as_str);
    let base_url_present = base_url.is_some();
    let base_url_is_loopback = base_url.map(is_loopback_base_url).unwrap_or(false);

    Inspection {
        file_present: true,
        hooks_wired,
        statusline_wired,
        api_key_helper_wired,
        base_url_present,
        base_url_is_loopback,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binary() -> PathBuf {
        PathBuf::from("/opt/libra/bin/libra-governor")
    }

    #[test]
    fn apply_on_a_missing_file_creates_it_with_only_our_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");

        let applied = apply(&path, &binary()).unwrap();
        assert_eq!(applied.hooks_added, 3);
        assert!(applied.statusline_added);
        assert!(applied.backup_path.is_none(), "nothing existed to back up");

        let inspection = inspect(&path);
        assert_eq!(inspection.hooks_wired, [true, true, true]);
        assert!(inspection.statusline_wired);
    }

    #[test]
    fn apply_twice_does_not_duplicate_hook_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        apply(&path, &binary()).unwrap();
        let second = apply(&path, &binary()).unwrap();
        assert_eq!(
            second.hooks_added, 0,
            "re-running apply must be a no-op on an already-wired file"
        );
        assert!(!second.statusline_added);
    }

    #[test]
    fn apply_refuses_and_writes_nothing_when_hooks_is_not_an_object() {
        // HORO-1380 S2 / HORO-998 UNKNOWN_FUTURE_FIELDS_ARE_PRESERVED: a
        // top-level "hooks" field of an unexpected shape (a newer Claude
        // Code version, or a hand-edited file) must never be silently
        // discarded and replaced with an empty object.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let original_bytes = serde_json::to_vec_pretty(&serde_json::json!({
            "hooks": "not-an-object-or-array"
        }))
        .unwrap();
        std::fs::write(&path, &original_bytes).unwrap();

        let err = apply(&path, &binary()).unwrap_err();
        assert!(matches!(err, SettingsError::HooksFieldNotAnObject { .. }));

        let bytes_after = std::fs::read(&path).unwrap();
        assert_eq!(
            bytes_after, original_bytes,
            "a refused apply must leave the file byte-for-byte unchanged"
        );
    }

    #[test]
    fn apply_refuses_and_writes_nothing_when_a_hook_event_is_not_an_array() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let original_bytes = serde_json::to_vec_pretty(&serde_json::json!({
            "hooks": { "UserPromptSubmit": "not-an-array" }
        }))
        .unwrap();
        std::fs::write(&path, &original_bytes).unwrap();

        let err = apply(&path, &binary()).unwrap_err();
        assert!(matches!(err, SettingsError::HookEventNotAnArray { .. }));

        let bytes_after = std::fs::read(&path).unwrap();
        assert_eq!(
            bytes_after, original_bytes,
            "a refused apply must leave the file byte-for-byte unchanged"
        );
    }

    #[test]
    fn apply_then_remove_returns_to_a_value_equal_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        apply(&path, &binary()).unwrap();
        let removed = remove(&path).unwrap();
        assert_eq!(removed.hook_commands_removed, 3);
        assert!(removed.statusline_removed);

        let inspection = inspect(&path);
        assert_eq!(inspection.hooks_wired, [false, false, false]);
        assert!(!inspection.statusline_wired);

        // The file itself must still be valid, minimal JSON afterward.
        let text = std::fs::read_to_string(&path).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        assert!(
            !value.as_object().unwrap().contains_key("hooks"),
            "an emptied hooks table must itself be removed"
        );
    }

    #[test]
    fn remove_preserves_every_foreign_entry_in_a_mixed_settings_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let seed = serde_json::json!({
            "hooks": {
                "PostToolUse": [
                    {
                        "hooks": [
                            { "type": "command", "command": "/opt/libra/bin/libra-governor hook post-tool-use" },
                            { "type": "command", "command": "/usr/local/bin/some-other-tool notify" }
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
            },
            "statusLine": { "type": "command", "command": "/usr/local/bin/my-other-statusline" },
            "apiKeyHelper": "/usr/local/bin/my-own-key-helper",
            "env": {
                "HTTP_PROXY": "http://corp-proxy.example.com:8080",
                "ANTHROPIC_BASE_URL": "https://corp-proxy.example.com/anthropic"
            }
        });
        std::fs::write(&path, serde_json::to_string_pretty(&seed).unwrap()).unwrap();

        let removed = remove(&path).unwrap();
        assert_eq!(
            removed.hook_commands_removed, 1,
            "only the Governor-owned PostToolUse command is ours"
        );
        assert!(!removed.statusline_removed);
        assert!(!removed.api_key_helper_removed);
        assert!(
            !removed.base_url_removed,
            "a non-loopback base URL is never ours"
        );

        let text = std::fs::read_to_string(&path).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value["hooks"]["PostToolUse"][0]["hooks"]
                .as_array()
                .unwrap()
                .len(),
            1,
            "the foreign PostToolUse entry must survive"
        );
        assert_eq!(
            value["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
            "/usr/local/bin/some-other-tool notify"
        );
        assert_eq!(
            value["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "/usr/local/bin/foreign-guard check"
        );
        assert_eq!(
            value["statusLine"]["command"],
            "/usr/local/bin/my-other-statusline"
        );
        assert_eq!(value["apiKeyHelper"], "/usr/local/bin/my-own-key-helper");
        assert_eq!(
            value["env"]["HTTP_PROXY"],
            "http://corp-proxy.example.com:8080"
        );
        assert_eq!(
            value["env"]["ANTHROPIC_BASE_URL"],
            "https://corp-proxy.example.com/anthropic"
        );
    }

    #[test]
    fn remove_takes_the_gateway_env_and_api_key_helper_keys_when_they_are_ours() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let seed = serde_json::json!({
            "env": { "ANTHROPIC_BASE_URL": "http://127.0.0.1:8787" },
            "apiKeyHelper": "/opt/libra/bin/libra-governor gateway token"
        });
        std::fs::write(&path, serde_json::to_string_pretty(&seed).unwrap()).unwrap();

        let removed = remove(&path).unwrap();
        assert!(removed.base_url_removed);
        assert!(removed.api_key_helper_removed);

        let text = std::fs::read_to_string(&path).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        assert!(value.get("apiKeyHelper").is_none());
        assert!(value["env"].get("ANTHROPIC_BASE_URL").is_none());
    }

    #[test]
    fn remove_on_a_missing_file_is_a_safe_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let removed = remove(&path).unwrap();
        assert!(removed.file_absent);
        assert!(!path.exists(), "a no-op must not create the file");
    }

    #[test]
    fn malformed_json_aborts_and_leaves_the_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{ not json").unwrap();

        let before = std::fs::read_to_string(&path).unwrap();
        let err = remove(&path).unwrap_err();
        assert!(matches!(err, SettingsError::Parse { .. }));
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(before, after, "a malformed file must never be rewritten");

        // No stray temp/backup file left behind by the aborted attempt.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("settings.json")]);
    }

    #[test]
    fn a_non_object_root_aborts_and_leaves_the_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "[1, 2, 3]").unwrap();

        let before = std::fs::read_to_string(&path).unwrap();
        let err = remove(&path).unwrap_err();
        assert!(matches!(err, SettingsError::NotAnObject { .. }));
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn a_backup_is_written_before_any_mutating_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let seed =
            serde_json::json!({"apiKeyHelper": "/opt/libra/bin/libra-governor gateway token"});
        let original_bytes = serde_json::to_string_pretty(&seed).unwrap();
        std::fs::write(&path, &original_bytes).unwrap();

        let removed = remove(&path).unwrap();
        let backup_path = removed.backup_path.expect("a backup must be written");
        let backup_bytes = std::fs::read_to_string(&backup_path).unwrap();
        assert_eq!(backup_bytes, original_bytes);
    }

    #[test]
    fn apply_never_overwrites_a_foreign_statusline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let seed = serde_json::json!({
            "statusLine": { "type": "command", "command": "/usr/local/bin/my-own-statusline.py" }
        });
        std::fs::write(&path, serde_json::to_string_pretty(&seed).unwrap()).unwrap();

        let applied = apply(&path, &binary()).unwrap();
        assert!(
            !applied.statusline_added,
            "a foreign statusLine must not be reported as added"
        );
        assert!(
            applied.statusline_conflict,
            "a foreign statusLine must be reported as a conflict"
        );

        let text = std::fs::read_to_string(&path).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value["statusLine"]["command"], "/usr/local/bin/my-own-statusline.py",
            "the foreign statusLine must survive install byte-for-byte in content"
        );
    }

    #[test]
    fn backup_filenames_never_collide_within_the_same_apply_remove_pair() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{}").unwrap();

        let applied = apply(&path, &binary()).unwrap();
        let removed = remove(&path).unwrap();

        let apply_backup = applied.backup_path.expect("apply must back up the file");
        let remove_backup = removed.backup_path.expect("remove must back up the file");
        assert_ne!(
            apply_backup, remove_backup,
            "two backups written in quick succession must never share a path"
        );
        assert!(
            apply_backup.exists(),
            "the first backup must survive the second write"
        );
        assert!(remove_backup.exists());
    }

    #[test]
    fn an_event_array_survives_when_a_sibling_matcher_remains_foreign() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let seed = serde_json::json!({
            "hooks": {
                "Stop": [
                    { "hooks": [{ "type": "command", "command": "/opt/libra/bin/libra-governor hook stop" }] },
                    { "hooks": [{ "type": "command", "command": "/usr/local/bin/other stop-notify" }] }
                ]
            }
        });
        std::fs::write(&path, serde_json::to_string_pretty(&seed).unwrap()).unwrap();

        remove(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        let stop = value["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1, "the foreign matcher must survive");
        assert_eq!(
            stop[0]["hooks"][0]["command"],
            "/usr/local/bin/other stop-notify"
        );
    }

    #[test]
    fn a_concurrent_change_between_read_and_write_is_refused() {
        // HORO-1380 S2b / HORO-998 CONCURRENT_CHANGE_DOES_NOT_CLOBBER:
        // an edit computed from content that is no longer on disk must
        // be thrown away, not written. Exercised at the
        // read_object/write_object_atomically seam because that is where
        // the property lives — `apply`/`remove` perform both halves
        // inside one synchronous call, so no public-API test can place a
        // racing writer between them without inventing a test hook.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{}\n").unwrap();

        // What this process read, and the edit it computed from it.
        let (root, digest) = read_object(&path).unwrap();
        let mut root = root.unwrap();
        root.insert(
            "statusLine".to_string(),
            serde_json::json!({ "type": "command", "command": "ours" }),
        );

        // What a racing writer put there in the meantime.
        let racing_bytes = br#"{"someoneElsesEdit": true}"#.to_vec();
        std::fs::write(&path, &racing_bytes).unwrap();

        let err = write_object_atomically(&path, &root, digest).unwrap_err();
        assert!(matches!(err, SettingsError::ConcurrentModification { .. }));

        assert_eq!(
            std::fs::read(&path).unwrap(),
            racing_bytes,
            "the racing writer's content must survive byte-for-byte — never clobbered by \
             our stale edit, never reverted to what we read"
        );

        // No backup and no temp file: the check runs before either can
        // be created, so a refused write leaves zero artifacts.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("settings.json")]);
    }

    #[test]
    fn a_file_created_between_an_absent_read_and_the_write_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");

        // Read it while it does not exist yet.
        let (root, digest) = read_object(&path).unwrap();
        assert!(root.is_none());
        let mut root = root.unwrap_or_default();
        root.insert(
            "statusLine".to_string(),
            serde_json::json!({ "type": "command", "command": "ours" }),
        );

        // Another writer creates it before we get to the write.
        let racing_bytes = br#"{"someoneElsesEdit": true}"#.to_vec();
        std::fs::write(&path, &racing_bytes).unwrap();

        let err = write_object_atomically(&path, &root, digest).unwrap_err();
        assert!(matches!(err, SettingsError::ConcurrentModification { .. }));

        assert_eq!(
            std::fs::read(&path).unwrap(),
            racing_bytes,
            "a file that appeared after an absent read must not be overwritten"
        );
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("settings.json")]);
    }

    #[test]
    fn is_loopback_base_url_accepts_only_loopback_hosts() {
        assert!(is_loopback_base_url("http://127.0.0.1:8787"));
        assert!(is_loopback_base_url("http://localhost:8787"));
        assert!(!is_loopback_base_url("https://api.example.com"));
        assert!(!is_loopback_base_url("http://10.0.0.5:8787"));
    }
}
