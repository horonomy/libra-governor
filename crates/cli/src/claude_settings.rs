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

/// Reads and parses `path` into a JSON object, or `None` if the file does
/// not exist yet. Errors (never falls back) on malformed JSON or a
/// non-object root.
fn read_object(path: &Path) -> Result<Option<Map<String, Value>>, SettingsError> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path).map_err(|source| SettingsError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let value: Value = serde_json::from_str(&text).map_err(|source| SettingsError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    match value {
        Value::Object(map) => Ok(Some(map)),
        _ => Err(SettingsError::NotAnObject {
            path: path.to_path_buf(),
        }),
    }
}

/// Writes `map` to `path`: a verbatim-bytes backup of any existing file
/// first, then a temp file in the same directory, fsynced, then renamed
/// over the original. Never truncates `path` in place. Returns the
/// backup path, when one was written.
fn write_object_atomically(
    path: &Path,
    map: &Map<String, Value>,
) -> Result<Option<PathBuf>, SettingsError> {
    let io_err = |source: std::io::Error| SettingsError::Io {
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
    let json_text =
        serde_json::to_string_pretty(&Value::Object(map.clone())).map_err(|source| {
            SettingsError::Parse {
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
    let mut root = read_object(path)?.unwrap_or_default();
    let binary = binary.display().to_string();
    let mut hooks_added = 0usize;

    let hooks_value = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()));
    let hooks_obj = as_object_or_replace(hooks_value);

    for (event, subcommand) in
        HOOK_EVENTS
            .iter()
            .zip(["hook user-prompt-submit", "hook post-tool-use", "hook stop"])
    {
        let command = format!("{binary} {subcommand}");
        let entries = hooks_obj
            .entry((*event).to_string())
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

    let backup_path = write_object_atomically(path, &root)?;
    Ok(Applied {
        hooks_added,
        statusline_added,
        statusline_conflict,
        backup_path,
    })
}

fn as_object_or_replace(value: &mut Value) -> &mut Map<String, Value> {
    if !value.is_object() {
        *value = Value::Object(Map::new());
    }
    value.as_object_mut().expect("just ensured object")
}

fn as_array_or_replace(value: &mut Value) -> &mut Vec<Value> {
    if !value.is_array() {
        *value = Value::Array(Vec::new());
    }
    value.as_array_mut().expect("just ensured array")
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
    let Some(mut root) = read_object(path)? else {
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

    let backup_path = write_object_atomically(path, &root)?;
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
    let Ok(Some(root)) = read_object(path) else {
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
    fn is_loopback_base_url_accepts_only_loopback_hosts() {
        assert!(is_loopback_base_url("http://127.0.0.1:8787"));
        assert!(is_loopback_base_url("http://localhost:8787"));
        assert!(!is_loopback_base_url("https://api.example.com"));
        assert!(!is_loopback_base_url("http://10.0.0.5:8787"));
    }
}
