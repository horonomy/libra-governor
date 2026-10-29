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
const OWNED_COMMAND_SUFFIXES: [&str; 8] = [
    "hook user-prompt-submit",
    "hook post-tool-use",
    "hook stop",
    "hook pre-tool-use",
    "hook subagent-stop",
    "hook session-end",
    "statusline",
    "gateway token",
];

/// The three legacy hooks, in the order [`Inspection::hooks_wired`] reports
/// them. The set is unchanged; since HORO-2217 an event counts as wired only
/// when our command is in an all-tools group (see `inspect`).
const HOOK_EVENTS: [&str; 3] = ["UserPromptSubmit", "PostToolUse", "Stop"];

/// The shared-budget hooks (HORO-2216, ADR-0026 S7), reported separately in
/// [`Inspection::shared_budget_hooks_wired`]. Each is a no-op unless
/// `<state_dir>/shared-budget.json` exists.
const SHARED_BUDGET_HOOK_EVENTS: [&str; 3] = ["PreToolUse", "SubagentStop", "SessionEnd"];

/// Hook subcommand per event, legacy three first, then the shared-budget
/// three (same order as the two event arrays above).
const HOOK_SUBCOMMANDS: [(&str, &str); 6] = [
    ("UserPromptSubmit", "hook user-prompt-submit"),
    ("PostToolUse", "hook post-tool-use"),
    ("Stop", "hook stop"),
    ("PreToolUse", "hook pre-tool-use"),
    ("SubagentStop", "hook subagent-stop"),
    ("SessionEnd", "hook session-end"),
];

/// Seconds Claude Code may let the settlement hooks run. A `SessionEnd` hook
/// otherwise gets a very short default window, and settling reads a
/// transcript; the hook itself bounds its own work well inside this.
const SETTLEMENT_HOOK_TIMEOUT_SECS: u64 = 10;

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("could not determine home directory (HOME env var unset)")]
    NoHomeDir,
    #[error(
        "refusing to wire hooks to {binary}: the path contains whitespace or a shell \
         metacharacter, and Claude Code runs a hook command through a shell, so the hook \
         would silently not run and prompts would pass ungated (nothing was changed) — \
         install the binary to a plain path such as ~/.cargo/bin and re-run"
    )]
    UnsafeBinaryPath { binary: PathBuf },
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
    /// ADR-0014 (HORO-1380 S4c): could not acquire the cross-process
    /// write lock on `path`'s sidecar within the 5000ms budget, or the
    /// attempt failed outright (e.g. `ENOTSUP`/`EACCES`/`EROFS`). Zero
    /// bytes are ever written to `path` when this is returned — nothing
    /// in the read/plan/write sequence ran. See
    /// [`crate::write_lock::LockFailure`] for the class-A/class-B
    /// distinction this wraps.
    #[error("{path}: {source}")]
    WriteLockUnavailable {
        path: PathBuf,
        source: crate::write_lock::LockFailure,
    },
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

/// Ensures `path`'s parent directory exists, then acquires the
/// cross-process write lock on `path`'s sidecar, then runs `body` while
/// holding it (ADR-0014 section 3, steps 1-4 before `body`'s
/// read/plan/re-hash/write, step 10 on return). Used by both `apply` and
/// `remove` so the lock always covers the entire read-through-write
/// sequence, never just the write.
fn with_write_lock<T>(
    path: &Path,
    body: impl FnOnce() -> Result<T, SettingsError>,
) -> Result<T, SettingsError> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).map_err(|source| SettingsError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let _guard =
        crate::write_lock::acquire(path).map_err(|source| SettingsError::WriteLockUnavailable {
            path: path.to_path_buf(),
            source,
        })?;
    body()
}

/// A hook group with no matcher, `""` or `"*"` applies to every tool.
fn matcher_is_all_tools(group: &Value) -> bool {
    match group.get("matcher") {
        None | Some(Value::Null) => true,
        Some(Value::String(m)) => m.is_empty() || m == "*",
        Some(_) => false,
    }
}

/// Ours only if the command is `<path>/libra-governor <exact suffix>`: the
/// text before the suffix must be a path whose final component is exactly
/// `libra-governor`. A merely similar name (`libra-governor-tools/x`,
/// `my-libra-governor-wrapper`) or a directory called `libra-governor`
/// holding some other binary is a foreign command and is never touched.
fn command_is_ours(command: &str) -> bool {
    OWNED_COMMAND_SUFFIXES.iter().any(|suffix| {
        command
            .strip_suffix(suffix)
            .map(str::trim_end)
            .is_some_and(|binary| {
                Path::new(binary)
                    .file_name()
                    .is_some_and(|name| name == "libra-governor")
            })
    })
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
/// and, when requested, `statusLine`, pointing at `binary`, to `path`. Idempotent:
/// an already-present Governor-owned entry is not duplicated. Never touches
/// any other key. `binary` should be an absolute path (a relative path
/// in `settings.json` would only work when Claude Code happens to be
/// launched from the right working directory).
pub fn apply(path: &Path, binary: &Path) -> Result<Applied, SettingsError> {
    reject_unsafe_binary(binary)?;
    with_write_lock(path, || apply_locked(path, binary, true))
}

/// Adds only this integration's three hooks, leaving `statusLine` untouched
/// whether it is absent, foreign, or has an unrecognized shape.
pub fn apply_hooks_only(path: &Path, binary: &Path) -> Result<Applied, SettingsError> {
    reject_unsafe_binary(binary)?;
    with_write_lock(path, || apply_locked(path, binary, false))
}

/// Whether `text`, written unquoted into a command a shell runs, would be
/// split or interpreted instead of naming one program.
fn has_shell_unsafe_char(text: &str) -> bool {
    text.chars()
        .any(|c| c.is_whitespace() || "\"'\\$`;&|<>()*?[]{}!#^".contains(c))
}

/// A command `command_is_ours` recognises whose binary part the shell would
/// split (an install from before PR 242). Ownership is unchanged; this only
/// says the command cannot run, so `doctor` must not call it healthy.
fn command_is_unusable(command: &str) -> bool {
    command_is_ours(command)
        && OWNED_COMMAND_SUFFIXES.iter().any(|suffix| {
            command
                .strip_suffix(suffix)
                .map(str::trim_end)
                .is_some_and(has_shell_unsafe_char)
        })
}

/// The hook command is `{binary} {subcommand}` run by a shell, unquoted.
fn reject_unsafe_binary(binary: &Path) -> Result<(), SettingsError> {
    if has_shell_unsafe_char(&binary.to_string_lossy()) {
        return Err(SettingsError::UnsafeBinaryPath {
            binary: binary.to_path_buf(),
        });
    }
    Ok(())
}

fn apply_locked(
    path: &Path,
    binary: &Path,
    install_statusline: bool,
) -> Result<Applied, SettingsError> {
    let (existing, digest) = read_object(path)?;
    let mut root = existing.unwrap_or_default();
    let binary = binary.display().to_string();
    let mut hooks_added = 0usize;

    let hooks_obj = hooks_object_mut(&mut root, path)?;

    for (event, subcommand) in HOOK_SUBCOMMANDS {
        let command = format!("{binary} {subcommand}");
        let entries = hooks_obj
            .entry(event.to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        let entries_arr =
            entries
                .as_array_mut()
                .ok_or_else(|| SettingsError::HookEventNotAnArray {
                    path: path.to_path_buf(),
                    event: event.to_string(),
                })?;

        let wants_timeout = matches!(event, "SubagentStop" | "SessionEnd");
        // Only an entry in a group that applies to every tool counts as
        // installed: our command under a user's `matcher: "Bash"` group
        // fires for Bash only. A present entry that lacks the settlement
        // timeout gets it added.
        let mut already_present = false;
        let mut upgraded = false;
        for group in entries_arr.iter_mut() {
            if !matcher_is_all_tools(group) {
                continue;
            }
            let Some(inner) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
                continue;
            };
            for h in inner.iter_mut() {
                if h.get("command").and_then(Value::as_str) != Some(command.as_str()) {
                    continue;
                }
                already_present = true;
                if wants_timeout && h.get("timeout").is_none() {
                    if let Some(obj) = h.as_object_mut() {
                        obj.insert(
                            "timeout".to_string(),
                            serde_json::json!(SETTLEMENT_HOOK_TIMEOUT_SECS),
                        );
                        upgraded = true;
                    }
                }
            }
        }
        if upgraded {
            hooks_added += 1;
        }
        if !already_present {
            let mut hook = serde_json::json!({ "type": "command", "command": command });
            if wants_timeout {
                hook["timeout"] = serde_json::json!(SETTLEMENT_HOOK_TIMEOUT_SECS);
            }
            entries_arr.push(serde_json::json!({ "hooks": [hook] }));
            hooks_added += 1;
        }
    }

    let (statusline_added, statusline_conflict) = if install_statusline {
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
        // statusline integration. Only an absent or already-ours `statusLine` is written.
        let statusline_conflict = existing_statusline_command.is_some() && !statusline_already_ours;
        let statusline_added = !statusline_already_ours && !statusline_conflict;
        if statusline_added {
            root.insert(
                "statusLine".to_string(),
                serde_json::json!({ "type": "command", "command": statusline_command }),
            );
        }
        (statusline_added, statusline_conflict)
    } else {
        (false, false)
    };

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
    with_write_lock(path, || remove_locked(path))
}

fn remove_locked(path: &Path) -> Result<Removed, SettingsError> {
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
                // Unlike `codex_hooks_file::remove_locked` (HORO-1876),
                // this unconditional `Vec::remove` is intentional, not a
                // bug: Claude Code's hook trust is not keyed by array
                // position. Trust is granted at a folder/session scope
                // (a workspace-trust dialog gates hooks from every
                // settings file for that folder, including the
                // user-level `~/.claude/settings.json` this module edits,
                // in interactive sessions) — there is no documented or
                // observed `(event, group_idx, handler_idx)`-keyed trust state for
                // Claude Code to invalidate by shifting a later foreign
                // group's index, the way Codex's `[hooks.state]` has
                // (HORO-1877 investigation: official Claude Code hooks
                // docs describe file/workspace-scoped trust only, and a
                // local `~/.claude.json` has no per-hook-entry state —
                // only aggregate duration metrics). Do not port Codex's
                // tail-check/tombstone fix here without new evidence that
                // this has changed.
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
    /// [PreToolUse, SubagentStop, SessionEnd] (HORO-2216). Reported apart
    /// from `hooks_wired` so an install that predates them still reads as
    /// the legacy three wired.
    pub shared_budget_hooks_wired: [bool; 3],
    pub statusline_wired: bool,
    pub api_key_helper_wired: bool,
    pub base_url_present: bool,
    pub base_url_is_loopback: bool,
    /// Recognised Governor commands (hooks and statusLine) whose binary
    /// path would be split by the shell that runs them, so they exit 127
    /// before Libra starts. Ownership recognition (`command_is_ours`) is
    /// deliberately unchanged: this only says the command cannot run.
    pub unusable_commands: usize,
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

    let hooks_obj = root.get("hooks").and_then(Value::as_object);
    let wired = |event: &str| -> bool {
        hooks_obj
            .and_then(|hooks| hooks.get(event))
            .and_then(Value::as_array)
            .map(|entries| {
                // Only a group that applies to every tool counts: our command
                // under a user's `matcher: "Bash"` group fires for Bash only
                // (the same rule `apply` uses to decide it is installed).
                entries
                    .iter()
                    .filter(|g| matcher_is_all_tools(g))
                    .any(|matcher| {
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
            .unwrap_or(false)
    };
    let hooks_wired = HOOK_EVENTS.map(wired);
    let shared_budget_hooks_wired = SHARED_BUDGET_HOOK_EVENTS.map(wired);

    // Every recognised Governor command in an all-tools group, plus the
    // statusLine, that the shell would split.
    let mut unusable_commands = hooks_obj
        .map(|hooks| {
            hooks
                .values()
                .filter_map(Value::as_array)
                .flatten()
                .filter(|group| matcher_is_all_tools(group))
                .filter_map(|group| group.get("hooks").and_then(Value::as_array))
                .flatten()
                .filter_map(|h| h.get("command").and_then(Value::as_str))
                .filter(|command| command_is_unusable(command))
                .count()
        })
        .unwrap_or(0);

    let statusline_wired = root
        .get("statusLine")
        .and_then(|v| v.get("command"))
        .and_then(Value::as_str)
        .map(command_is_ours)
        .unwrap_or(false);

    unusable_commands += usize::from(
        root.get("statusLine")
            .and_then(|v| v.get("command"))
            .and_then(Value::as_str)
            .is_some_and(command_is_unusable),
    );

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
        shared_budget_hooks_wired,
        statusline_wired,
        api_key_helper_wired,
        base_url_present,
        base_url_is_loopback,
        unusable_commands,
    }
}

/// Whether the `env` block of `path` has a key named `key`. Presence only:
/// the value is never read out of this function, so a doctor report cannot
/// leak it. A missing or unparsable file has no keys.
pub fn env_key_present(path: &Path, key: &str) -> bool {
    let Ok((Some(root), _)) = read_object(path) else {
        return false;
    };
    root.get("env")
        .and_then(Value::as_object)
        .is_some_and(|env| env.contains_key(key))
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
        assert_eq!(applied.hooks_added, 6);
        assert!(applied.statusline_added);
        assert!(applied.backup_path.is_none(), "nothing existed to back up");

        let inspection = inspect(&path);
        assert_eq!(inspection.hooks_wired, [true, true, true]);
        assert!(inspection.statusline_wired);
    }

    #[test]
    fn apply_refuses_a_binary_path_the_hook_shell_would_split_and_writes_nothing() {
        // Claude Code runs a hook command through a shell. An unquoted path
        // with a space runs the wrong program (exit 127, a non-blocking
        // error), so every prompt would pass ungated while `doctor` still
        // saw the hooks as wired.
        let dir = tempfile::tempdir().unwrap();
        for bad in [
            "/tmp/libra gov/libra-governor",
            "/tmp/a;b/libra-governor",
            "/tmp/$HOME/libra-governor",
            "/tmp/a\"b/libra-governor",
        ] {
            let path = dir.path().join("settings.json");
            let binary = std::path::PathBuf::from(bad);
            for result in [apply(&path, &binary), apply_hooks_only(&path, &binary)] {
                assert!(
                    matches!(result, Err(SettingsError::UnsafeBinaryPath { .. })),
                    "{bad}: {result:?}"
                );
            }
            assert!(!path.exists(), "{bad}: nothing may be written");
        }
    }

    #[test]
    fn inspect_flags_a_recognised_command_whose_path_the_shell_would_split() {
        // A settings file written by a version that did not refuse such a
        // path: still recognised as ours (so uninstall removes it), but it
        // exits 127 before Libra runs, which `inspect` must not hide.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        apply(&path, &binary()).unwrap();
        assert_eq!(inspect(&path).unusable_commands, 0, "valid install");
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replace("/opt/libra/bin", "/opt/legacy path")).unwrap();
        let inspection = inspect(&path);
        assert_eq!(inspection.hooks_wired, [true, true, true], "still ours");
        assert_eq!(inspection.shared_budget_hooks_wired, [true, true, true]);
        assert_eq!(
            inspection.unusable_commands, 7,
            "six hooks and the statusLine"
        );
    }

    #[test]
    fn inspect_never_counts_or_wires_through_a_corrupt_or_unsupported_shape() {
        // Static validation reads JSON only; nothing is executed. Shapes that
        // are corrupt, unsupported or not Governor's own must neither panic
        // nor be reported as wired, and must not inflate the unusable count.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let unsafe_cmd = "/opt/legacy path/libra-governor hook stop";
        let cases: Vec<(&str, String)> = vec![
            ("unparsable", "{ not json".to_string()),
            ("top-level array", "[1,2,3]".to_string()),
            ("hooks is a string", r#"{"hooks":"x"}"#.to_string()),
            ("event is not an array", r#"{"hooks":{"Stop":{"a":1}}}"#.to_string()),
            ("group without hooks", r#"{"hooks":{"Stop":[{"matcher":""}]}}"#.to_string()),
            ("command is not a string", r#"{"hooks":{"Stop":[{"hooks":[{"command":7}]}]}}"#.to_string()),
            (
                "non-all-tools group",
                format!(r#"{{"hooks":{{"Stop":[{{"matcher":"Bash","hooks":[{{"command":"{unsafe_cmd}"}}]}}]}}}}"#),
            ),
            (
                "hand-quoted command (not recognised as ours)",
                r#"{"hooks":{"Stop":[{"hooks":[{"command":"\"/opt/legacy path/libra-governor\" hook stop"}]}]}}"#.to_string(),
            ),
            (
                "lookalike binary name",
                r#"{"hooks":{"Stop":[{"hooks":[{"command":"/opt/a b/libra-governor-tools hook stop"}]}]}}"#.to_string(),
            ),
            (
                "foreign command with a space",
                r#"{"hooks":{"Stop":[{"hooks":[{"command":"/usr/local/my tools/lint --all"}]}]}}"#.to_string(),
            ),
        ];
        for (name, body) in cases {
            std::fs::write(&path, body).unwrap();
            let inspection = inspect(&path);
            assert_eq!(inspection.unusable_commands, 0, "{name}");
            assert_eq!(inspection.hooks_wired, [false; 3], "{name}");
            assert_eq!(inspection.shared_budget_hooks_wired, [false; 3], "{name}");
        }
        // An unsafe legacy statusLine alone is counted, and is not a hook.
        std::fs::write(
            &path,
            r#"{"statusLine":{"type":"command","command":"/opt/legacy path/libra-governor statusline"}}"#,
        )
        .unwrap();
        let inspection = inspect(&path);
        assert_eq!(inspection.unusable_commands, 1);
        assert_eq!(inspection.hooks_wired, [false; 3]);
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
    fn apply_hooks_only_leaves_an_absent_statusline_absent_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");

        let applied = apply_hooks_only(&path, &binary()).unwrap();
        assert_eq!(applied.hooks_added, 6);
        assert!(!applied.statusline_added);
        assert!(!applied.statusline_conflict);

        let value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(value.get("statusLine").is_none());
        assert_eq!(inspect(&path).hooks_wired, [true, true, true]);

        let second = apply_hooks_only(&path, &binary()).unwrap();
        assert_eq!(second.hooks_added, 0);
        assert!(!second.statusline_added);
    }

    #[test]
    fn apply_hooks_only_preserves_existing_statusline_metadata_and_unknown_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let seed = serde_json::json!({
            "statusLine": { "type": "command", "command": "/usr/local/bin/foreign", "custom": true },
            "futureSetting": { "kept": [1, 2, 3] },
            "hooks": { "PostToolUse": [{ "matcher": "Read", "hooks": [
                { "type": "command", "command": "/usr/local/bin/foreign-hook" }
            ] }] }
        });
        std::fs::write(&path, serde_json::to_vec(&seed).unwrap()).unwrap();

        let applied = apply_hooks_only(&path, &binary()).unwrap();
        assert_eq!(applied.hooks_added, 6);
        assert!(!applied.statusline_added);
        assert!(!applied.statusline_conflict);

        let value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["statusLine"], seed["statusLine"]);
        assert_eq!(value["futureSetting"], seed["futureSetting"]);
        assert_eq!(
            value["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
            "/usr/local/bin/foreign-hook"
        );

        apply_hooks_only(&path, &binary()).unwrap();
        let after: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(after["statusLine"], seed["statusLine"]);
        assert_eq!(
            after["hooks"]["UserPromptSubmit"].as_array().unwrap().len(),
            1,
            "rerunning must not duplicate Libra hooks"
        );
    }

    #[test]
    fn apply_hooks_only_preserves_malformed_statusline_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let malformed = serde_json::json!(["unexpected", { "future": true }]);
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({ "statusLine": malformed })).unwrap(),
        )
        .unwrap();

        let applied = apply_hooks_only(&path, &binary()).unwrap();
        assert_eq!(applied.hooks_added, 6);
        assert!(!applied.statusline_added);
        assert!(!applied.statusline_conflict);

        let value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["statusLine"], malformed);
    }

    #[test]
    fn apply_then_remove_returns_to_a_value_equal_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        apply(&path, &binary()).unwrap();
        let removed = remove(&path).unwrap();
        assert_eq!(removed.hook_commands_removed, 6);
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

        // No stray temp/backup file left behind by the aborted attempt —
        // only the config file itself and the lock sidecar (ADR-0014:
        // the lock is acquired, and the sidecar created, before the
        // parse error is even reached).
        let mut entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        entries.sort();
        let mut expected = vec![
            std::ffi::OsString::from("settings.json"),
            std::ffi::OsString::from("settings.json.horonom-write.lock"),
        ];
        expected.sort();
        assert_eq!(entries, expected);
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

    fn read_json(path: &Path) -> Value {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    fn commands(value: &Value, event: &str) -> Vec<String> {
        value["hooks"][event]
            .as_array()
            .map(|groups| {
                groups
                    .iter()
                    .flat_map(|g| g["hooks"].as_array().unwrap().iter())
                    .map(|h| h["command"].as_str().unwrap().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn apply_wires_the_shared_budget_hooks_beside_the_legacy_three() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        apply_hooks_only(&path, &binary()).unwrap();

        let value = read_json(&path);
        for (event, sub) in [
            ("UserPromptSubmit", "hook user-prompt-submit"),
            ("PostToolUse", "hook post-tool-use"),
            ("Stop", "hook stop"),
            ("PreToolUse", "hook pre-tool-use"),
            ("SubagentStop", "hook subagent-stop"),
            ("SessionEnd", "hook session-end"),
        ] {
            assert_eq!(
                commands(&value, event),
                vec![format!("/opt/libra/bin/libra-governor {sub}")],
                "{event}"
            );
        }
        // The settlement hooks ask for a window long enough to settle; the
        // legacy and gate hooks keep their historical entry shape.
        assert_eq!(value["hooks"]["SubagentStop"][0]["hooks"][0]["timeout"], 10);
        assert_eq!(value["hooks"]["SessionEnd"][0]["hooks"][0]["timeout"], 10);
        for event in ["UserPromptSubmit", "PostToolUse", "Stop", "PreToolUse"] {
            assert!(value["hooks"][event][0]["hooks"][0]
                .get("timeout")
                .is_none());
        }
        let inspection = inspect(&path);
        assert_eq!(inspection.hooks_wired, [true, true, true]);
        assert_eq!(inspection.shared_budget_hooks_wired, [true, true, true]);
    }

    #[test]
    fn apply_is_idempotent_for_the_shared_budget_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        apply_hooks_only(&path, &binary()).unwrap();
        let first = std::fs::read(&path).unwrap();

        let second = apply_hooks_only(&path, &binary()).unwrap();
        assert_eq!(second.hooks_added, 0);
        assert_eq!(std::fs::read(&path).unwrap(), first, "no byte may change");
        for event in ["PreToolUse", "SubagentStop", "SessionEnd"] {
            assert_eq!(
                read_json(&path)["hooks"][event].as_array().unwrap().len(),
                1
            );
        }
    }

    #[test]
    fn upgrading_a_legacy_install_adds_only_the_new_hooks_and_keeps_foreign_ones() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let seed = serde_json::json!({
            "futureSetting": { "kept": [1, 2, 3] },
            "hooks": {
                "UserPromptSubmit": [{ "hooks": [
                    { "type": "command", "command": "/opt/libra/bin/libra-governor hook user-prompt-submit" }
                ] }],
                "PostToolUse": [{ "hooks": [
                    { "type": "command", "command": "/opt/libra/bin/libra-governor hook post-tool-use" }
                ] }],
                "Stop": [{ "hooks": [
                    { "type": "command", "command": "/opt/libra/bin/libra-governor hook stop" }
                ] }],
                "PreToolUse": [{ "matcher": "Bash", "hooks": [
                    { "type": "command", "command": "/usr/local/bin/foreign-guard check", "timeout": 3 }
                ] }],
                "SubagentStop": [{ "hooks": [
                    { "type": "command", "command": "/usr/local/bin/their-session-end hook subagent-stop" }
                ] }],
                "FutureEvent": [{ "hooks": [{ "type": "command", "command": "/x/y" }] }]
            }
        });
        std::fs::write(&path, serde_json::to_vec(&seed).unwrap()).unwrap();
        assert_eq!(
            inspect(&path).shared_budget_hooks_wired,
            [false, false, false],
            "a foreign hook with a lookalike suffix is not ours"
        );

        let applied = apply_hooks_only(&path, &binary()).unwrap();
        assert_eq!(applied.hooks_added, 3);

        let value = read_json(&path);
        assert_eq!(value["futureSetting"], seed["futureSetting"]);
        assert_eq!(value["hooks"]["FutureEvent"], seed["hooks"]["FutureEvent"]);
        for event in ["UserPromptSubmit", "PostToolUse", "Stop"] {
            assert_eq!(value["hooks"][event], seed["hooks"][event], "{event}");
        }
        // Foreign groups stay first and byte-equal; ours is appended.
        assert_eq!(
            value["hooks"]["PreToolUse"][0],
            seed["hooks"]["PreToolUse"][0]
        );
        assert_eq!(
            value["hooks"]["SubagentStop"][0],
            seed["hooks"]["SubagentStop"][0]
        );
        assert_eq!(
            commands(&value, "PreToolUse"),
            vec![
                "/usr/local/bin/foreign-guard check".to_string(),
                "/opt/libra/bin/libra-governor hook pre-tool-use".to_string()
            ]
        );
        assert_eq!(
            commands(&value, "SubagentStop")[1],
            "/opt/libra/bin/libra-governor hook subagent-stop"
        );
        assert_eq!(inspect(&path).shared_budget_hooks_wired, [true, true, true]);
    }

    #[test]
    fn remove_takes_the_shared_budget_hooks_and_leaves_lookalikes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let seed = serde_json::json!({
            "hooks": {
                "SessionEnd": [{ "hooks": [
                    { "type": "command", "command": "/usr/local/bin/other-tool hook session-end" },
                    { "type": "command", "command": "/opt/libra/bin/libra-governor hook session-end", "timeout": 10 }
                ] }],
                "PreToolUse": [{ "hooks": [
                    { "type": "command", "command": "/usr/local/bin/foreign-guard check" },
                    { "type": "command", "command": "/opt/libra/bin/libra-governor hook pre-tool-use" }
                ] }],
                "SubagentStop": [{ "hooks": [
                    { "type": "command", "command": "/opt/libra/bin/libra-governor hook subagent-stop", "timeout": 10 }
                ] }]
            }
        });
        std::fs::write(&path, serde_json::to_vec(&seed).unwrap()).unwrap();

        let removed = remove(&path).unwrap();
        assert_eq!(removed.hook_commands_removed, 3);

        let value = read_json(&path);
        assert_eq!(
            commands(&value, "SessionEnd"),
            vec!["/usr/local/bin/other-tool hook session-end".to_string()]
        );
        assert_eq!(
            commands(&value, "PreToolUse"),
            vec!["/usr/local/bin/foreign-guard check".to_string()]
        );
        assert!(
            value["hooks"].get("SubagentStop").is_none(),
            "an event our removal emptied is dropped"
        );
        assert_eq!(
            inspect(&path).shared_budget_hooks_wired,
            [false, false, false]
        );
    }

    #[test]
    fn env_key_presence_never_depends_on_or_exposes_the_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        assert!(!env_key_present(&path, "K"), "missing file");
        std::fs::write(&path, br#"{"env":{"K":"anything","OTHER":"x"}}"#).unwrap();
        assert!(env_key_present(&path, "K"));
        assert!(!env_key_present(&path, "ABSENT"));
        std::fs::write(&path, b"not json").unwrap();
        assert!(!env_key_present(&path, "K"), "unparsable file");
    }

    #[test]
    fn our_command_under_a_foreign_matcher_group_is_not_reported_wired() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let ours = |sub: &str| format!("/opt/libra/bin/libra-governor {sub}");
        let group = |matcher: Option<&str>, command: String| {
            let mut g = serde_json::json!({ "hooks": [{ "type": "command", "command": command }] });
            if let Some(m) = matcher {
                g["matcher"] = serde_json::json!(m);
            }
            g
        };
        let hooks = serde_json::json!({
            "PreToolUse": [group(Some("Bash"), ours("hook pre-tool-use"))],
            "PostToolUse": [group(Some("Edit|Write"), ours("hook post-tool-use"))],
            "SubagentStop": [group(Some("*"), ours("hook subagent-stop"))],
            "SessionEnd": [group(Some(""), ours("hook session-end"))],
            "Stop": [group(None, ours("hook stop"))],
        });
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({ "hooks": hooks })).unwrap(),
        )
        .unwrap();
        let inspection = inspect(&path);
        assert_eq!(inspection.shared_budget_hooks_wired, [false, true, true]);
        assert_eq!(inspection.hooks_wired, [false, false, true]);
    }

    #[test]
    fn lookalike_foreign_commands_survive_uninstall_and_are_not_reported_wired() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let foreign = [
            ("Stop", "/opt/libra-governor-tools/x hook stop"),
            ("SessionEnd", "my-libra-governor-wrapper hook session-end"),
            ("PreToolUse", "~/libra-governor/bin/other hook pre-tool-use"),
            (
                "SubagentStop",
                "/usr/bin/libra-governorx hook subagent-stop",
            ),
            (
                "PostToolUse",
                "/opt/libra-governor/bin/helper hook post-tool-use",
            ),
        ];
        let mut hooks = serde_json::Map::new();
        for (event, command) in foreign {
            hooks.insert(
                event.to_string(),
                serde_json::json!([{ "hooks": [{ "type": "command", "command": command }] }]),
            );
        }
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({ "hooks": hooks })).unwrap(),
        )
        .unwrap();
        let before = read_json(&path);
        assert_eq!(inspect(&path).hooks_wired, [false; 3]);
        assert_eq!(inspect(&path).shared_budget_hooks_wired, [false; 3]);

        // Install adds our own six beside them; uninstall removes only those.
        apply_hooks_only(&path, &binary()).unwrap();
        let removed = remove(&path).unwrap();
        assert_eq!(removed.hook_commands_removed, 6);
        assert_eq!(read_json(&path), before, "every foreign command survives");
    }

    #[test]
    fn a_legacy_installs_own_entries_are_still_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let seed = serde_json::json!({ "hooks": {
            "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": "/Users/a b/.cargo/bin/libra-governor hook user-prompt-submit" }] }],
            "PostToolUse": [{ "hooks": [{ "type": "command", "command": "/opt/libra/bin/libra-governor hook post-tool-use" }] }],
            "Stop": [{ "hooks": [{ "type": "command", "command": "libra-governor hook stop" }] }]
        }, "statusLine": { "type": "command", "command": "/opt/libra/bin/libra-governor statusline" } });
        std::fs::write(&path, serde_json::to_vec(&seed).unwrap()).unwrap();
        let removed = remove(&path).unwrap();
        assert_eq!(removed.hook_commands_removed, 3);
        assert!(removed.statusline_removed);
    }

    #[test]
    fn our_command_under_a_foreign_matcher_group_is_not_installed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let ours = "/opt/libra/bin/libra-governor hook pre-tool-use";
        let seed = serde_json::json!({ "hooks": { "PreToolUse": [
            { "matcher": "Bash", "hooks": [{ "type": "command", "command": ours }] }
        ] } });
        std::fs::write(&path, serde_json::to_vec(&seed).unwrap()).unwrap();

        apply_hooks_only(&path, &binary()).unwrap();
        let value = read_json(&path);
        let groups = value["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(groups.len(), 2, "an all-tools entry of our own is added");
        assert_eq!(groups[0], seed["hooks"]["PreToolUse"][0]);
        assert!(groups[1].get("matcher").is_none());
        assert_eq!(groups[1]["hooks"][0]["command"], ours);

        // `""` and `"*"` do count as all-tools: no second entry.
        for matcher in ["", "*"] {
            let seed = serde_json::json!({ "hooks": { "PreToolUse": [
                { "matcher": matcher, "hooks": [{ "type": "command", "command": ours }] }
            ] } });
            std::fs::write(&path, serde_json::to_vec(&seed).unwrap()).unwrap();
            apply_hooks_only(&path, &binary()).unwrap();
            assert_eq!(
                read_json(&path)["hooks"]["PreToolUse"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
        }
    }

    #[test]
    fn reinstall_adds_the_settlement_timeout_to_an_entry_that_lacks_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let seed = serde_json::json!({ "hooks": {
            "SubagentStop": [{ "hooks": [{ "type": "command", "command": "/opt/libra/bin/libra-governor hook subagent-stop" }] }],
            "SessionEnd": [{ "hooks": [{ "type": "command", "command": "/opt/libra/bin/libra-governor hook session-end", "timeout": 3 }] }]
        } });
        std::fs::write(&path, serde_json::to_vec(&seed).unwrap()).unwrap();

        let applied = apply_hooks_only(&path, &binary()).unwrap();
        let value = read_json(&path);
        assert_eq!(value["hooks"]["SubagentStop"][0]["hooks"][0]["timeout"], 10);
        assert_eq!(
            value["hooks"]["SessionEnd"][0]["hooks"][0]["timeout"], 3,
            "a timeout the user chose is never overwritten"
        );
        assert_eq!(value["hooks"]["SubagentStop"].as_array().unwrap().len(), 1);
        assert_eq!(
            applied.hooks_added, 5,
            "4 added (3 legacy + PreToolUse), SubagentStop upgraded"
        );
        assert_eq!(apply_hooks_only(&path, &binary()).unwrap().hooks_added, 0);
    }

    /// HORO-2303 item 1: a hand-quoted binary path is still our command, so
    /// uninstall removes it and install does not add a duplicate beside it.
    /// RED before the quote-aware `command_is_ours` (the trailing quote made
    /// the file name `libra-governor'`); NOT_RUN.
    #[test]
    fn a_hand_quoted_binary_path_is_recognised_as_ours() {
        assert!(command_is_ours("'/opt/my tools/libra-governor' hook stop"));
        assert!(command_is_ours(
            "\"/opt/my tools/libra-governor\" hook stop"
        ));
    }

    /// Quoting never makes a lookalike ours: the unquoted path's final
    /// component must still be exactly `libra-governor`, and the quotes must
    /// match.
    #[test]
    fn quoting_does_not_make_a_lookalike_or_a_mismatched_quote_ours() {
        assert!(!command_is_ours("'/opt/libra-governor-tools/x' hook stop"));
        assert!(!command_is_ours("'/opt/libra-governor\" hook stop"));
    }
}
