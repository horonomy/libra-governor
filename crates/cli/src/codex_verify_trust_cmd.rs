//! `libra-governor doctor --verify-trust` (HORO-1906, ADR-0015 Option A).
//!
//! The **one** explicit, operator-invoked command in this binary that
//! starts Codex's `app-server` process. Everything else in this crate —
//! including plain `doctor`, `doctor --json`, `--help`, and every other
//! passive/inspection surface — must never reach
//! [`crate::codex_app_server_client::AppServerClient::spawn`]; see
//! `tests/codex_verify_trust_never_implicit.rs` for the regression test
//! that proves it (ADR-0015 requirement 1/2).
//!
//! # What this reports
//!
//! For each of this integration's three Codex hooks
//! (`UserPromptSubmit`/`PostToolUse`/`Stop`) that `codex_hooks_file`
//! reports as locally wired — using the *actual* command string
//! recorded in `hooks.json` ([`codex_hooks_file::wired_commands`]), not
//! a reconstruction from the currently-running binary's own path, which
//! can differ from what's on disk if the binary moved since `install`
//! last ran — this command asks Codex's own `hooks/list` RPC for that
//! hook's real `currentHash`/`trustStatus`, matched by
//! `(eventName, command, sourcePath)`.
//!
//! # Fail-closed semantics (ADR-0015 requirement 6)
//!
//! - `app-server` unreachable (spawn failure) → every wired hook reports
//!   `Unknown`.
//! - RPC timeout → `Unknown`.
//! - Malformed/truncated RPC response → `Unknown`, never a crash.
//! - A hook wired locally but absent from the RPC response (orphaned
//!   entry) → `Unknown` for that hook specifically, reported by name,
//!   never silently dropped and never reported trusted.
//! - More than one RPC entry matches the same local hook and they
//!   disagree on `trustStatus`/`currentHash` (conflicting evidence) →
//!   `Unknown`, never picked arbitrarily.
//! - The RPC response records a parse/load `error` against the source
//!   document (`hooks.json`) a matched hook came from → `Unknown`,
//!   never trusted on top of evidence Codex itself flagged as suspect.
//! - Codex reports `enabled: false` for an otherwise-trusted hook →
//!   `Unknown` (a disabled hook will not actually run even if its
//!   content hash is trusted — reporting "trusted" here would be a true
//!   statement about the hash and a misleading one about enforcement).
//! - Codex's own `"untrusted"`/`"modified"`/`"managed"` values are
//!   relayed verbatim (they are Codex's authoritative answer, not an
//!   absence of one) — only Codex's own literal `"trusted"` (with
//!   `enabled` not `false` and no conflicting/errored evidence) is ever
//!   rendered as trusted.
//!
//! # No side effects (ADR-0015 requirement 4/5)
//!
//! `hooks/list` is a read-only Codex RPC — this command never writes
//! `hooks.json`, `config.toml`, or any other persistent Libra Governor
//! state, never grants trust, never executes a hook, and makes no
//! network call or model-spend of its own *in its own request/response
//! handling*. See the PR description's security-review section for one
//! residual risk this ticket's own evidence surfaced: starting Codex's
//! `app-server` itself (independent of which RPC is then called, and
//! entirely Codex's own behavior, not anything added here) performs its
//! own internal SQLite/state-file bootstrapping in `$CODEX_HOME` —
//! observed empirically against both an isolated synthetic home and the
//! operator's real `~/.codex` during this ticket's evidence-gathering.
//! That means ADR-0015's own requirement 5 ("no persistent config
//! change... as part of resolving host_trust") is not cleanly
//! satisfiable by Option A as literally specified against this Codex
//! version — flagged here rather than silently asserted clean.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::codex_app_server_client::{AppServerClient, AppServerLatency};
use crate::codex_hooks_file;

/// Bounds each individual RPC call (`initialize`, `hooks/list`)
/// separately — generous for an explicit, operator-invoked diagnostic
/// command that is not a hot path (ADR-0015's own Option A analysis:
/// "not expected to be prohibitive; this is not a hot path"), but still
/// finite so an unreachable/hostile app-server cannot hang the operator
/// forever.
const DEFAULT_CALL_DEADLINE: Duration = Duration::from_secs(10);

/// Fixed argv for the one process this command ever spawns. Not
/// operator- or environment-overridable: unlike the binary path itself
/// (a legitimate "which Codex installation" override, below), an
/// argv-injection seam here would let anything that can set env vars in
/// this process's environment change what a security-reviewed process
/// launch actually runs.
const APP_SERVER_ARGS: [&str; 3] = ["app-server", "--listen", "stdio://"];

const HOOK_EVENTS: [(&str, &str); 3] = [
    ("UserPromptSubmit", "userPromptSubmit"),
    ("PostToolUse", "postToolUse"),
    ("Stop", "stop"),
];

#[derive(Debug, Clone, PartialEq)]
pub enum HookTrust {
    /// Codex's own `hooks/list` reported `trustStatus: "trusted"`,
    /// `enabled` not `false`, no conflicting duplicate entry, and no
    /// recorded source error, for this exact hook.
    Trusted { current_hash: String },
    /// Codex returned a real, non-"trusted" status for this hook —
    /// relayed verbatim (`"untrusted"`, `"modified"`, or `"managed"`),
    /// not a failure of this command.
    CodexReported {
        trust_status: String,
        current_hash: String,
    },
    /// This command could not obtain an authoritative "yes, trusted"
    /// answer: app-server unreachable, RPC timeout, malformed response,
    /// the hook wired locally but absent from Codex's response
    /// (orphaned entry), conflicting duplicate entries, a recorded
    /// source error, or `enabled: false`. Never defaults to trusted.
    Unknown { reason: String },
}

impl HookTrust {
    pub fn is_trusted(&self) -> bool {
        matches!(self, HookTrust::Trusted { .. })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HookReport {
    pub event: &'static str,
    pub trust: HookTrust,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct VerifyTrustReport {
    pub hooks: Vec<HookReport>,
    pub latency: Option<AppServerLatency>,
}

/// Everything [`verify`] needs, resolved once up front so the function
/// itself never touches `std::env`/`current_exe` directly — tests
/// construct this struct with fixed, explicit values (mirroring
/// `codex_hooks_file`'s own test convention of a fixed
/// `/opt/libra/bin/libra-governor`-style path) instead of racing on
/// process-global environment variables.
pub struct VerifyConfig {
    pub hooks_path: PathBuf,
    pub codex_binary: String,
    pub codex_home: Option<PathBuf>,
    pub deadline: Duration,
}

/// Resolves [`VerifyConfig`] from the real environment:
/// `codex_hooks_file::hooks_path`/`codex_home` (which already honor
/// `LIBRA_GOVERNOR_CODEX_HOME` for tests, else `$CODEX_HOME`, else
/// `$HOME/.codex` — the operator's **real** home by default, per
/// ADR-0015 requirement 3), and `codex` on `PATH` unless
/// `LIBRA_GOVERNOR_CODEX_BINARY` names a specific installation.
fn resolve_config() -> Option<VerifyConfig> {
    let hooks_path = codex_hooks_file::hooks_path().ok()?;
    let codex_home = codex_hooks_file::codex_home().ok();
    let codex_binary =
        std::env::var("LIBRA_GOVERNOR_CODEX_BINARY").unwrap_or_else(|_| "codex".to_string());
    Some(VerifyConfig {
        hooks_path,
        codex_binary,
        codex_home,
        deadline: DEFAULT_CALL_DEADLINE,
    })
}

pub fn run(json: bool) -> i32 {
    let Some(config) = resolve_config() else {
        eprintln!(
            "libra-governor doctor --verify-trust: could not resolve $CODEX_HOME (HOME unset)"
        );
        return 1;
    };
    let report = verify(&config);
    render(&report, json);
    // A hook reporting Unknown/untrusted is real, honest diagnostic
    // output, not a tool failure — this command's own job succeeded by
    // producing it. Exit 1 only when there was nothing to check at all.
    if report.hooks.is_empty() {
        1
    } else {
        0
    }
}

fn unknown_report(wired: &[(&'static str, String)], reason: String) -> VerifyTrustReport {
    VerifyTrustReport {
        hooks: wired
            .iter()
            .map(|(event, _)| HookReport {
                event,
                trust: HookTrust::Unknown {
                    reason: reason.clone(),
                },
            })
            .collect(),
        latency: None,
    }
}

fn verify(config: &VerifyConfig) -> VerifyTrustReport {
    let wired_commands = codex_hooks_file::wired_commands(&config.hooks_path);
    let wired: Vec<(&'static str, String)> = HOOK_EVENTS
        .iter()
        .zip(wired_commands)
        .filter_map(|((event, _), command)| command.map(|c| (*event, c)))
        .collect();

    if wired.is_empty() {
        return VerifyTrustReport::default();
    }

    let mut env_pairs: Vec<(&str, &OsStr)> = Vec::new();
    let codex_home_os = config.codex_home.as_deref().map(Path::as_os_str);
    if let Some(home) = codex_home_os {
        env_pairs.push(("CODEX_HOME", home));
    }

    let spawned = AppServerClient::spawn(&config.codex_binary, &APP_SERVER_ARGS, &env_pairs);
    let (mut client, spawn_elapsed) = match spawned {
        Ok(pair) => pair,
        Err(e) => {
            return unknown_report(&wired, format!("could not reach Codex app-server: {e}"));
        }
    };

    let client_info = serde_json::json!({
        "clientInfo": {
            "name": "libra-governor",
            "version": env!("CARGO_PKG_VERSION"),
        }
    });
    let init_result = client.call("initialize", client_info, config.deadline);
    let init_elapsed = match &init_result {
        Ok((_, elapsed)) => *elapsed,
        Err(_) => Duration::ZERO,
    };
    if let Err(e) = init_result {
        client.kill();
        return unknown_report(
            &wired,
            format!("Codex app-server did not respond to initialize: {e}"),
        );
    }

    let hooks_result = client.call("hooks/list", serde_json::json!({}), config.deadline);
    client.kill();

    let (hooks_value, hooks_elapsed) = match hooks_result {
        Ok((value, elapsed)) => (value, elapsed),
        Err(e) => {
            return unknown_report(
                &wired,
                format!("Codex app-server did not respond to hooks/list: {e}"),
            );
        }
    };

    let remote_hooks = extract_hook_entries(&hooks_value);
    let hooks_path_str = config.hooks_path.display().to_string();

    let hooks = wired
        .iter()
        .map(|(event, expected_command)| {
            let rpc_event_name = HOOK_EVENTS
                .iter()
                .find(|(e, _)| e == event)
                .map(|(_, rpc)| *rpc)
                .unwrap_or_default();
            let matches: Vec<&RemoteHookEntry> = remote_hooks
                .iter()
                .filter(|entry| {
                    entry.event_name.as_deref() == Some(rpc_event_name)
                        && entry.command.as_deref() == Some(expected_command.as_str())
                        && entry.source_path.as_deref() == Some(hooks_path_str.as_str())
                })
                .collect();

            let trust = resolve_trust(&matches);
            HookReport { event, trust }
        })
        .collect();

    VerifyTrustReport {
        hooks,
        latency: Some(AppServerLatency {
            spawn: spawn_elapsed,
            initialize: init_elapsed,
            hooks_list: hooks_elapsed,
        }),
    }
}

/// Resolves one hook's trust from the (possibly empty, possibly
/// multi-element) set of RPC entries that matched it by
/// `(eventName, command, sourcePath)`. Every branch that is not a clean
/// single trusted answer resolves to [`HookTrust::Unknown`] or a
/// verbatim [`HookTrust::CodexReported`] — never a guess at trusted.
fn resolve_trust(matches: &[&RemoteHookEntry]) -> HookTrust {
    match matches {
        [] => HookTrust::Unknown {
            reason: "wired in hooks.json but absent from Codex's hooks/list response (orphaned \
                     entry) — Codex may not see this hook, or its command path changed"
                .to_string(),
        },
        [entry] => resolve_single_entry(entry),
        many => {
            let first = &many[0];
            // Every field this function's trust decision depends on must
            // agree, not just trust_status/current_hash — a security
            // review (HORO-1906) found that comparing only those two
            // fields let a second matching entry's `enabled: false` or
            // non-empty `source_errors` be silently dropped whenever
            // `first` happened to be the clean/enabled one, which could
            // report Trusted on conflicting evidence. Compare the whole
            // entry (`RemoteHookEntry` derives `PartialEq`) so any
            // disagreement on any field is conflicting evidence.
            let all_agree = many.iter().all(|e| e == first);
            if all_agree {
                resolve_single_entry(first)
            } else {
                HookTrust::Unknown {
                    reason: format!(
                        "{} entries in Codex's hooks/list response matched this hook but \
                         disagree on trust state — conflicting evidence",
                        many.len()
                    ),
                }
            }
        }
    }
}

fn resolve_single_entry(entry: &RemoteHookEntry) -> HookTrust {
    if !entry.source_errors.is_empty() {
        return HookTrust::Unknown {
            reason: format!(
                "Codex reported load errors for this hook's source file: {}",
                entry.source_errors.join("; ")
            ),
        };
    }
    // Fail closed: trusted requires an explicit `enabled: true`, not
    // merely the absence of `enabled: false`. A security review
    // (HORO-1906) found that a missing `enabled` field (schema drift, or
    // a non-standard responder) fell through to the trust_status check
    // below and could report Trusted with enabled-ness genuinely
    // unknown — this treats "unknown" the same as "disabled" here,
    // matching the module's own doc claim that schema drift degrades to
    // Unknown.
    if entry.enabled != Some(true) {
        return HookTrust::Unknown {
            reason: format!(
                "Codex did not report this hook as enabled (enabled: {:?}) — even if trusted by \
                 hash, an entry that is not confirmed enabled may not actually run",
                entry.enabled
            ),
        };
    }
    match (&entry.trust_status, &entry.current_hash) {
        (Some(status), Some(hash)) if status == "trusted" => HookTrust::Trusted {
            current_hash: hash.clone(),
        },
        (Some(status), Some(hash)) => HookTrust::CodexReported {
            trust_status: status.clone(),
            current_hash: hash.clone(),
        },
        _ => HookTrust::Unknown {
            reason: "Codex's response entry was missing trustStatus or currentHash".to_string(),
        },
    }
}

#[derive(Debug, Clone, PartialEq)]
struct RemoteHookEntry {
    event_name: Option<String>,
    command: Option<String>,
    source_path: Option<String>,
    trust_status: Option<String>,
    current_hash: Option<String>,
    enabled: Option<bool>,
    /// Load/parse errors Codex itself recorded against the source
    /// document (`hooks.json`/`config.toml`) this hook came from — the
    /// enclosing `data[i].errors` array, not per-hook. Non-empty means
    /// Codex itself flagged this source as suspect.
    source_errors: Vec<String>,
}

/// Walks `hooks/list`'s real response shape
/// (`{"data": [{"cwd", "hooks": [...], "warnings", "errors"}, ...]}`,
/// verified against a real local Codex CLI install during this ticket's
/// evidence-gathering) into a flat list, tolerant of any missing/renamed
/// field — a schema drift here must degrade to `Unknown` per-hook
/// (handled by `resolve_trust`'s `None`/`_` match arms), never panic.
fn extract_hook_entries(value: &serde_json::Value) -> Vec<RemoteHookEntry> {
    let mut out = Vec::new();
    let Some(data) = value.get("data").and_then(serde_json::Value::as_array) else {
        return out;
    };
    for cwd_entry in data {
        let Some(hooks) = cwd_entry.get("hooks").and_then(serde_json::Value::as_array) else {
            continue;
        };
        let source_errors: Vec<String> = cwd_entry
            .get("errors")
            .and_then(serde_json::Value::as_array)
            .map(|errors| {
                errors
                    .iter()
                    .filter_map(|e| {
                        e.get("message")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string)
                    })
                    .collect()
            })
            .unwrap_or_default();
        for hook in hooks {
            out.push(RemoteHookEntry {
                event_name: hook
                    .get("eventName")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                command: hook
                    .get("command")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                source_path: hook
                    .get("sourcePath")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                trust_status: hook
                    .get("trustStatus")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                current_hash: hook
                    .get("currentHash")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                enabled: hook.get("enabled").and_then(serde_json::Value::as_bool),
                source_errors: source_errors.clone(),
            });
        }
    }
    out
}

fn render(report: &VerifyTrustReport, json: bool) {
    if json {
        let hooks: Vec<serde_json::Value> = report
            .hooks
            .iter()
            .map(|h| {
                let (status, detail) = match &h.trust {
                    HookTrust::Trusted { current_hash } => (
                        "trusted".to_string(),
                        serde_json::json!({"currentHash": current_hash}),
                    ),
                    HookTrust::CodexReported {
                        trust_status,
                        current_hash,
                    } => (
                        trust_status.clone(),
                        serde_json::json!({"currentHash": current_hash}),
                    ),
                    HookTrust::Unknown { reason } => {
                        ("unknown".to_string(), serde_json::json!({"reason": reason}))
                    }
                };
                serde_json::json!({
                    "event": h.event,
                    "status": status,
                    "detail": detail,
                })
            })
            .collect();
        let latency = report.latency.map(|l| {
            serde_json::json!({
                "spawnMs": l.spawn.as_secs_f64() * 1000.0,
                "initializeMs": l.initialize.as_secs_f64() * 1000.0,
                "hooksListMs": l.hooks_list.as_secs_f64() * 1000.0,
                "totalMs": l.total().as_secs_f64() * 1000.0,
            })
        });
        let out = serde_json::json!({"hooks": hooks, "latency": latency});
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
        return;
    }

    if report.hooks.is_empty() {
        println!(
            "libra-governor doctor --verify-trust: no Codex hooks are currently wired in \
             hooks.json — run `libra-governor install --agent codex` first"
        );
        return;
    }

    let trusted_count = report.hooks.iter().filter(|h| h.trust.is_trusted()).count();
    for hook in &report.hooks {
        match &hook.trust {
            HookTrust::Trusted { current_hash } => {
                println!("{}: trusted (hash {current_hash})", hook.event);
            }
            HookTrust::CodexReported {
                trust_status,
                current_hash,
            } => {
                println!("{}: {trust_status} (hash {current_hash})", hook.event);
            }
            HookTrust::Unknown { reason } => {
                println!("{}: unknown — {reason}", hook.event);
            }
        }
    }
    println!(
        "{trusted_count}/{} hooks reported trusted",
        report.hooks.len()
    );

    if let Some(latency) = report.latency {
        println!(
            "app-server latency: spawn {:.1}ms, initialize {:.1}ms, hooks/list {:.1}ms, total \
             {:.1}ms",
            latency.spawn.as_secs_f64() * 1000.0,
            latency.initialize.as_secs_f64() * 1000.0,
            latency.hooks_list.as_secs_f64() * 1000.0,
            latency.total().as_secs_f64() * 1000.0,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_hook_entries_parses_the_real_response_shape() {
        let value = serde_json::json!({
            "data": [{
                "cwd": "/some/dir",
                "hooks": [{
                    "key": "/x/hooks.json:stop:0:0",
                    "eventName": "stop",
                    "command": "/opt/libra-governor codex-hook stop",
                    "sourcePath": "/x/hooks.json",
                    "currentHash": "sha256:abc",
                    "trustStatus": "trusted",
                    "enabled": true
                }],
                "warnings": [],
                "errors": []
            }]
        });
        let entries = extract_hook_entries(&value);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].event_name.as_deref(), Some("stop"));
        assert_eq!(entries[0].trust_status.as_deref(), Some("trusted"));
        assert_eq!(entries[0].enabled, Some(true));
        assert!(entries[0].source_errors.is_empty());
    }

    #[test]
    fn extract_hook_entries_tolerates_a_missing_data_field() {
        let value = serde_json::json!({"unexpected": "shape"});
        assert!(extract_hook_entries(&value).is_empty());
    }

    #[test]
    fn extract_hook_entries_tolerates_a_hook_missing_every_field() {
        let value = serde_json::json!({
            "data": [{"cwd": "/x", "hooks": [{}], "warnings": [], "errors": []}]
        });
        let entries = extract_hook_entries(&value);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].event_name.is_none());
        assert!(entries[0].trust_status.is_none());
    }

    #[test]
    fn extract_hook_entries_collects_source_errors() {
        let value = serde_json::json!({
            "data": [{
                "cwd": "/x",
                "hooks": [{"eventName": "stop", "trustStatus": "trusted", "currentHash": "h"}],
                "warnings": [],
                "errors": [{"message": "failed to parse hooks config", "path": "/x/hooks.json"}]
            }]
        });
        let entries = extract_hook_entries(&value);
        assert_eq!(
            entries[0].source_errors,
            vec!["failed to parse hooks config".to_string()]
        );
    }

    #[test]
    fn hook_trust_is_trusted_only_for_the_trusted_variant() {
        assert!(HookTrust::Trusted {
            current_hash: "x".into()
        }
        .is_trusted());
        assert!(!HookTrust::CodexReported {
            trust_status: "trusted-looking-but-not".into(),
            current_hash: "x".into()
        }
        .is_trusted());
        assert!(!HookTrust::Unknown { reason: "x".into() }.is_trusted());
    }

    fn entry(
        event_name: &str,
        command: &str,
        source_path: &str,
        trust_status: &str,
        hash: &str,
    ) -> RemoteHookEntry {
        RemoteHookEntry {
            event_name: Some(event_name.to_string()),
            command: Some(command.to_string()),
            source_path: Some(source_path.to_string()),
            trust_status: Some(trust_status.to_string()),
            current_hash: Some(hash.to_string()),
            enabled: Some(true),
            source_errors: Vec::new(),
        }
    }

    #[test]
    fn resolve_trust_no_matches_is_unknown_orphaned() {
        let trust = resolve_trust(&[]);
        assert!(matches!(trust, HookTrust::Unknown { .. }));
        assert!(!trust.is_trusted());
    }

    #[test]
    fn resolve_trust_single_trusted_match_is_trusted() {
        let e = entry("stop", "/bin/x stop", "/x/hooks.json", "trusted", "h1");
        assert!(resolve_trust(&[&e]).is_trusted());
    }

    #[test]
    fn resolve_trust_conflicting_duplicate_matches_is_unknown() {
        let a = entry("stop", "/bin/x stop", "/x/hooks.json", "trusted", "h1");
        let b = entry("stop", "/bin/x stop", "/x/hooks.json", "untrusted", "h1");
        let trust = resolve_trust(&[&a, &b]);
        assert!(matches!(trust, HookTrust::Unknown { .. }));
    }

    // Regression test (security review, HORO-1906): agreement across
    // duplicate matches must be checked on the WHOLE entry, not just
    // trust_status/current_hash — otherwise a second matching entry's
    // `enabled: false` could be silently dropped whenever the first
    // matched entry happened to be the clean/enabled one.
    #[test]
    fn resolve_trust_duplicates_agreeing_on_hash_but_disagreeing_on_enabled_is_unknown() {
        let a = entry("stop", "/bin/x stop", "/x/hooks.json", "trusted", "h1");
        let mut b = entry("stop", "/bin/x stop", "/x/hooks.json", "trusted", "h1");
        b.enabled = Some(false);
        let trust = resolve_trust(&[&a, &b]);
        assert!(matches!(trust, HookTrust::Unknown { .. }), "{trust:?}");
        assert!(!trust.is_trusted());
        // Order must not matter — putting the disabled entry first must
        // not accidentally "win" either.
        let trust_reordered = resolve_trust(&[&b, &a]);
        assert!(matches!(trust_reordered, HookTrust::Unknown { .. }));
    }

    #[test]
    fn resolve_trust_agreeing_duplicate_matches_is_trusted() {
        let a = entry("stop", "/bin/x stop", "/x/hooks.json", "trusted", "h1");
        let b = entry("stop", "/bin/x stop", "/x/hooks.json", "trusted", "h1");
        assert!(resolve_trust(&[&a, &b]).is_trusted());
    }

    #[test]
    fn resolve_trust_disabled_hook_is_unknown_even_if_trusted() {
        let mut e = entry("stop", "/bin/x stop", "/x/hooks.json", "trusted", "h1");
        e.enabled = Some(false);
        let trust = resolve_trust(&[&e]);
        assert!(matches!(trust, HookTrust::Unknown { .. }));
        assert!(!trust.is_trusted());
    }

    // Regression test (security review, HORO-1906): a *missing*
    // `enabled` field (schema drift, or a non-standard responder) must
    // degrade to Unknown just like `enabled: false` does — trusted
    // requires an explicit `enabled: true`, not merely the absence of
    // `enabled: false`.
    #[test]
    fn resolve_trust_missing_enabled_field_is_unknown_even_if_trusted() {
        let mut e = entry("stop", "/bin/x stop", "/x/hooks.json", "trusted", "h1");
        e.enabled = None;
        let trust = resolve_trust(&[&e]);
        assert!(matches!(trust, HookTrust::Unknown { .. }), "{trust:?}");
        assert!(!trust.is_trusted());
    }

    #[test]
    fn resolve_trust_source_errors_is_unknown_even_if_trusted() {
        let mut e = entry("stop", "/bin/x stop", "/x/hooks.json", "trusted", "h1");
        e.source_errors = vec!["failed to parse hooks config".to_string()];
        let trust = resolve_trust(&[&e]);
        assert!(matches!(trust, HookTrust::Unknown { .. }));
        assert!(!trust.is_trusted());
    }

    // --- End-to-end `verify()` tests against a fake `codex` binary ---
    //
    // No env vars, no global mutable state: `VerifyConfig` is built
    // directly with a fixed, explicit binary/hooks-path, so every test
    // here can run fully in parallel. The real protocol shape (method
    // names, `hooks/list`'s response envelope, the match key) was
    // verified separately against a real local Codex CLI install during
    // this ticket's evidence-gathering (see the PR description), not
    // re-derived here.

    fn write_fixture(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-codex.sh");
        let mut file = std::fs::File::create(&path).unwrap();
        write!(file, "#!/bin/sh\n{body}").unwrap();
        drop(file);
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    const FIXTURE_BINARY: &str = "/opt/libra/bin/libra-governor";

    fn wire_real_hooks(hooks_path: &std::path::Path) {
        crate::codex_hooks_file::apply(hooks_path, std::path::Path::new(FIXTURE_BINARY)).unwrap();
    }

    fn expected_commands() -> [String; 3] {
        [
            format!("{FIXTURE_BINARY} codex-hook user-prompt-submit"),
            format!("{FIXTURE_BINARY} codex-hook post-tool-use"),
            format!("{FIXTURE_BINARY} codex-hook stop"),
        ]
    }

    /// A generous deadline for tests whose fixture script is expected to
    /// respond promptly — this machine's own `sh` fork/exec cost can
    /// spike well past 500ms under heavy parallel build/test load from
    /// sibling worktrees sharing CI/dev hosts, so a short deadline here
    /// would make a *correctness* test flaky for a *performance* reason
    /// unrelated to what it verifies. Only the dedicated timeout test
    /// uses a short deadline, deliberately, to exercise that path.
    const GENEROUS_TEST_DEADLINE: Duration = Duration::from_secs(10);

    fn config(dir: &std::path::Path, hooks_path: PathBuf, binary: String) -> VerifyConfig {
        VerifyConfig {
            hooks_path,
            codex_binary: binary,
            codex_home: Some(dir.to_path_buf()),
            deadline: GENEROUS_TEST_DEADLINE,
        }
    }

    #[test]
    fn positive_a_genuinely_trusted_hook_reports_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_path = dir.path().join("hooks.json");
        wire_real_hooks(&hooks_path);
        let [prompt, tool, stop] = expected_commands();
        let hooks_path_str = hooks_path.display().to_string();

        let script = write_fixture(
            dir.path(),
            &format!(
                "read -r _init\nprintf '{{\"id\":1,\"result\":{{\"ok\":true}}}}\\n'\nread -r \
                 _hooks\nprintf '{{\"id\":2,\"result\":{{\"data\":[{{\"cwd\":\"/x\",\"warnings\":[],\
                 \"errors\":[],\"hooks\":[{{\"eventName\":\"userPromptSubmit\",\"command\":\"{prompt}\",\
                 \"sourcePath\":\"{hooks_path_str}\",\"currentHash\":\"sha256:aaa\",\"trustStatus\":\"trusted\",\
                 \"enabled\":true}},{{\"eventName\":\"postToolUse\",\"command\":\"{tool}\",\
                 \"sourcePath\":\"{hooks_path_str}\",\"currentHash\":\"sha256:bbb\",\"trustStatus\":\"trusted\",\
                 \"enabled\":true}},{{\"eventName\":\"stop\",\"command\":\"{stop}\",\
                 \"sourcePath\":\"{hooks_path_str}\",\"currentHash\":\"sha256:ccc\",\"trustStatus\":\"trusted\",\
                 \"enabled\":true}}]}}]}}}}\\n'\n"
            ),
        );

        let cfg = config(dir.path(), hooks_path, script.display().to_string());
        let report = verify(&cfg);
        assert_eq!(report.hooks.len(), 3, "{:?}", report.hooks);
        assert!(
            report.hooks.iter().all(|h| h.trust.is_trusted()),
            "{:?}",
            report.hooks
        );
        assert!(report.latency.is_some());
    }

    #[test]
    fn negative_app_server_unreachable_reports_unknown_not_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_path = dir.path().join("hooks.json");
        wire_real_hooks(&hooks_path);

        let cfg = config(
            dir.path(),
            hooks_path,
            "/definitely/not/a/real/codex/binary/xyz".to_string(),
        );
        let report = verify(&cfg);
        assert_eq!(report.hooks.len(), 3);
        assert!(report.hooks.iter().all(|h| !h.trust.is_trusted()));
        assert!(report
            .hooks
            .iter()
            .all(|h| matches!(h.trust, HookTrust::Unknown { .. })));
    }

    #[test]
    fn negative_rpc_timeout_reports_unknown_not_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_path = dir.path().join("hooks.json");
        wire_real_hooks(&hooks_path);

        // Responds to initialize, then simply never answers hooks/list
        // within the test's short deadline.
        let script = write_fixture(
            dir.path(),
            "read -r _init\nprintf '{\"id\":1,\"result\":{\"ok\":true}}\\n'\nread -r _hooks\nsleep 30\n",
        );

        // Deliberately short, unlike every other test's
        // `GENEROUS_TEST_DEADLINE` — this test exists specifically to
        // exercise the timeout path quickly, not to be a flakiness risk
        // under load like the correctness tests would be with a short
        // deadline.
        let mut cfg = config(dir.path(), hooks_path, script.display().to_string());
        cfg.deadline = Duration::from_millis(300);
        let start = std::time::Instant::now();
        let report = verify(&cfg);
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "must not hang waiting for the sleeping fixture"
        );
        assert_eq!(report.hooks.len(), 3);
        assert!(report
            .hooks
            .iter()
            .all(|h| matches!(h.trust, HookTrust::Unknown { .. })));
    }

    #[test]
    fn negative_malformed_response_reports_unknown_not_trusted_and_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_path = dir.path().join("hooks.json");
        wire_real_hooks(&hooks_path);

        let script = write_fixture(
            dir.path(),
            "read -r _init\nprintf '{\"id\":1,\"result\":{\"ok\":true}}\\n'\nread -r _hooks\nprintf 'not-json-garbage\\n'\n",
        );

        let cfg = config(dir.path(), hooks_path, script.display().to_string());
        let report = verify(&cfg); // must not panic
        assert_eq!(report.hooks.len(), 3);
        assert!(report
            .hooks
            .iter()
            .all(|h| matches!(h.trust, HookTrust::Unknown { .. })));
    }

    #[test]
    fn negative_orphaned_local_hook_absent_from_rpc_response_reports_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_path = dir.path().join("hooks.json");
        wire_real_hooks(&hooks_path);
        // Only PostToolUse is reported by Codex; UserPromptSubmit and
        // Stop are wired locally but silently absent from the RPC
        // response — the orphaned-entry case.
        let [_prompt, tool, _stop] = expected_commands();
        let hooks_path_str = hooks_path.display().to_string();

        let script = write_fixture(
            dir.path(),
            &format!(
                "read -r _init\nprintf '{{\"id\":1,\"result\":{{\"ok\":true}}}}\\n'\nread -r \
                 _hooks\nprintf '{{\"id\":2,\"result\":{{\"data\":[{{\"cwd\":\"/x\",\"warnings\":[],\
                 \"errors\":[],\"hooks\":[{{\"eventName\":\"postToolUse\",\"command\":\"{tool}\",\
                 \"sourcePath\":\"{hooks_path_str}\",\"currentHash\":\"sha256:bbb\",\
                 \"trustStatus\":\"trusted\",\"enabled\":true}}]}}]}}}}\\n'\n"
            ),
        );

        let cfg = config(dir.path(), hooks_path, script.display().to_string());
        let report = verify(&cfg);
        assert_eq!(report.hooks.len(), 3);
        let by_event = |event: &str| {
            report
                .hooks
                .iter()
                .find(|h| h.event == event)
                .unwrap()
                .trust
                .clone()
        };
        assert!(by_event("PostToolUse").is_trusted());
        assert!(matches!(
            by_event("UserPromptSubmit"),
            HookTrust::Unknown { .. }
        ));
        assert!(matches!(by_event("Stop"), HookTrust::Unknown { .. }));
    }

    #[test]
    fn codex_reported_non_trusted_status_is_relayed_verbatim_not_claimed_trusted() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_path = dir.path().join("hooks.json");
        wire_real_hooks(&hooks_path);
        let [prompt, tool, stop] = expected_commands();
        let hooks_path_str = hooks_path.display().to_string();

        let script = write_fixture(
            dir.path(),
            &format!(
                "read -r _init\nprintf '{{\"id\":1,\"result\":{{\"ok\":true}}}}\\n'\nread -r \
                 _hooks\nprintf '{{\"id\":2,\"result\":{{\"data\":[{{\"cwd\":\"/x\",\"warnings\":[],\
                 \"errors\":[],\"hooks\":[{{\"eventName\":\"userPromptSubmit\",\"command\":\"{prompt}\",\
                 \"sourcePath\":\"{hooks_path_str}\",\"currentHash\":\"sha256:aaa\",\"trustStatus\":\"untrusted\",\
                 \"enabled\":true}},{{\"eventName\":\"postToolUse\",\"command\":\"{tool}\",\
                 \"sourcePath\":\"{hooks_path_str}\",\"currentHash\":\"sha256:bbb\",\"trustStatus\":\"modified\",\
                 \"enabled\":true}},{{\"eventName\":\"stop\",\"command\":\"{stop}\",\
                 \"sourcePath\":\"{hooks_path_str}\",\"currentHash\":\"sha256:ccc\",\"trustStatus\":\"trusted\",\
                 \"enabled\":true}}]}}]}}}}\\n'\n"
            ),
        );

        let cfg = config(dir.path(), hooks_path, script.display().to_string());
        let report = verify(&cfg);
        let by_event = |event: &str| {
            report
                .hooks
                .iter()
                .find(|h| h.event == event)
                .unwrap()
                .trust
                .clone()
        };
        assert!(matches!(
            by_event("UserPromptSubmit"),
            HookTrust::CodexReported { trust_status, .. } if trust_status == "untrusted"
        ));
        assert!(matches!(
            by_event("PostToolUse"),
            HookTrust::CodexReported { trust_status, .. } if trust_status == "modified"
        ));
        assert!(by_event("Stop").is_trusted());
    }

    #[test]
    fn negative_disabled_hook_reports_unknown_not_trusted_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_path = dir.path().join("hooks.json");
        wire_real_hooks(&hooks_path);
        let [prompt, tool, stop] = expected_commands();
        let hooks_path_str = hooks_path.display().to_string();

        let script = write_fixture(
            dir.path(),
            &format!(
                "read -r _init\nprintf '{{\"id\":1,\"result\":{{\"ok\":true}}}}\\n'\nread -r \
                 _hooks\nprintf '{{\"id\":2,\"result\":{{\"data\":[{{\"cwd\":\"/x\",\"warnings\":[],\
                 \"errors\":[],\"hooks\":[{{\"eventName\":\"userPromptSubmit\",\"command\":\"{prompt}\",\
                 \"sourcePath\":\"{hooks_path_str}\",\"currentHash\":\"sha256:aaa\",\"trustStatus\":\"trusted\",\
                 \"enabled\":false}},{{\"eventName\":\"postToolUse\",\"command\":\"{tool}\",\
                 \"sourcePath\":\"{hooks_path_str}\",\"currentHash\":\"sha256:bbb\",\"trustStatus\":\"trusted\",\
                 \"enabled\":true}},{{\"eventName\":\"stop\",\"command\":\"{stop}\",\
                 \"sourcePath\":\"{hooks_path_str}\",\"currentHash\":\"sha256:ccc\",\"trustStatus\":\"trusted\",\
                 \"enabled\":true}}]}}]}}}}\\n'\n"
            ),
        );

        let cfg = config(dir.path(), hooks_path, script.display().to_string());
        let report = verify(&cfg);
        let by_event = |event: &str| {
            report
                .hooks
                .iter()
                .find(|h| h.event == event)
                .unwrap()
                .trust
                .clone()
        };
        assert!(matches!(
            by_event("UserPromptSubmit"),
            HookTrust::Unknown { .. }
        ));
        assert!(by_event("PostToolUse").is_trusted());
        assert!(by_event("Stop").is_trusted());
    }

    #[test]
    fn no_wired_hooks_produces_an_empty_report_without_starting_app_server() {
        let dir = tempfile::tempdir().unwrap();
        let hooks_path = dir.path().join("hooks.json"); // deliberately never wired
        let cfg = config(
            dir.path(),
            hooks_path,
            "/definitely/not/a/real/codex/binary/xyz".to_string(),
        );
        let report = verify(&cfg);
        assert!(report.hooks.is_empty());
    }
}
