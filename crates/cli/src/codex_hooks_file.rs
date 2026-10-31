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
//! 5. Immediately before that write, the file is re-read and its bytes
//!    re-hashed, then compared against what this process read at the
//!    start of the operation. If they differ, some other writer changed
//!    the file in between and the whole operation aborts with
//!    [`CodexHooksError::ConcurrentModification`] — no backup, no temp
//!    file, no write at all, so the racing writer's content survives
//!    exactly as it is rather than being silently clobbered by an edit
//!    computed from stale content (HORO-1380 S2b / HORO-998
//!    `CONCURRENT_CHANGE_DOES_NOT_CLOBBER`). See
//!    `claude_settings`'s module docs for why a check-then-rename, and
//!    not a lock, is the strongest honest guarantee available here.
//!
//! # File shape — verified against a real local Codex CLI install
//! (HORO-1157)
//!
//! `hooks.json`'s **top-level** object accepts exactly two fields:
//! `description` (optional, a free-text string) and `hooks` (an object
//! keyed by event name). Codex's own top-level schema is strict — a real
//! installed Codex CLI (v0.154.0, verified locally during HORO-1157)
//! rejects *any other top-level key* with `failed to parse hooks config:
//! unknown field "<key>", expected "description" or "hooks"` and, on that
//! parse failure, silently disables every hook in the file (fail-open on
//! Codex's side — it warns, but a real prompt still proceeds without any
//! hook running). An earlier draft of this module assumed the same flat,
//! event-keyed-at-the-top shape Claude Code's `settings.json` `hooks`
//! table uses; that was wrong, and was caught by actually invoking the
//! real `codex` binary (`codex exec --dangerously-bypass-hook-trust`)
//! against a real, sandboxed `$CODEX_HOME` and observing the parse
//! warning. The **shape of one event's entries** — the array of
//! `{"hooks": [{"type": "command", "command": "...", ...}]}` matcher
//! groups — matches Claude Code's shape exactly and was also confirmed
//! by the same local run (`hook: UserPromptSubmit` / `hook:
//! UserPromptSubmit Completed` printed by the real `codex` binary, and
//! the real daemon's `ledger.sqlite3` recording a `session_tasks` row
//! keyed by Codex's own real session id). See
//! `docs/adr/0004-agent-adapter-contract.md` for the full evidence trail.
//!
//! `description` is never written or removed by this module — it is not
//! Governor-owned, and a user (or another tool) may have set their own.
//!
//! # Ownership predicate
//!
//! A hook command string is "ours" only if it both contains
//! `libra-governor` *and* ends with one of the exact `codex-hook`
//! subcommand invocations this integration documents
//! (`integrations/codex/README.md`) — see [`OWNED_COMMAND_SUFFIXES`].
//! [`command_is_ours`] (which also accepts a legacy unquoted path with
//! spaces) decides what `remove`, `inspect` and `wired_commands` act on.
//!
//! `apply` rewrites a command, so it uses the stricter
//! [`command_is_ours_for`], which has no legacy leniency (HORO-2303). Both
//! use `claude_settings::command_is_ours_with_suffix`, the one shared rule:
//! the text before the event's exact suffix must be ONE path token whose
//! final component is exactly `libra-governor`. A lookalike, a wrapped
//! command (`env X=1 ...`, `cd x && ...`, a pipeline) or a trailing-`/` path
//! is never ours.
//!
//! # Trust gate is out of scope for this file
//!
//! Writing `hooks.json` is necessary but not sufficient for Codex to
//! actually run these hooks — the user must separately run `/hooks`
//! inside Codex to trust them by content hash (`[hooks.state]` in
//! `~/.codex/config.toml`), or pass `--dangerously-bypass-hook-trust` to
//! `codex exec`/`codex` (only ever appropriate for a vetted source, e.g.
//! the local smoke test that verified this module's file shape).
//! `install_cmd`'s printed next-steps and `doctor_cmd`'s read-only
//! inspection cover that; this module only ever writes/removes/inspects
//! `hooks.json` itself.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

pub const HOOKS_FILE_NAME: &str = "hooks.json";

/// The top-level key holding the event-keyed hook groups. See module
/// docs: `description` is the only other top-level key Codex accepts,
/// and this module never touches it.
const HOOKS_KEY: &str = "hooks";

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
    #[error(
        "refusing to install: the binary path {binary} contains whitespace or a shell \
         metacharacter, and the hook command is run unquoted by a shell. Install the \
         binary to a plain path such as ~/.cargo/bin and re-run"
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
    /// bytes are ever written to `path` when this is returned. See
    /// `claude_settings::SettingsError::WriteLockUnavailable` (mirrored
    /// exactly) and [`crate::write_lock::LockFailure`] for the
    /// class-A/class-B distinction this wraps.
    #[error("{path}: {source}")]
    WriteLockUnavailable {
        path: PathBuf,
        source: crate::write_lock::LockFailure,
    },
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

/// Ensures `path`'s parent directory exists, then acquires the
/// cross-process write lock on `path`'s sidecar, then runs `body` while
/// holding it. Mirrors `claude_settings::with_write_lock` exactly — see
/// that function's docs.
fn with_write_lock<T>(
    path: &Path,
    body: impl FnOnce() -> Result<T, CodexHooksError>,
) -> Result<T, CodexHooksError> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).map_err(|source| CodexHooksError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let _guard = crate::write_lock::acquire(path).map_err(|source| {
        CodexHooksError::WriteLockUnavailable {
            path: path.to_path_buf(),
            source,
        }
    })?;
    body()
}

/// Read/remove-side ownership (`remove`, `inspect`, `wired_commands`): the
/// shared one-path-token rule, plus the legacy unquoted-space install (this
/// installer did not refuse such a path before HORO-2303), so uninstall still
/// removes Libra's own old hooks. A lookalike such as
/// `/opt/libra-governor-tools/x codex-hook stop` is NOT ours and is no longer
/// deleted by uninstall.
fn command_is_ours(command: &str) -> bool {
    OWNED_COMMAND_SUFFIXES
        .iter()
        .any(|suffix| crate::claude_settings::command_is_ours_with_suffix(command, suffix, true))
}

/// Install-side ownership: only an unambiguous single-path-token command is
/// repointed or collapsed; a wrapped or legacy-space command is left alone.
fn command_is_ours_for(command: &str, subcommand: &str) -> bool {
    crate::claude_settings::command_is_ours_with_suffix(command, subcommand, false)
}

/// `true` when `group` applies to every tool (no matcher, or an empty / `*`
/// one). Our own groups never carry a matcher; a user's scoped group that
/// happens to hold our command is left alone.
fn matcher_is_all_tools(group: &Value) -> bool {
    match group.get("matcher") {
        None | Some(Value::Null) => true,
        Some(Value::String(m)) => m.is_empty() || m == "*",
        Some(_) => false,
    }
}

/// A sha256 over this file's exact bytes at one instant, used only to
/// answer "is this still the file I read?" immediately before a write
/// (module docs property 5). Mirrors `claude_settings::FileDigest`
/// exactly — see that type for why sha256 rather than a byte compare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileDigest([u8; 32]);

impl FileDigest {
    fn of(bytes: &[u8]) -> Self {
        let mut out = [0u8; 32];
        out.copy_from_slice(&Sha256::digest(bytes));
        Self(out)
    }

    /// The digest of a file that is not there, so a file created by
    /// another writer between an absent read and the write fails the
    /// check too rather than being overwritten.
    fn absent() -> Self {
        Self::of(&[])
    }
}

/// The file's bytes, or `None` if it does not exist.
fn read_bytes_if_present(path: &Path) -> Result<Option<Vec<u8>>, CodexHooksError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(CodexHooksError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Reads and parses `path` into a JSON object, or `None` if the file
/// does not exist yet. The returned [`FileDigest`] is of the exact bytes
/// read and must be handed to [`write_object_atomically`] so the write
/// can refuse to clobber a change made in between.
fn read_object(path: &Path) -> Result<(Option<Map<String, Value>>, FileDigest), CodexHooksError> {
    let Some(raw) = read_bytes_if_present(path)? else {
        return Ok((None, FileDigest::absent()));
    };
    let digest = FileDigest::of(&raw);
    let text = std::str::from_utf8(&raw).map_err(|_| CodexHooksError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "stream did not contain valid UTF-8",
        ),
    })?;
    let value: Value = serde_json::from_str(text).map_err(|source| CodexHooksError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    match value {
        Value::Object(map) => Ok((Some(map), digest)),
        _ => Err(CodexHooksError::NotAnObject {
            path: path.to_path_buf(),
        }),
    }
}

/// Returns the `root["hooks"]` object, creating an empty one if absent.
/// Errors (never silently replaces) if `root["hooks"]` exists but is not
/// itself a JSON object — that would mean either a hand-edited file in a
/// shape this module cannot safely reason about, or a future Codex
/// version changing the field's type; either way, refuse rather than
/// guess.
fn hooks_object_mut<'a>(
    root: &'a mut Map<String, Value>,
    path: &Path,
) -> Result<&'a mut Map<String, Value>, CodexHooksError> {
    let entry = root
        .entry(HOOKS_KEY.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    entry
        .as_object_mut()
        .ok_or_else(|| CodexHooksError::HooksFieldNotAnObject {
            path: path.to_path_buf(),
        })
}

/// Writes `map` to `path`, but only if `expected` still matches what is
/// on disk — a mismatch aborts with
/// [`CodexHooksError::ConcurrentModification`] before anything at all is
/// written (module docs property 5). Otherwise: a verbatim-bytes backup
/// of any existing file first, then a temp file in the same directory,
/// fsynced, then renamed over the original. Mirrors
/// `claude_settings::write_object_atomically` exactly, including its
/// collision-safe backup naming and its check-before-backup ordering.
fn write_object_atomically(
    path: &Path,
    map: &Map<String, Value>,
    expected: FileDigest,
) -> Result<Option<PathBuf>, CodexHooksError> {
    let io_err = |source: std::io::Error| CodexHooksError::Io {
        path: path.to_path_buf(),
        source,
    };

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).map_err(io_err)?;

    // The first thing this function observes about the file itself, and
    // the gate on every mutating step below it.
    let now_raw = read_bytes_if_present(path)?.unwrap_or_default();
    if FileDigest::of(&now_raw) != expected {
        return Err(CodexHooksError::ConcurrentModification {
            path: path.to_path_buf(),
        });
    }

    // Serialized before the backup is taken so that even a (practically
    // impossible) serialization failure cannot leave a backup behind for
    // a write that never happened.
    let json_text =
        serde_json::to_string_pretty(&Value::Object(map.clone())).map_err(|source| {
            CodexHooksError::Parse {
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

/// What [`apply`] did.
#[derive(Debug, Default, PartialEq)]
pub struct Applied {
    pub hooks_added: usize,
    pub backup_path: Option<PathBuf>,
}

/// Adds this integration's `UserPromptSubmit`/`PostToolUse`/`Stop` hook
/// groups, pointing at `binary`, to `path["hooks"]` (see module docs on
/// why `"hooks"`, not the top level, is where these belong).
/// Idempotent: an already-present Governor-owned entry is not
/// duplicated. A Governor-owned entry pointing at a different binary path
/// (or hand-quoted) is repointed in place, keeping its group, position,
/// timeout and any other keys; extra Governor-owned entries for the same
/// event (left by the pre-HORO-2303 installer) are collapsed into that one.
/// Entries that are not ours are never touched. Never touches any other key (including the sibling
/// top-level `description` field, or any foreign hook group). `binary`
/// should be an absolute path. Writes [`HOOK_TIMEOUT_SECS`] on every
/// entry and `"async": true` on the `PostToolUse` entry only (it prints
/// nothing and must never add perceptible latency) — see that constant's
/// docs. No `matcher` field is written: this integration observes every
/// tool, not a filtered subset.
pub fn apply(path: &Path, binary: &Path) -> Result<Applied, CodexHooksError> {
    // The command is `{binary} {subcommand}` run by a shell, unquoted; a
    // repoint must never produce a command that splits (HORO-2303).
    if crate::claude_settings::has_shell_unsafe_char(&binary.to_string_lossy()) {
        return Err(CodexHooksError::UnsafeBinaryPath {
            binary: binary.to_path_buf(),
        });
    }
    with_write_lock(path, || apply_locked(path, binary))
}

fn apply_locked(path: &Path, binary: &Path) -> Result<Applied, CodexHooksError> {
    let (existing, digest) = read_object(path)?;
    let mut root = existing.unwrap_or_default();
    let binary = binary.display().to_string();
    let mut hooks_added = 0usize;

    {
        let hooks_obj = hooks_object_mut(&mut root, path)?;

        for (event, subcommand, is_async) in [
            ("UserPromptSubmit", "codex-hook user-prompt-submit", false),
            ("PostToolUse", "codex-hook post-tool-use", true),
            ("Stop", "codex-hook stop", false),
        ] {
            let command = format!("{binary} {subcommand}");
            let entries = hooks_obj
                .entry(event.to_string())
                .or_insert_with(|| Value::Array(Vec::new()));
            let entries_arr =
                entries
                    .as_array_mut()
                    .ok_or_else(|| CodexHooksError::HookEventNotAnArray {
                        path: path.to_path_buf(),
                        event: event.to_string(),
                    })?;

            // Our hook for this event, installed from any path: keep the
            // first in place (repointed if it points elsewhere) and drop
            // later ones, so a reinstall from another binary path -- or a
            // file already doubled by the earlier bug -- ends with exactly
            // one (HORO-2303). Only a group that applies to every tool
            // counts; user hooks and lookalikes never pass
            // `command_is_ours_for`.
            let mut kept = false;
            let mut changed = false;
            let mut emptied_groups = Vec::new();
            for (idx, group) in entries_arr.iter_mut().enumerate() {
                if !matcher_is_all_tools(group) {
                    continue;
                }
                let Some(inner) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
                    continue;
                };
                let before = inner.len();
                inner.retain_mut(|h| {
                    let Some(existing) = h.get("command").and_then(Value::as_str) else {
                        return true;
                    };
                    if existing != command && !command_is_ours_for(existing, subcommand) {
                        return true;
                    }
                    if kept {
                        changed = true;
                        return false;
                    }
                    kept = true;
                    if existing != command {
                        h["command"] = Value::String(command.clone());
                        changed = true;
                    }
                    true
                });
                if before > 0 && inner.is_empty() {
                    emptied_groups.push(idx);
                }
            }
            // Same index-shift rule as `remove`: Codex keys trust by group
            // index, so a collapsed duplicate's group is dropped only when
            // it is the tail; otherwise its empty `{"hooks": []}` stays.
            for idx in emptied_groups.into_iter().rev() {
                if idx == entries_arr.len() - 1 {
                    entries_arr.remove(idx);
                }
            }
            if changed {
                hooks_added += 1;
            }
            if !kept {
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
    }

    let backup_path = write_object_atomically(path, &root, digest)?;
    Ok(Applied {
        hooks_added,
        backup_path,
    })
}

/// What [`remove`] did.
#[derive(Debug, Default, PartialEq)]
pub struct Removed {
    pub hook_commands_removed: usize,
    pub backup_path: Option<PathBuf>,
    /// `true` if the file did not exist at all — a safe no-op.
    pub file_absent: bool,
}

/// Removes only Governor-owned hook groups from `path["hooks"]` —
/// pruning empty matchers/event-arrays only when our removal is what
/// emptied them, and pruning the `"hooks"` key itself only when doing so
/// leaves it empty, mirroring `claude_settings::remove`'s discipline for
/// everything except the index-shift tombstone behavior below, which is
/// Codex-specific (HORO-1877: Claude Code's hook trust is not keyed by
/// array position the way Codex's is, so `claude_settings::remove` uses
/// a plain unconditional `Vec::remove` — see that function's docs).
/// Every other key (including `description`), and every foreign entry
/// inside an event's array, is left exactly as it was. A missing file is
/// a safe no-op.
pub fn remove(path: &Path) -> Result<Removed, CodexHooksError> {
    with_write_lock(path, || remove_locked(path))
}

fn remove_locked(path: &Path) -> Result<Removed, CodexHooksError> {
    let (existing, digest) = read_object(path)?;
    let Some(mut root) = existing else {
        return Ok(Removed {
            file_absent: true,
            ..Default::default()
        });
    };

    let mut hook_commands_removed = 0usize;
    if let Some(hooks_value) = root.get_mut(HOOKS_KEY) {
        let hooks_obj =
            hooks_value
                .as_object_mut()
                .ok_or_else(|| CodexHooksError::HooksFieldNotAnObject {
                    path: path.to_path_buf(),
                })?;

        let mut events_to_drop = Vec::new();
        for event in HOOK_EVENTS {
            let Some(entries_value) = hooks_obj.get_mut(event) else {
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
            // Codex keys hook trust by (event, group_idx, handler_idx), so
            // `Vec::remove` on a group that is NOT the array's current
            // tail would shift every later group's index down by one --
            // silently changing a foreign hook's trust key even though
            // its content never changed (HORO-1876). Only remove a group
            // when it is (still, after any prior removals in this same
            // pass) the last element; otherwise leave its already-emptied
            // `{"hooks": []}` matcher in place as a tombstone, matching
            // Fornax's own equivalent fix (PR #265) for the identical
            // shared-array problem.
            for idx in matchers_to_drop.into_iter().rev() {
                if idx == entries_arr.len() - 1 {
                    entries_arr.remove(idx);
                }
            }
            if entries_arr.iter().all(|m| {
                m.get("hooks")
                    .and_then(Value::as_array)
                    .map(|h| h.is_empty())
                    .unwrap_or(false)
            }) {
                events_to_drop.push(event.to_string());
            }
        }
        for event in events_to_drop {
            hooks_obj.remove(&event);
        }
        if hooks_obj.is_empty() {
            root.remove(HOOKS_KEY);
        }
    }

    let backup_path = write_object_atomically(path, &root, digest)?;
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

/// Read-only inspection of `path["hooks"]` for `doctor`. A missing or
/// unparsable file, or a `"hooks"` field that is present but not an
/// object, is reported as simply "not present"/"nothing wired" rather
/// than propagating a parse error, mirroring `claude_settings::inspect`.
pub fn inspect(path: &Path) -> Inspection {
    let Ok((Some(root), _)) = read_object(path) else {
        return Inspection {
            file_present: path.exists(),
            ..Default::default()
        };
    };
    let Some(hooks_obj) = root.get(HOOKS_KEY).and_then(Value::as_object) else {
        return Inspection {
            file_present: true,
            ..Default::default()
        };
    };

    let mut hooks_wired = [false; 3];
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

    Inspection {
        file_present: true,
        hooks_wired,
    }
}

/// Like [`inspect`], but returns the actual Governor-owned `command`
/// string wired for each event (`None` if that event has no
/// Governor-owned entry), rather than just a boolean. HORO-1906's
/// `codex_verify_trust_cmd` uses this to correlate against Codex's own
/// `hooks/list` RPC response by the *real* command text this file was
/// last written with — not a reconstruction from the currently-running
/// binary's own path, which can differ from what's on disk (e.g. the
/// binary moved since `install` last ran; see `doctor`'s own
/// `wired_command_matches_current_binary` check for that exact case).
/// If a Governor-owned event has more than one matching command (a file
/// written by a pre-HORO-2303 installer reinstalled from another path, or
/// a hand-edited one; the next `apply` collapses them), the first one
/// found is returned; this is a read-only best-effort lookup, not a
/// validator.
pub fn wired_commands(path: &Path) -> [Option<String>; 3] {
    let mut out: [Option<String>; 3] = Default::default();
    let Ok((Some(root), _)) = read_object(path) else {
        return out;
    };
    let Some(hooks_obj) = root.get(HOOKS_KEY).and_then(Value::as_object) else {
        return out;
    };
    for (idx, event) in HOOK_EVENTS.iter().enumerate() {
        out[idx] = hooks_obj
            .get(*event)
            .and_then(Value::as_array)
            .and_then(|entries| {
                entries.iter().find_map(|matcher| {
                    matcher
                        .get("hooks")
                        .and_then(Value::as_array)
                        .and_then(|inner| {
                            inner.iter().find_map(|h| {
                                h.get("command")
                                    .and_then(Value::as_str)
                                    .filter(|c| command_is_ours(c))
                                    .map(str::to_string)
                            })
                        })
                })
            });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binary() -> PathBuf {
        PathBuf::from("/opt/libra/bin/libra-governor")
    }

    #[test]
    fn wired_commands_returns_the_real_command_text_per_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        apply(&path, &binary()).unwrap();

        let commands = wired_commands(&path);
        assert_eq!(
            commands[0].as_deref(),
            Some("/opt/libra/bin/libra-governor codex-hook user-prompt-submit")
        );
        assert_eq!(
            commands[1].as_deref(),
            Some("/opt/libra/bin/libra-governor codex-hook post-tool-use")
        );
        assert_eq!(
            commands[2].as_deref(),
            Some("/opt/libra/bin/libra-governor codex-hook stop")
        );
    }

    #[test]
    fn wired_commands_is_none_for_an_unwired_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        // Only write Stop, leave the others unwired.
        let seed = serde_json::json!({
            "hooks": {
                "Stop": [{ "hooks": [{ "type": "command", "command": "/opt/libra/bin/libra-governor codex-hook stop" }] }]
            }
        });
        std::fs::write(&path, serde_json::to_string_pretty(&seed).unwrap()).unwrap();

        let commands = wired_commands(&path);
        assert!(commands[0].is_none());
        assert!(commands[1].is_none());
        assert_eq!(
            commands[2].as_deref(),
            Some("/opt/libra/bin/libra-governor codex-hook stop")
        );
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
    fn apply_writes_entries_under_the_top_level_hooks_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        apply(&path, &binary()).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();

        assert!(
            value.get("UserPromptSubmit").is_none(),
            "hook groups must never be written at the top level — Codex's real schema only \
             accepts \"description\" and \"hooks\" as top-level keys"
        );
        assert!(value["hooks"]["UserPromptSubmit"].is_array());
        assert!(value["hooks"]["PostToolUse"].is_array());
        assert!(value["hooks"]["Stop"].is_array());
    }

    #[test]
    fn apply_writes_the_documented_timeout_and_async_flag() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        apply(&path, &binary()).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();

        assert_eq!(
            value["hooks"]["UserPromptSubmit"][0]["hooks"][0]["timeout"],
            HOOK_TIMEOUT_SECS
        );
        assert!(value["hooks"]["UserPromptSubmit"][0]["hooks"][0]
            .get("async")
            .is_none());
        assert_eq!(
            value["hooks"]["PostToolUse"][0]["hooks"][0]["timeout"],
            HOOK_TIMEOUT_SECS
        );
        assert_eq!(value["hooks"]["PostToolUse"][0]["hooks"][0]["async"], true);
        assert!(value["hooks"]["Stop"][0]["hooks"][0].get("async").is_none());
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
    fn apply_preserves_a_foreign_top_level_description_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        std::fs::write(&path, r#"{"description": "my own hooks file"}"#).unwrap();

        apply(&path, &binary()).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["description"], "my own hooks file");
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
            "an emptied file must contain no leftover \"hooks\" key"
        );
    }

    #[test]
    fn remove_preserves_every_foreign_entry_in_a_mixed_hooks_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        let seed = serde_json::json!({
            "description": "my own hooks file",
            "hooks": {
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
            }
        });
        std::fs::write(&path, serde_json::to_string_pretty(&seed).unwrap()).unwrap();

        let removed = remove(&path).unwrap();
        assert_eq!(
            removed.hook_commands_removed, 1,
            "only the Governor-owned PostToolUse command is ours"
        );

        let text = std::fs::read_to_string(&path).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["description"], "my own hooks file");
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
    fn a_non_object_hooks_field_aborts_and_leaves_the_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        std::fs::write(&path, r#"{"hooks": "not an object"}"#).unwrap();

        let before = std::fs::read_to_string(&path).unwrap();
        let err = remove(&path).unwrap_err();
        assert!(matches!(err, CodexHooksError::HooksFieldNotAnObject { .. }));
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn apply_refuses_and_writes_nothing_when_hooks_is_not_an_object() {
        // HORO-1380 S2 / HORO-998 UNKNOWN_FUTURE_FIELDS_ARE_PRESERVED.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        let original_bytes = b"{\"hooks\": \"not-an-object\"}".to_vec();
        std::fs::write(&path, &original_bytes).unwrap();

        let err = apply(&path, &binary()).unwrap_err();
        assert!(matches!(err, CodexHooksError::HooksFieldNotAnObject { .. }));

        let bytes_after = std::fs::read(&path).unwrap();
        assert_eq!(
            bytes_after, original_bytes,
            "a refused apply must leave the file byte-for-byte unchanged"
        );
    }

    #[test]
    fn apply_refuses_and_writes_nothing_when_a_hook_event_is_not_an_array() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        let original_bytes = serde_json::to_vec_pretty(&serde_json::json!({
            "hooks": { "UserPromptSubmit": "not-an-array" }
        }))
        .unwrap();
        std::fs::write(&path, &original_bytes).unwrap();

        let err = apply(&path, &binary()).unwrap_err();
        assert!(matches!(err, CodexHooksError::HookEventNotAnArray { .. }));

        let bytes_after = std::fs::read(&path).unwrap();
        assert_eq!(
            bytes_after, original_bytes,
            "a refused apply must leave the file byte-for-byte unchanged"
        );
    }

    #[test]
    fn a_backup_is_written_before_any_mutating_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        let seed = serde_json::json!({"hooks": {"UserPromptSubmit": []}});
        let original_bytes = serde_json::to_string_pretty(&seed).unwrap();
        std::fs::write(&path, &original_bytes).unwrap();

        let removed = remove(&path).unwrap();
        let backup_path = removed.backup_path.expect("a backup must be written");
        let backup_bytes = std::fs::read_to_string(&backup_path).unwrap();
        assert_eq!(backup_bytes, original_bytes);
    }

    #[test]
    fn a_concurrent_change_between_read_and_write_is_refused() {
        // HORO-1380 S2b / HORO-998 CONCURRENT_CHANGE_DOES_NOT_CLOBBER —
        // mirrors `claude_settings`'s test of the same property; see
        // there for why this is exercised at the
        // read_object/write_object_atomically seam.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        std::fs::write(&path, "{}\n").unwrap();

        let (root, digest) = read_object(&path).unwrap();
        let mut root = root.unwrap();
        root.insert("description".to_string(), Value::String("ours".to_string()));

        let racing_bytes = br#"{"description": "someone else's edit"}"#.to_vec();
        std::fs::write(&path, &racing_bytes).unwrap();

        let err = write_object_atomically(&path, &root, digest).unwrap_err();
        assert!(matches!(
            err,
            CodexHooksError::ConcurrentModification { .. }
        ));

        assert_eq!(
            std::fs::read(&path).unwrap(),
            racing_bytes,
            "the racing writer's content must survive byte-for-byte — never clobbered by \
             our stale edit, never reverted to what we read"
        );

        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("hooks.json")]);
    }

    #[test]
    fn a_file_created_between_an_absent_read_and_the_write_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");

        let (root, digest) = read_object(&path).unwrap();
        assert!(root.is_none());
        let mut root = root.unwrap_or_default();
        root.insert("description".to_string(), Value::String("ours".to_string()));

        let racing_bytes = br#"{"description": "someone else's edit"}"#.to_vec();
        std::fs::write(&path, &racing_bytes).unwrap();

        let err = write_object_atomically(&path, &root, digest).unwrap_err();
        assert!(matches!(
            err,
            CodexHooksError::ConcurrentModification { .. }
        ));

        assert_eq!(
            std::fs::read(&path).unwrap(),
            racing_bytes,
            "a file that appeared after an absent read must not be overwritten"
        );
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("hooks.json")]);
    }

    #[test]
    fn an_event_array_survives_when_a_sibling_matcher_remains_foreign() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        let seed = serde_json::json!({
            "hooks": {
                "Stop": [
                    { "hooks": [{ "type": "command", "command": "/opt/libra/bin/libra-governor codex-hook stop" }] },
                    { "hooks": [{ "type": "command", "command": "/usr/local/bin/other stop-notify" }] }
                ]
            }
        });
        std::fs::write(&path, serde_json::to_string_pretty(&seed).unwrap()).unwrap();

        remove(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        let stop = value["hooks"]["Stop"].as_array().unwrap();
        // Our own (now-empty) group is left in place as a `{"hooks": []}`
        // tombstone rather than removed, specifically because removing it
        // would shift the foreign group from index 1 to index 0 --
        // Codex keys hook trust by (event, group_idx, handler_idx), so
        // that shift would silently invalidate the foreign group's trust
        // state even though its content never changed (HORO-1876).
        assert_eq!(stop.len(), 2, "our own emptied group must be left as a tombstone, not removed, so the foreign group's index never shifts");
        assert_eq!(
            stop[0]["hooks"].as_array().unwrap().len(),
            0,
            "our own group is emptied but kept as a tombstone at its original index"
        );
        assert_eq!(
            stop[1]["hooks"][0]["command"], "/usr/local/bin/other stop-notify",
            "the foreign matcher must survive at its original index, unshifted"
        );
    }

    #[test]
    fn uninstall_before_a_foreign_group_leaves_a_tombstone_so_a_later_foreign_groups_index_never_shifts(
    ) {
        // HORO-1876: a third group after the foreign one makes the index-
        // preservation claim unambiguous -- if our removal ever shifted
        // indices, this foreign group would move from index 2 to index 1.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        let seed = serde_json::json!({
            "hooks": {
                "PostToolUse": [
                    { "hooks": [{ "type": "command", "command": "/opt/libra/bin/libra-governor codex-hook post-tool-use" }] },
                    { "hooks": [{ "type": "command", "command": "/usr/local/bin/fornax post-tool-use" }] },
                    { "hooks": [{ "type": "command", "command": "/usr/local/bin/another-tool post-tool-use" }] }
                ]
            }
        });
        std::fs::write(&path, serde_json::to_string_pretty(&seed).unwrap()).unwrap();

        remove(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        let entries = value["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(
            entries.len(),
            3,
            "our own emptied group stays in place as a tombstone; nothing is removed from the middle"
        );
        assert_eq!(
            entries[0]["hooks"].as_array().unwrap().len(),
            0,
            "our own group at its original index 0 is emptied, not removed"
        );
        assert_eq!(
            entries[1]["hooks"][0]["command"], "/usr/local/bin/fornax post-tool-use",
            "the first foreign group stays at its original index 1"
        );
        assert_eq!(
            entries[2]["hooks"][0]["command"], "/usr/local/bin/another-tool post-tool-use",
            "the second foreign group stays at its original index 2, unshifted"
        );
    }

    #[test]
    fn uninstall_after_every_foreign_group_actually_removes_our_own_trailing_group() {
        // When nothing foreign follows ours, there is nothing to protect
        // from an index shift -- removing our own trailing group (rather
        // than leaving a needless tombstone) matches Fornax's own
        // equivalent fix and keeps the file minimal.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        let seed = serde_json::json!({
            "hooks": {
                "PostToolUse": [
                    { "hooks": [{ "type": "command", "command": "/usr/local/bin/fornax post-tool-use" }] },
                    { "hooks": [{ "type": "command", "command": "/opt/libra/bin/libra-governor codex-hook post-tool-use" }] }
                ]
            }
        });
        std::fs::write(&path, serde_json::to_string_pretty(&seed).unwrap()).unwrap();

        remove(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        let entries = value["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(
            entries.len(),
            1,
            "our own trailing group is actually removed, not left as a needless tombstone"
        );
        assert_eq!(
            entries[0]["hooks"][0]["command"], "/usr/local/bin/fornax post-tool-use",
            "the foreign group stays at its original index 0, unaffected"
        );
    }

    // --- HORO-2303: reinstall from a different binary path ---------------

    const OLD: &str = "/old/bin/libra-governor";

    fn old_command(subcommand: &str) -> String {
        format!("{OLD} {subcommand}")
    }

    /// Every event wired from `OLD`, in the shape `apply` writes.
    fn seed_wired_from_old(path: &Path) {
        apply(path, Path::new(OLD)).unwrap();
    }

    fn read_value(path: &Path) -> Value {
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    /// Our commands (loose predicate) per event, in file order.
    fn owned_commands(root: &Value, event: &str) -> Vec<String> {
        root["hooks"][event]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|g| g["hooks"].as_array().into_iter().flatten())
            .filter_map(|h| h["command"].as_str())
            .filter(|c| command_is_ours(c))
            .map(str::to_string)
            .collect()
    }

    const EVENT_SUBCOMMANDS: [(&str, &str); 3] = [
        ("UserPromptSubmit", "codex-hook user-prompt-submit"),
        ("PostToolUse", "codex-hook post-tool-use"),
        ("Stop", "codex-hook stop"),
    ];

    #[test]
    fn reinstall_from_another_path_leaves_exactly_one_group_per_event_with_the_new_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        seed_wired_from_old(&path);

        let applied = apply(&path, &binary()).unwrap();

        let root = read_value(&path);
        for (event, subcommand) in EVENT_SUBCOMMANDS {
            assert_eq!(root["hooks"][event].as_array().unwrap().len(), 1, "{event}");
            assert_eq!(
                owned_commands(&root, event),
                vec![format!("{} {subcommand}", binary().display())],
                "{event}"
            );
        }
        assert_eq!(applied.hooks_added, 3, "three repoints reported");
        // Timeout and the async flag of the repointed entries survive.
        assert_eq!(
            root["hooks"]["Stop"][0]["hooks"][0]["timeout"],
            HOOK_TIMEOUT_SECS
        );
        assert_eq!(root["hooks"]["PostToolUse"][0]["hooks"][0]["async"], true);
    }

    #[test]
    fn reinstall_repoints_in_place_and_leaves_foreign_lookalike_and_unknown_entries_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        let foreign = serde_json::json!({ "hooks": [{ "type": "command", "command": "/usr/local/bin/fornax stop" }] });
        let lookalike = serde_json::json!({ "hooks": [{ "type": "command", "command": "/opt/libra-governor-tools/x codex-hook stop" }] });
        let scoped = serde_json::json!({
            "matcher": "Bash",
            "hooks": [{ "type": "command", "command": old_command("codex-hook stop") }]
        });
        let ours = serde_json::json!({
            "unknownGroupKey": 7,
            "hooks": [
                { "type": "command", "command": "/usr/local/bin/before stop" },
                {
                    "type": "command",
                    "command": old_command("codex-hook stop"),
                    "timeout": 99,
                    "statusMessage": "keep me"
                },
                { "type": "command", "command": "/usr/local/bin/after stop" }
            ]
        });
        let seed = serde_json::json!({
            "description": "mine",
            "hooks": { "Stop": [foreign, lookalike, scoped, ours] }
        });
        std::fs::write(&path, serde_json::to_string_pretty(&seed).unwrap()).unwrap();

        apply(&path, &binary()).unwrap();

        let root = read_value(&path);
        let stop = root["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(root["description"], "mine");
        assert_eq!(stop.len(), 4, "no group added or removed for Stop");
        assert_eq!(stop[0], seed["hooks"]["Stop"][0], "foreign byte-preserved");
        assert_eq!(
            stop[1], seed["hooks"]["Stop"][1],
            "lookalike byte-preserved"
        );
        assert_eq!(
            stop[2], seed["hooks"]["Stop"][2],
            "a matcher-scoped group is not an every-tool install and is left alone"
        );
        let mut expected = seed["hooks"]["Stop"][3].clone();
        expected["hooks"][1]["command"] =
            Value::String(format!("{} codex-hook stop", binary().display()));
        assert_eq!(stop[3], expected, "only the command changed, in place");
    }

    #[test]
    fn reinstall_repoints_a_hand_quoted_old_path_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        let seed = serde_json::json!({
            "hooks": {
                "UserPromptSubmit": [{ "hooks": [{ "type": "command",
                    "command": "'/opt/my tools/libra-governor' codex-hook user-prompt-submit" }] }],
                "Stop": [{ "hooks": [{ "type": "command",
                    "command": "\"/opt/my tools/libra-governor\" codex-hook stop" }] }]
            }
        });
        std::fs::write(&path, serde_json::to_string_pretty(&seed).unwrap()).unwrap();

        apply(&path, &binary()).unwrap();

        let root = read_value(&path);
        for event in ["UserPromptSubmit", "Stop"] {
            assert_eq!(root["hooks"][event].as_array().unwrap().len(), 1, "{event}");
        }
        assert_eq!(
            owned_commands(&root, "Stop"),
            vec![format!("{} codex-hook stop", binary().display())]
        );
        assert_eq!(owned_commands(&root, "UserPromptSubmit").len(), 1);
        assert_eq!(owned_commands(&root, "PostToolUse").len(), 1);
    }

    #[test]
    fn reinstall_collapses_pre_existing_duplicate_entries_to_one_and_keeps_user_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        // The state the earlier installer produced: a second Libra group per
        // event, plus one more in the same group as a user hook.
        let user = serde_json::json!({ "hooks": [{ "type": "command", "command": "/usr/local/bin/fornax stop" }] });
        let seed = serde_json::json!({
            "hooks": { "Stop": [
                { "hooks": [{ "type": "command", "command": old_command("codex-hook stop"), "timeout": 15 }] },
                user,
                { "hooks": [
                    { "type": "command", "command": "/usr/local/bin/mine stop" },
                    { "type": "command", "command": format!("{} codex-hook stop", binary().display()), "timeout": 15 }
                ] },
                { "hooks": [{ "type": "command", "command": "/third/libra-governor codex-hook stop", "timeout": 15 }] }
            ] }
        });
        std::fs::write(&path, serde_json::to_string_pretty(&seed).unwrap()).unwrap();

        apply(&path, &binary()).unwrap();

        let root = read_value(&path);
        assert_eq!(
            owned_commands(&root, "Stop"),
            vec![format!("{} codex-hook stop", binary().display())],
            "one Libra entry, kept at the position of the first"
        );
        let stop = root["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(
            stop[0]["hooks"][0]["command"],
            format!("{} codex-hook stop", binary().display())
        );
        assert_eq!(stop[1], seed["hooks"]["Stop"][1], "user group untouched");
        assert_eq!(
            stop[2],
            serde_json::json!({ "hooks": [{ "type": "command", "command": "/usr/local/bin/mine stop" }] }),
            "a user hook sharing a group with a duplicate stays"
        );
        assert_eq!(
            stop.len(),
            3,
            "the emptied tail group is dropped (nothing after it shifts)"
        );
    }

    #[test]
    fn collapsing_a_duplicate_before_a_later_group_leaves_a_tombstone_not_an_index_shift() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        let later = serde_json::json!({ "hooks": [{ "type": "command", "command": "/usr/local/bin/fornax stop" }] });
        let seed = serde_json::json!({
            "hooks": { "Stop": [
                { "hooks": [{ "type": "command", "command": old_command("codex-hook stop") }] },
                { "hooks": [{ "type": "command", "command": "/third/libra-governor codex-hook stop" }] },
                later
            ] }
        });
        std::fs::write(&path, serde_json::to_string_pretty(&seed).unwrap()).unwrap();

        apply(&path, &binary()).unwrap();

        let stop = read_value(&path)["hooks"]["Stop"].clone();
        let stop = stop.as_array().unwrap();
        assert_eq!(stop.len(), 3);
        assert_eq!(stop[1], serde_json::json!({ "hooks": [] }));
        assert_eq!(
            stop[2], seed["hooks"]["Stop"][2],
            "later group keeps its index"
        );
    }

    #[test]
    fn second_identical_install_is_a_byte_for_byte_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        seed_wired_from_old(&path);
        apply(&path, &binary()).unwrap();
        let first = std::fs::read(&path).unwrap();

        let second = apply(&path, &binary()).unwrap();

        assert_eq!(second.hooks_added, 0);
        assert_eq!(std::fs::read(&path).unwrap(), first);
    }

    #[test]
    fn uninstall_after_a_reinstall_from_another_path_removes_ours_and_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        let foreign = serde_json::json!({ "hooks": [{ "type": "command", "command": "/usr/local/bin/fornax stop" }] });
        let seed = serde_json::json!({ "hooks": { "Stop": [foreign.clone()] } });
        std::fs::write(&path, serde_json::to_string_pretty(&seed).unwrap()).unwrap();
        seed_wired_from_old(&path);
        apply(&path, &binary()).unwrap();

        let removed = remove(&path).unwrap();

        assert_eq!(
            removed.hook_commands_removed, 3,
            "one per event, no leftover"
        );
        assert_eq!(read_value(&path), seed);
    }

    /// Commands that merely end in our suffix or mention our binary; none is
    /// a single `libra-governor` path token, so none is ours.
    fn not_ours_commands(sub: &str) -> Vec<String> {
        vec![
            format!("env X=1 /p/libra-governor {sub}"),
            format!("/usr/bin/env LIBRA_GOVERNOR_STATE_DIR=/s /p/libra-governor {sub}"),
            format!("cd /x && /p/libra-governor {sub}"),
            format!("a | /p/libra-governor {sub}"),
            format!("/p/libra-governor/{sub}"),
            format!("/p/libra-governor/ {sub}"),
            format!("/opt/libra-governor-tools/x {sub}"),
            format!("/p/other-tool {sub}"),
            format!("/p/libra-governor{sub}"),
        ]
    }

    #[test]
    fn wrapped_lookalike_and_other_binary_commands_are_not_ours() {
        for (_, sub) in EVENT_SUBCOMMANDS {
            for cmd in not_ours_commands(sub) {
                assert!(!command_is_ours(&cmd), "{cmd}");
                assert!(!command_is_ours_for(&cmd, sub), "{cmd}");
            }
        }
    }

    #[test]
    fn install_and_duplicate_collapse_leave_wrapped_and_lookalike_commands_byte_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        let mut hooks = serde_json::Map::new();
        let mut originals = serde_json::Map::new();
        for (event, sub) in EVENT_SUBCOMMANDS {
            let mut groups: Vec<Value> = not_ours_commands(sub)
                .into_iter()
                .map(|c| serde_json::json!({ "hooks": [{ "type": "command", "command": c }] }))
                .collect();
            originals.insert(event.to_string(), Value::Array(groups.clone()));
            // Two real duplicates after the user entries, to drive the collapse.
            groups.push(serde_json::json!({ "hooks": [{ "type": "command", "command": old_command(sub) }] }));
            groups.push(serde_json::json!({ "hooks": [{ "type": "command", "command": format!("/third/libra-governor {sub}") }] }));
            hooks.insert(event.to_string(), Value::Array(groups));
        }
        let seed = serde_json::json!({ "hooks": Value::Object(hooks) });
        std::fs::write(&path, serde_json::to_string_pretty(&seed).unwrap()).unwrap();

        apply(&path, &binary()).unwrap();

        let root = read_value(&path);
        for (event, sub) in EVENT_SUBCOMMANDS {
            let groups = root["hooks"][event].as_array().unwrap();
            let user = originals[event].as_array().unwrap();
            assert_eq!(
                &groups[..user.len()],
                &user[..],
                "{event}: user entries byte-preserved"
            );
            assert_eq!(groups.len(), user.len() + 1, "{event}: one Libra entry");
            assert_eq!(
                groups[user.len()]["hooks"][0]["command"],
                format!("{} {sub}", binary().display())
            );
        }
    }

    #[test]
    fn uninstall_keeps_a_lookalike_binary_command_and_wrapped_commands() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        let user: Vec<Value> = not_ours_commands("codex-hook stop")
            .into_iter()
            .map(|c| serde_json::json!({ "hooks": [{ "type": "command", "command": c }] }))
            .collect();
        let seed = serde_json::json!({ "hooks": { "Stop": user } });
        std::fs::write(&path, serde_json::to_string_pretty(&seed).unwrap()).unwrap();
        apply(&path, &binary()).unwrap();

        let removed = remove(&path).unwrap();

        assert_eq!(removed.hook_commands_removed, 3);
        assert_eq!(read_value(&path), seed);
    }

    #[test]
    fn a_legacy_unquoted_space_path_is_still_removed_by_uninstall_but_not_rewritten_by_install() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        let legacy = "/opt/legacy path/libra-governor codex-hook stop";
        let seed = serde_json::json!({ "hooks": { "Stop": [
            { "hooks": [{ "type": "command", "command": legacy }] }
        ] } });
        std::fs::write(&path, serde_json::to_string_pretty(&seed).unwrap()).unwrap();

        apply(&path, &binary()).unwrap();
        let root = read_value(&path);
        assert_eq!(
            root["hooks"]["Stop"][0], seed["hooks"]["Stop"][0],
            "not rewritten"
        );
        assert_eq!(root["hooks"]["Stop"].as_array().unwrap().len(), 2);

        remove(&path).unwrap();
        assert!(
            read_value(&path).get("hooks").is_none(),
            "legacy entry removed too"
        );
    }

    #[test]
    fn a_hand_quoted_path_round_trips_through_reinstall_and_uninstall() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        let seed = serde_json::json!({ "hooks": { "Stop": [{ "hooks": [{ "type": "command",
            "command": "'/opt/my tools/libra-governor' codex-hook stop" }] }] } });
        std::fs::write(&path, serde_json::to_string_pretty(&seed).unwrap()).unwrap();
        assert!(command_is_ours_for(
            "'/opt/my tools/libra-governor' codex-hook stop",
            "codex-hook stop"
        ));
        assert!(!command_is_ours_for(
            "'/opt/my tools/libra-governor\" codex-hook stop",
            "codex-hook stop"
        ));

        apply(&path, &binary()).unwrap();
        assert_eq!(owned_commands(&read_value(&path), "Stop").len(), 1);
        remove(&path).unwrap();
        assert!(read_value(&path).get("hooks").is_none());
    }

    #[test]
    fn install_refuses_a_binary_path_the_shell_would_split_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        for bad in [
            "/opt/my tools/libra-governor",
            "/opt/a;b/libra-governor",
            "/opt/$HOME/libra-governor",
        ] {
            let err = apply(&path, Path::new(bad)).unwrap_err();
            assert!(
                matches!(err, CodexHooksError::UnsafeBinaryPath { .. }),
                "{bad}"
            );
        }
        assert!(!path.exists());
    }
}
