//! `libra-governor doctor` (HORO-1150) — a read-only diagnostic snapshot
//! covering daemon availability/version, SQLite schema health, Claude
//! Code integration wiring, gateway mode/capability, config.json
//! validity, and (trivially, since none exists) telemetry posture.
//!
//! Thin per this repo's own architecture rule: every check here either
//! reads a local file this process is already allowed to read (state
//! dir permissions, `config.json` presence, `gateway.token` presence,
//! `~/.claude/settings.json` wiring) or relays [`Request::Doctor`] and
//! renders what the daemon answers — no policy/business logic lives
//! here. Never spawns a daemon (mirrors `statusline`'s `connect_only`
//! discipline, `crates/cli/src/gateway_cmd.rs`): a diagnostic must not
//! change the thing it is diagnosing.
//!
//! # Never prints a secret
//!
//! No check here reads `gateway.token`'s contents, runs a
//! `credential_command`, or echoes anything from `config.json` beyond
//! [`libra_governor_protocol::DoctorResult::config_file_error`], which is
//! itself never more than a preset name, a missing-field message, or a
//! JSON parse-position message — `config.json` never contains a secret
//! value itself (only a *pointer* to one, e.g. `security` or `op`).

use std::path::{Path, PathBuf};

use libra_governor_protocol::{DoctorResult, Request, Response};

use crate::{claude_settings, client, codex_hooks_file, gateway_cmd, install_cmd};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Severity {
    Ok,
    Warn,
    Error,
}

impl Severity {
    fn label(self) -> &'static str {
        match self {
            Severity::Ok => "ok",
            Severity::Warn => "warn",
            Severity::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Finding {
    id: &'static str,
    severity: Severity,
    message: String,
}

/// Runs every check and prints either a human-readable report (default)
/// or a single JSON object to stdout (`--json`). Exits `1` if any
/// finding is [`Severity::Error`], `0` otherwise — a daemon that simply
/// is not running yet (the normal state before the first prompt) is
/// [`Severity::Warn`], not a failure.
pub fn run(json: bool) {
    let daemon = fetch_daemon_doctor();
    let daemon_present = daemon.is_some();

    let mut all = collect_findings(daemon_present);
    all.extend(daemon_findings(&daemon));

    let exit_code = if all.iter().any(|f| f.severity == Severity::Error) {
        1
    } else {
        0
    };

    if json {
        println!("{}", render_json(&all, daemon.as_ref()));
    } else {
        println!("{}", render_human(&all, daemon.as_ref()));
    }

    std::process::exit(exit_code);
}

/// Local, no-daemon-required checks: state directory, `config.json`
/// presence, `gateway.token` presence, and Claude Code settings wiring.
/// `daemon_present` gates the two identity checks below that must never
/// fire when nothing at all is listening on the daemon's socket — see
/// [`stale_runtime_finding`] and [`orphan_socket_finding`].
fn collect_findings(daemon_present: bool) -> Vec<Finding> {
    let mut findings = Vec::new();

    let state_dir = libra_governor_daemon::paths::state_dir().ok();
    match &state_dir {
        None => findings.push(Finding {
            id: "state_dir",
            severity: Severity::Warn,
            message: "could not resolve a state directory (HOME unset)".to_string(),
        }),
        Some(dir) if !dir.exists() => findings.push(Finding {
            id: "state_dir",
            severity: Severity::Warn,
            message: format!(
                "state directory {} does not exist yet — the daemon has never run \
                 (normal before the first prompt)",
                dir.display()
            ),
        }),
        Some(dir) => {
            findings.push(state_dir_permission_finding(dir));
            findings.push(config_file_presence_finding(dir));
            findings.push(gateway_token_finding(dir));
            if let Some(f) = stale_runtime_finding(dir, daemon_present) {
                findings.push(f);
            }
            if let Some(f) = orphan_socket_finding(dir, daemon_present) {
                findings.push(f);
            }
        }
    }

    findings.push(claude_settings_finding());
    findings.push(codex_hooks_finding());
    findings.push(quota_source_mode_finding());
    findings.push(pacing_authority_finding());
    findings.push(execution_identity_capture_finding());
    findings
}

/// HORO-1767: names `quota explain --replay`'s actual authority honestly
/// — a *simulated* advisory forecast only, never live enforcement.
/// Always `Ok` severity (never `Warn`/`Error`, which would change
/// `doctor`'s exit code — AC4 forbids that): this is documented,
/// intentional scope, not a broken or missing thing to fix. See
/// `crates/cli/src/quota_cmd/mod.rs`'s own module docs and HORO-1727.
fn pacing_authority_finding() -> Finding {
    Finding {
        id: "pacing_authority",
        severity: Severity::Ok,
        message: "pacing: simulated only (quota explain --replay); no live enforcement \
                  authority (HORO-1727)"
            .to_string(),
    }
}

/// HORO-1764: reports which `libra-governor-quota-source` mode is active.
/// Additive only — no `DoctorResult`/protocol/daemon change, since this
/// ticket stays out of HORO-1763's daemon/ledger lane. Always `Ok` in
/// this build: there is no live adapter to fail to reach, so the honest
/// severity is informational, not an error or even a warning — this is
/// documented scope, not a broken thing to fix.
fn quota_source_mode_finding() -> Finding {
    Finding {
        id: "quota_source_mode",
        severity: Severity::Ok,
        message: format!(
            "quota-source mode: {}",
            libra_governor_quota_source::describe_active_mode()
        ),
    }
}

/// HORO-1603 AC2: a static, side-effect-free statement of which
/// `horonomy/.github` execution-identity dimensions (`session`, `agent`,
/// `turn`, `task`, `host`) this build's own write path actually
/// populates on a real decision — never a reading of any stored row
/// (this doctor must not query the ledger for this, mirroring this
/// module's own "a diagnostic must not change the thing it is
/// diagnosing" rule by never pretending to read it either).
///
/// Mirrors Circinus's `execution_identity_capture` check
/// (`src/circinus/diagnostics/checks.py`) in spirit, not in code: Libra
/// has no per-decision `ExecutionIdentity` envelope at all today. Only
/// `session` is captured, as a raw `session_id` column on
/// `session_preflights`/`session_tasks`/`shadow_runtime_decisions`
/// (`crates/daemon/src/server.rs`), not as a persisted envelope.
/// `agent`/`turn` are parsed by the hook into an envelope
/// (`crates/cli/src/agent/identity.rs`) that only ever reaches a
/// redacted log line — no daemon request carries them, and the
/// `ExecutionOwner` IPC that would persist them (HORO-1600,
/// `crates/ledger/src/execution_owner.rs`) has no production caller
/// yet (only `crates/daemon/tests/execution_owner.rs` sends it).
///
/// `task` is a false-friend case, not a clean "field is missing": every
/// decision row has its own `task_id`, but that is Libra's own economic
/// `TaskId` minted per session (see `crates/domain/src/execution_identity.rs`'s
/// own doc comment and ADR-0002) — a different concept from the
/// contract's provider-native `turn`/`task` dimension. Reporting it as
/// captured would be exactly the fabricated-precision failure HORO-1597
/// exists to close. `host` is implicit (one daemon/one ledger per state
/// directory, not per machine — `LIBRA_GOVERNOR_STATE_DIR` can override
/// it) and a real `host_id` is generated but never recorded on a
/// decision.
///
/// `Severity::Ok`, matching `quota_source_mode_finding`/
/// `pacing_authority_finding` above: partial adoption of the
/// execution-identity contract is documented, expected scope, not a
/// defect to warn about.
fn execution_identity_capture_finding() -> Finding {
    Finding {
        id: "execution_identity_capture",
        severity: Severity::Ok,
        message: "session: captured (the host's real session_id is sent on every \
                  Preflight/ToolInvoked/Finalize request and keys session_preflights, \
                  session_tasks and shadow_runtime_decisions rows -- as a raw session_id \
                  column, not a persisted ExecutionIdentity envelope) -- agent/turn: not \
                  captured (hooks parse agent_id/turn_id into an envelope that is only \
                  written to a redacted log line; no daemon request carries them, and the \
                  ExecutionOwner IPC that would persist them has no production caller yet) \
                  -- task: not captured (every decision row has a task_id, but it is \
                  Libra's own economic TaskId minted per session -- a different concept \
                  from the contract's provider-native turn/task dimension) -- host: \
                  implicit (one daemon and one ledger per state directory; a generated \
                  host_id exists but is not recorded on decisions) -- not a defect: partial \
                  adoption of the execution-identity contract is expected (horonomy/.github \
                  governance/product/execution-identity-contract.md#partial-adoption-is-expected-not-a-defect); \
                  remaining dimensions tracked under HORO-1603, owner-IPC wiring under HORO-1714"
            .to_string(),
    }
}

/// HORO-1380 S4b: the running daemon's `exe_sha256` (from its pidfile)
/// differs from the sha256 of the binary currently at the installed path
/// (from `install.json`'s `binary_path`, hashed fresh here — never a
/// cached hash, which would itself go stale if the binary were ever
/// replaced in place). `None` — not reported, never treated as a mismatch
/// — whenever any input is unavailable:
///
/// - `daemon_present` is `false` (nothing answers the daemon's socket).
///   The pidfile deliberately outlives a
///   signalled daemon (see `pidfile`'s module docs — there is no
///   "remove on clean exit"), so after any `daemon stop` the file left
///   behind still names the *old* binary. Comparing it to a freshly
///   installed one with no daemon actually running would be a permanent
///   false positive on the single most common upgrade path.
/// - no pidfile, or a pidfile written before `exe_sha256` existed.
/// - no `install.json`, or no `binary_path` in it, or that path no
///   longer has anything readable at it.
fn stale_runtime_finding(state_dir: &Path, daemon_present: bool) -> Option<Finding> {
    if !daemon_present {
        return None;
    }
    let pid_record = libra_governor_daemon::pidfile::read(state_dir)?;
    let running_sha = pid_record.exe_sha256?;

    let marker_path = state_dir.join(install_cmd::INSTALL_MARKER_FILE_NAME);
    let text = std::fs::read_to_string(marker_path).ok()?;
    let marker: serde_json::Value = serde_json::from_str(&text).ok()?;
    let binary_path = marker.get("binary_path").and_then(|v| v.as_str())?;
    let installed_sha = libra_governor_daemon::pidfile::hash_file(Path::new(binary_path))?;

    if running_sha == installed_sha {
        return None;
    }
    Some(Finding {
        id: "stale_runtime",
        severity: Severity::Error,
        message: format!(
            "the running daemon's executable (sha256 {running_sha}) differs from the binary \
             currently installed at {binary_path} (sha256 {installed_sha}) — binary upgraded \
             while daemon exists; run `libra-governor daemon stop` — the next hook invocation \
             will respawn it automatically"
        ),
    })
}

/// HORO-1380 S4b: `daemon.sock` exists but `daemon.pid` is missing or
/// unreadable. Two distinct causes, reported distinctly rather than
/// guessed at:
///
/// - A daemon *is* answering on the socket right now (`daemon_present`)
///   — a live daemon that predates the pidfile feature this very ticket
///   adds, so it never wrote one. `daemon stop` cannot identity-check it;
///   this finding says so and tells the operator to stop it by hand, not
///   to delete the socket out from under a process still using it.
/// - Nothing answers the socket — a genuinely stale leftover. This
///   finding guides the operator to confirm via `ps`/`lsof` that no live
///   Governor daemon holds this state dir, then remove the stale socket
///   and log by hand. `doctor` never auto-remediates this itself, per
///   this ticket's own "no blind deletion of user evidence" principle.
fn orphan_socket_finding(state_dir: &Path, daemon_present: bool) -> Option<Finding> {
    let socket_path = state_dir.join("daemon.sock");
    let pid_path = state_dir.join("daemon.pid");
    if !socket_path.exists() || libra_governor_daemon::pidfile::read(state_dir).is_some() {
        return None;
    }

    let message = if daemon_present {
        format!(
            "{} exists and a daemon is currently answering on it, but {} is missing or \
             unreadable — this daemon predates the pid-record feature (HORO-1380) and never \
             wrote one, so `daemon stop` cannot identity-check it; stop it by hand (confirm \
             which process via `ps`/`lsof` first) — the next `daemon run` will write a fresh \
             pid record automatically",
            socket_path.display(),
            pid_path.display()
        )
    } else {
        format!(
            "{} exists but {} is missing or unreadable, and nothing answers the socket — \
             confirm via `ps`/`lsof` that no live Governor daemon holds this state dir, then \
             remove the stale socket and daemon.log by hand; doctor does not auto-remediate \
             this (there is no usable pid record to identity-check a `daemon stop` against)",
            socket_path.display(),
            pid_path.display()
        )
    };

    Some(Finding {
        id: "orphan_socket",
        severity: Severity::Warn,
        message,
    })
}

#[cfg(unix)]
fn state_dir_permission_finding(dir: &Path) -> Finding {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(dir) {
        Ok(meta) if meta.permissions().mode() & 0o777 == 0o700 => Finding {
            id: "state_dir_permissions",
            severity: Severity::Ok,
            message: format!("{} is owner-only (0700)", dir.display()),
        },
        Ok(meta) => Finding {
            id: "state_dir_permissions",
            severity: Severity::Warn,
            message: format!(
                "{} is mode {:o}, not owner-only (0700) — it will be tightened on the \
                 next daemon start",
                dir.display(),
                meta.permissions().mode() & 0o777
            ),
        },
        Err(e) => Finding {
            id: "state_dir_permissions",
            severity: Severity::Warn,
            message: format!("could not stat {}: {e}", dir.display()),
        },
    }
}

/// Validates `config.json` locally (the same `load_overrides` parser the
/// daemon itself uses) rather than only via the daemon: a fresh install
/// with a corrupt `config.json` and no daemon running yet must still be
/// reported as broken, not silently pass because nothing has started it
/// up to notice. When a daemon *is* reachable, [`daemon_findings`] also
/// reports `config_file_valid` from its own live re-read — deliberately
/// redundant with this check rather than relying on only one of the two,
/// since either one running alone must still catch the problem.
fn config_file_presence_finding(dir: &Path) -> Finding {
    let path = dir.join(libra_governor_daemon::config_file::CONFIG_FILE_NAME);
    if !path.exists() {
        return Finding {
            id: "config_file_present",
            severity: Severity::Ok,
            message: "no config.json — running the balanced preset with no gateway (defaults)"
                .to_string(),
        };
    }
    match libra_governor_daemon::config_file::load_overrides(dir) {
        Ok(_) => Finding {
            id: "config_file_present",
            severity: Severity::Ok,
            message: format!("{} is present and parses cleanly", path.display()),
        },
        Err(e) => Finding {
            id: "config_file_present",
            severity: Severity::Error,
            message: format!(
                "{} is present but invalid: {e} — the daemon falls back to its hardcoded \
                 defaults until this is fixed",
                path.display()
            ),
        },
    }
}

fn gateway_token_finding(dir: &Path) -> Finding {
    let path = dir.join(gateway_cmd::TOKEN_FILE_NAME);
    Finding {
        id: "gateway_token_present",
        severity: Severity::Ok,
        message: format!(
            "gateway capability token {} ({})",
            if path.exists() {
                "present"
            } else {
                "not yet created"
            },
            path.display()
        ),
    }
}

fn claude_settings_finding() -> Finding {
    let Ok(path) = claude_settings::settings_path() else {
        return Finding {
            id: "claude_settings",
            severity: Severity::Warn,
            message: "could not resolve ~/.claude/settings.json (HOME unset)".to_string(),
        };
    };
    let inspection = claude_settings::inspect(&path);
    let all_hooks_wired = inspection.hooks_wired.iter().all(|w| *w);
    if !inspection.file_present || (!all_hooks_wired && !inspection.statusline_wired) {
        Finding {
            id: "claude_settings",
            severity: Severity::Warn,
            message: format!(
                "not installed into {} — run `libra-governor install`",
                path.display()
            ),
        }
    } else if !all_hooks_wired {
        Finding {
            id: "claude_settings",
            severity: Severity::Warn,
            message: format!(
                "only some of UserPromptSubmit/PostToolUse/Stop are wired in {} — \
                 re-run `libra-governor install`",
                path.display()
            ),
        }
    } else if !inspection.statusline_wired {
        Finding {
            id: "claude_settings",
            severity: Severity::Ok,
            message: format!(
                "hooks wired in {}; statusLine is not Governor's legacy one (none is \
                 configured, or a foreign one was left in place by `install`, or the slot \
                 is routed through the shared statusline host — see docs/statusline.md)",
                path.display()
            ),
        }
    } else {
        Finding {
            id: "claude_settings",
            severity: Severity::Ok,
            message: format!("hooks and statusline wired in {}", path.display()),
        }
    }
}

/// The `codex_hooks.json` mirror of [`claude_settings_finding`]
/// (HORO-1157): whether `~/.codex/hooks.json` (or
/// `$CODEX_HOME/hooks.json`) has this integration's three hook groups
/// wired, whether the wired command path matches the binary currently
/// running this `doctor` invocation (a moved binary needs re-trust —
/// Codex's trust gate is keyed by content hash, so a relocated binary is
/// silently a *different* hash from Codex's point of view even though
/// its content is identical), and a best-effort, read-only look at
/// `~/.codex/config.toml`'s `[hooks.state]` trust gate. See
/// [`codex_trust_state`] for why that last part is conservative by
/// design: never claim trusted when this doctor cannot confidently
/// parse the evidence for it.
fn codex_hooks_finding() -> Finding {
    let Ok(path) = codex_hooks_file::hooks_path() else {
        return Finding {
            id: "codex_hooks",
            severity: Severity::Warn,
            message: "could not resolve ~/.codex/hooks.json (HOME unset)".to_string(),
        };
    };
    let inspection = codex_hooks_file::inspect(&path);
    let all_hooks_wired = inspection.hooks_wired.iter().all(|w| *w);
    let no_hooks_wired = inspection.hooks_wired.iter().all(|w| !*w);

    if !inspection.file_present || !all_hooks_wired {
        return Finding {
            id: "codex_hooks",
            severity: Severity::Warn,
            message: if !inspection.file_present || no_hooks_wired {
                format!(
                    "not installed into {} — run `libra-governor install --agent codex`",
                    path.display()
                )
            } else {
                format!(
                    "only some of UserPromptSubmit/PostToolUse/Stop are wired in {} — \
                     re-run `libra-governor install --agent codex`",
                    path.display()
                )
            },
        };
    }

    let binary_matches = wired_command_matches_current_binary(&path);
    let trust = codex_home_config_dir()
        .map(|dir| codex_trust_state(&dir.join("config.toml")))
        .unwrap_or(CodexTrustState::Unknown(
            "could not resolve $CODEX_HOME".to_string(),
        ));

    let mut message = format!("hooks wired in {}", path.display());
    if !binary_matches {
        message.push_str(
            "; WARNING: the wired command path does not match the binary running this doctor \
             check — if you moved the binary, re-run `libra-governor install --agent codex` \
             and re-trust with `/hooks` inside Codex",
        );
    }
    message.push_str(&format!("; trust state: {trust}"));

    Finding {
        id: "codex_hooks",
        severity: if binary_matches {
            Severity::Ok
        } else {
            Severity::Warn
        },
        message,
    }
}

/// `true` if every wired hook command in `hooks_path` names the exact
/// binary currently running this `doctor` invocation. `false` (never a
/// hard error) when the check cannot be completed at all — a diagnostic
/// must degrade, never crash.
fn wired_command_matches_current_binary(hooks_path: &Path) -> bool {
    let Ok(current_exe) = std::env::current_exe() else {
        return true; // can't check — don't manufacture a false warning
    };
    let Ok(text) = std::fs::read_to_string(hooks_path) else {
        return true;
    };
    let Ok(root) = serde_json::from_str::<serde_json::Value>(&text) else {
        return true;
    };
    // Hook groups live under the top-level "hooks" key — see
    // codex_hooks_file's module docs on the real, verified file shape.
    let Some(hooks) = root.get("hooks") else {
        return true;
    };
    let current = current_exe.display().to_string();
    for event in ["UserPromptSubmit", "PostToolUse", "Stop"] {
        let Some(entries) = hooks.get(event).and_then(serde_json::Value::as_array) else {
            continue;
        };
        for matcher in entries {
            let Some(inner) = matcher.get("hooks").and_then(serde_json::Value::as_array) else {
                continue;
            };
            for hook in inner {
                let Some(command) = hook.get("command").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                if command.contains("libra-governor")
                    && command.ends_with(&format!(" codex-hook {}", subcommand_for(event)))
                    && !command.starts_with(&current)
                {
                    return false;
                }
            }
        }
    }
    true
}

fn subcommand_for(event: &str) -> &'static str {
    match event {
        "UserPromptSubmit" => "user-prompt-submit",
        "PostToolUse" => "post-tool-use",
        "Stop" => "stop",
        _ => "",
    }
}

/// Resolution order matching `codex_hooks_file::codex_home` exactly, so
/// `doctor`'s trust-state check always looks at the same directory
/// `install --agent codex` wrote `hooks.json` into.
fn codex_home_config_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("LIBRA_GOVERNOR_CODEX_HOME") {
        return Some(PathBuf::from(dir));
    }
    if let Ok(dir) = std::env::var("CODEX_HOME") {
        return Some(PathBuf::from(dir));
    }
    std::env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".codex"))
}

/// Codex's `hooks.json` trust gate: writing `hooks.json` is not enough
/// for Codex to actually run a hook — the user must run `/hooks` inside
/// Codex to trust each hook by content hash, recorded in
/// `~/.codex/config.toml`'s `[hooks.state]`. This repo adds no TOML
/// parser dependency to read that file (none existed in the workspace
/// before HORO-1157, and the ticket's own scope explicitly avoids adding
/// one) — this is a minimal, conservative text scan, not a real parser,
/// and it is documented as such rather than pretending to be exact.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CodexTrustState {
    /// `[features]` with a literal `hooks = false` line was found — an
    /// unambiguous text match regardless of TOML nesting/quoting
    /// subtleties.
    HooksDisabled,
    /// No `[hooks.state]` section header was found anywhere in the file
    /// — reasonably strong (if not proof-level) evidence the user has
    /// not yet run `/hooks`, since Codex has nowhere else to record
    /// trust.
    LikelyNotYetTrusted,
    /// A `[hooks.state]` section header was found, but this doctor does
    /// not know its real key shape (unverified during HORO-1157 — see
    /// `docs/adr/0004-agent-adapter-contract.md`), so it makes no claim
    /// about *what* is trusted inside it. Carries the reason.
    Unknown(String),
}

impl std::fmt::Display for CodexTrustState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodexTrustState::HooksDisabled => {
                write!(
                    f,
                    "hooks disabled in Codex config ([features] hooks = false)"
                )
            }
            CodexTrustState::LikelyNotYetTrusted => write!(
                f,
                "likely not yet trusted — no [hooks.state] section found in config.toml; run \
                 `/hooks` inside Codex"
            ),
            CodexTrustState::Unknown(reason) => write!(f, "unknown — {reason}"),
        }
    }
}

/// Reads `config_toml_path` (never writes) and returns a conservative
/// [`CodexTrustState`]. Never claims a hook is actually trusted: this
/// doctor cannot confidently parse `[hooks.state]`'s real shape, so the
/// honest answer when that section exists is "unknown", not "trusted" —
/// see the type's docs.
fn codex_trust_state(config_toml_path: &Path) -> CodexTrustState {
    let Ok(text) = std::fs::read_to_string(config_toml_path) else {
        return CodexTrustState::Unknown(format!(
            "{} not found or unreadable",
            config_toml_path.display()
        ));
    };

    if let Some(features_section) = toml_section_body(&text, "features") {
        if features_section
            .lines()
            .any(|line| line.trim().replace(' ', "") == "hooks=false")
        {
            return CodexTrustState::HooksDisabled;
        }
    }

    if text
        .lines()
        .any(|line| line.trim() == "[hooks.state]" || line.trim().starts_with("[hooks.state."))
    {
        return CodexTrustState::Unknown(
            "a [hooks.state] section exists but its exact key shape was not verified — verify \
             manually with `/hooks` inside Codex"
                .to_string(),
        );
    }

    CodexTrustState::LikelyNotYetTrusted
}

/// Returns the raw text between a `[section]` header and the next
/// top-level `[...]` header (or end of file), or `None` if the header is
/// never found. A minimal, line-oriented scan — not a TOML parser (see
/// [`codex_trust_state`]'s docs on why none is used): does not handle
/// quoted section names, inline tables, or nested `[section.sub]`
/// headers as members of `section`.
fn toml_section_body<'a>(text: &'a str, section: &str) -> Option<&'a str> {
    let header = format!("[{section}]");
    let start = text.lines().position(|line| line.trim() == header)?;
    let lines: Vec<&str> = text.lines().collect();
    let body_start: usize = lines[..=start].iter().map(|l| l.len() + 1).sum();
    let end_offset = lines[start + 1..]
        .iter()
        .position(|line| line.trim_start().starts_with('['))
        .map(|rel| {
            body_start
                + lines[start + 1..start + 1 + rel]
                    .iter()
                    .map(|l| l.len() + 1)
                    .sum::<usize>()
        })
        .unwrap_or(text.len());
    text.get(body_start.min(text.len())..end_offset.min(text.len()))
}

/// Connects to an already-running daemon and asks `Request::Doctor`.
/// Never spawns one. `None` (not an error) if the daemon is not
/// reachable at all — that becomes its own [`Finding`] via
/// [`daemon_findings`].
fn fetch_daemon_doctor() -> Option<Result<DoctorResult, String>> {
    let socket_path = libra_governor_daemon::paths::socket_path().ok()?;
    let stream = client::connect_only(&socket_path).ok()?;
    Some(match client::roundtrip(&stream, Request::Doctor) {
        Ok(Response::Doctor(result)) => Ok(*result),
        Ok(other) => Err(format!("unexpected daemon response: {other:?}")),
        Err(e) => Err(e.to_string()),
    })
}

fn daemon_findings(daemon: &Option<Result<DoctorResult, String>>) -> Vec<Finding> {
    let mut findings = Vec::new();
    match daemon {
        None => findings.push(Finding {
            id: "daemon_reachable",
            severity: Severity::Warn,
            message: "daemon is not running — it starts on demand on the next Claude Code \
                      prompt (this is normal)"
                .to_string(),
        }),
        Some(Err(message)) => findings.push(Finding {
            id: "daemon_reachable",
            severity: Severity::Error,
            message: format!(
                "daemon reachable but returned an error: {message} — if this mentions a \
                 protocol version mismatch (this binary was upgraded while an older daemon \
                 was still running), the daemon has already shut itself down after replying \
                 to this request; the next hook invocation or `doctor` run will spawn a \
                 fresh one automatically. To force it immediately instead of waiting: \
                 libra-governor daemon stop"
            ),
        }),
        Some(Ok(result)) => {
            findings.push(Finding {
                id: "daemon_reachable",
                severity: Severity::Ok,
                message: format!(
                    "daemon {} reachable, protocol v{}",
                    result.daemon_version, result.protocol_version
                ),
            });
            findings.push(Finding {
                id: "schema_version",
                severity: if result.schema_ahead_of_binary {
                    Severity::Error
                } else {
                    Severity::Ok
                },
                message: if result.schema_ahead_of_binary {
                    format!(
                        "ledger schema v{} is ahead of this daemon build's known v{} — \
                         upgrade the daemon binary",
                        result.schema_version_applied, result.schema_version_known
                    )
                } else {
                    format!(
                        "ledger schema v{} matches this daemon build",
                        result.schema_version_applied
                    )
                },
            });
            if result.config_file_present && !result.config_file_valid {
                findings.push(Finding {
                    id: "config_file_valid",
                    severity: Severity::Error,
                    message: format!(
                        "config.json is present but was rejected at daemon startup: {} — \
                         the daemon is running on its hardcoded defaults",
                        result
                            .config_file_error
                            .clone()
                            .unwrap_or_else(|| "(no detail)".to_string())
                    ),
                });
            }
            findings.push(Finding {
                id: "policy_preset",
                severity: Severity::Ok,
                message: format!("admission policy preset: {}", result.policy_preset),
            });
            if !result.running_config_matches_disk {
                findings.push(Finding {
                    id: "stale_config",
                    severity: Severity::Error,
                    message: "config.json on disk no longer matches what this running daemon \
                              loaded at startup — restart it to pick up the change: \
                              libra-governor daemon stop"
                        .to_string(),
                });
            }
            if result.gateway_configured {
                findings.push(Finding {
                    id: "gateway",
                    severity: if result.gateway_running {
                        Severity::Ok
                    } else {
                        Severity::Error
                    },
                    message: if result.gateway_running {
                        format!(
                            "gateway configured and running ({:?})",
                            result.gateway_capabilities.as_ref().map(|c| c.tier)
                        )
                    } else {
                        format!(
                            "gateway configured but not running: {}",
                            result
                                .gateway_disabled_reason
                                .clone()
                                .unwrap_or_else(|| "(no reason given)".to_string())
                        )
                    },
                });
            }
            findings.push(Finding {
                id: "telemetry",
                severity: if result.telemetry_enabled {
                    Severity::Error
                } else {
                    Severity::Ok
                },
                message: "telemetry: off (local-only; no telemetry code path exists)".to_string(),
            });
        }
    }
    findings
}

fn render_human(findings: &[Finding], _daemon: Option<&Result<DoctorResult, String>>) -> String {
    let mut out = String::new();
    out.push_str("libra-governor doctor\n");
    out.push_str("======================\n\n");
    for finding in findings {
        out.push_str(&format!(
            "[{}] {}: {}\n",
            finding.severity.label(),
            finding.id,
            finding.message
        ));
    }
    out
}

fn render_json(findings: &[Finding], daemon: Option<&Result<DoctorResult, String>>) -> String {
    let findings_json: Vec<serde_json::Value> = findings
        .iter()
        .map(|f| {
            serde_json::json!({
                "id": f.id,
                "severity": f.severity.label(),
                "message": f.message,
            })
        })
        .collect();
    let daemon_json = match daemon {
        Some(Ok(result)) => serde_json::to_value(result).unwrap_or(serde_json::Value::Null),
        _ => serde_json::Value::Null,
    };
    let out = serde_json::json!({
        "findings": findings_json,
        "daemon": daemon_json,
    });
    serde_json::to_string_pretty(&out).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(id: &'static str) -> Finding {
        Finding {
            id,
            severity: Severity::Ok,
            message: "fine".to_string(),
        }
    }

    fn error(id: &'static str) -> Finding {
        Finding {
            id,
            severity: Severity::Error,
            message: "broken".to_string(),
        }
    }

    #[test]
    fn human_render_lists_every_finding() {
        let findings = vec![ok("a"), error("b")];
        let rendered = render_human(&findings, None);
        assert!(rendered.contains("[ok] a"));
        assert!(rendered.contains("[error] b"));
    }

    #[test]
    fn json_render_is_valid_json_with_a_findings_array() {
        let findings = vec![ok("a")];
        let rendered = render_json(&findings, None);
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(value["findings"][0]["id"], "a");
        assert_eq!(value["findings"][0]["severity"], "ok");
    }

    #[test]
    fn a_daemon_unreachable_finding_is_a_warning_not_an_error() {
        let findings = daemon_findings(&None);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Warn);
    }

    #[test]
    fn a_protocol_mismatch_error_string_becomes_an_error_finding_with_the_restart_fix() {
        let findings = daemon_findings(&Some(Err("protocol version mismatch".to_string())));
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Error);
        assert!(findings[0].message.contains("libra-governor daemon stop"));
    }

    #[test]
    fn trust_state_reports_disabled_when_features_hooks_is_literally_false() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[features]\nhooks = false\nother = true\n").unwrap();
        assert_eq!(codex_trust_state(&path), CodexTrustState::HooksDisabled);
    }

    #[test]
    fn trust_state_reports_likely_not_yet_trusted_when_no_hooks_state_section_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[some_other_section]\nfoo = \"bar\"\n").unwrap();
        assert_eq!(
            codex_trust_state(&path),
            CodexTrustState::LikelyNotYetTrusted
        );
    }

    #[test]
    fn trust_state_never_claims_trusted_when_hooks_state_section_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[hooks.state]\nsome-hash = \"trusted\"\nanother-hash = \"trusted\"\n",
        )
        .unwrap();
        let state = codex_trust_state(&path);
        assert!(
            matches!(state, CodexTrustState::Unknown(_)),
            "must never claim a confidently-parsed trust verdict for an unverified shape: {state}"
        );
    }

    #[test]
    fn trust_state_reports_unknown_when_config_toml_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.toml");
        assert!(matches!(
            codex_trust_state(&path),
            CodexTrustState::Unknown(_)
        ));
    }

    #[test]
    fn toml_section_body_extracts_only_the_named_section() {
        let text = "[a]\nx = 1\n\n[b]\ny = 2\n";
        let body = toml_section_body(text, "a").unwrap();
        assert!(body.contains("x = 1"));
        assert!(!body.contains("y = 2"));
    }

    #[test]
    fn toml_section_body_returns_none_when_the_section_is_absent() {
        let text = "[a]\nx = 1\n";
        assert!(toml_section_body(text, "z").is_none());
    }

    /// Regression for the exact scenario `stale_runtime_finding` exists to
    /// catch: a protocol-version-mismatch response still means *something*
    /// answered this request on the daemon's socket just now — `run`'s
    /// `daemon_present` must stay `true` for a `Some(Err(_))`, not just a
    /// `Some(Ok(_))`, or the single most common upgrade-while-running path
    /// (see `daemon_findings`'s own protocol-mismatch message above) would
    /// silently suppress the finding.
    #[test]
    fn daemon_present_is_true_even_when_the_daemon_replied_with_a_protocol_mismatch_error() {
        let daemon: Option<Result<DoctorResult, String>> =
            Some(Err("protocol version mismatch".to_string()));
        assert!(daemon.is_some());
    }

    fn write_pid_record(dir: &Path, exe_sha256: Option<&str>) {
        libra_governor_daemon::pidfile::write(
            dir,
            &libra_governor_daemon::pidfile::PidRecord {
                pid: std::process::id(),
                exe_path: std::path::PathBuf::from("/does/not/matter"),
                started_at: "2026-09-27T00:00:00Z".to_string(),
                exe_sha256: exe_sha256.map(str::to_string),
            },
        )
        .unwrap();
    }

    fn write_install_marker(dir: &Path, binary_path: &std::path::Path) {
        std::fs::write(
            dir.join(install_cmd::INSTALL_MARKER_FILE_NAME),
            serde_json::json!({
                "installed_by": "libra-governor",
                "version": "0.0.0",
                "binary_path": binary_path.display().to_string(),
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn stale_runtime_finding_fires_on_a_genuine_binary_sha_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("libra-governor");
        std::fs::write(&binary, b"new binary bytes").unwrap();

        // The pidfile records a hash that does not match what is on disk
        // at `binary_path` right now — exactly the "upgraded while the
        // daemon is running" scenario.
        write_pid_record(dir.path(), Some("not-the-real-hash"));
        write_install_marker(dir.path(), &binary);

        let finding = stale_runtime_finding(dir.path(), true).expect("must report a mismatch");
        assert_eq!(finding.id, "stale_runtime");
        assert_eq!(finding.severity, Severity::Error);
        assert!(finding.message.contains("daemon stop"));
    }

    #[test]
    fn stale_runtime_finding_is_none_when_hashes_match() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("libra-governor");
        std::fs::write(&binary, b"same binary bytes").unwrap();
        let sha = libra_governor_daemon::pidfile::hash_file(&binary).unwrap();

        write_pid_record(dir.path(), Some(&sha));
        write_install_marker(dir.path(), &binary);

        assert!(stale_runtime_finding(dir.path(), true).is_none());
    }

    #[test]
    fn stale_runtime_finding_is_none_when_daemon_is_not_reachable() {
        // Guards the exact false-positive this check exists to avoid: a
        // pidfile deliberately outlives a signalled daemon (no
        // "remove on clean exit" — see `pidfile`'s module docs), so after
        // an upgrade-then-`daemon stop` the old hash is still on disk with
        // no daemon actually running. That must never be reported.
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("libra-governor");
        std::fs::write(&binary, b"new binary bytes").unwrap();
        write_pid_record(dir.path(), Some("stale-hash-from-old-binary"));
        write_install_marker(dir.path(), &binary);

        assert!(stale_runtime_finding(dir.path(), false).is_none());
    }

    #[test]
    fn stale_runtime_finding_is_none_when_pidfile_predates_exe_sha256() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("libra-governor");
        std::fs::write(&binary, b"new binary bytes").unwrap();
        write_pid_record(dir.path(), None);
        write_install_marker(dir.path(), &binary);

        assert!(stale_runtime_finding(dir.path(), true).is_none());
    }

    #[test]
    fn stale_runtime_finding_is_none_when_no_install_marker_exists() {
        let dir = tempfile::tempdir().unwrap();
        write_pid_record(dir.path(), Some("some-hash"));
        assert!(stale_runtime_finding(dir.path(), true).is_none());
    }

    #[test]
    fn orphan_socket_finding_is_none_when_no_socket_exists() {
        let dir = tempfile::tempdir().unwrap();
        assert!(orphan_socket_finding(dir.path(), false).is_none());
    }

    #[test]
    fn orphan_socket_finding_is_none_when_a_usable_pid_record_exists() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("daemon.sock"), b"").unwrap();
        write_pid_record(dir.path(), Some("some-hash"));
        assert!(orphan_socket_finding(dir.path(), true).is_none());
    }

    #[test]
    fn orphan_socket_finding_warns_with_manual_removal_guidance_when_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("daemon.sock"), b"").unwrap();
        let finding = orphan_socket_finding(dir.path(), false).expect("must report the gap");
        assert_eq!(finding.id, "orphan_socket");
        assert_eq!(finding.severity, Severity::Warn);
        assert!(finding.message.contains("ps"));
        assert!(finding.message.contains("lsof"));
    }

    #[test]
    fn orphan_socket_finding_warns_without_telling_the_operator_to_delete_a_live_socket() {
        // A live daemon predating the pidfile feature must never be told
        // to have its socket deleted out from under it.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("daemon.sock"), b"").unwrap();
        let finding = orphan_socket_finding(dir.path(), true).expect("must report the gap");
        assert_eq!(finding.id, "orphan_socket");
        assert_eq!(finding.severity, Severity::Warn);
        assert!(finding.message.contains("predates"));
        assert!(!finding.message.contains("remove the stale socket"));
    }

    #[test]
    fn execution_identity_capture_is_truthful_about_partial_adoption() {
        let finding = execution_identity_capture_finding();
        assert_eq!(finding.id, "execution_identity_capture");
        assert_eq!(finding.severity, Severity::Ok);
        assert!(finding.message.contains("session: captured"));
        assert!(finding.message.contains("agent/turn: not captured"));
        assert!(finding.message.contains("task: not captured"));
        assert!(finding.message.contains("TaskId"));
        assert!(finding.message.contains("host: implicit"));
        assert!(finding
            .message
            .contains("#partial-adoption-is-expected-not-a-defect"));
        // Never claim a dimension is captured when it isn't -- the exact
        // fabricated-precision failure this check exists to avoid.
        assert!(!finding.message.contains("agent: captured"));
        assert!(!finding.message.contains("turn: captured"));
        assert!(!finding.message.contains("task: captured"));
    }
}
