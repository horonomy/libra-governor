//! `libra-governor install` (HORO-1150) — wires this binary's
//! `UserPromptSubmit`/`PostToolUse`/`Stop` hooks and `statusLine` into
//! `~/.claude/settings.json`, without touching any key that is not
//! Governor-owned. See `crates/cli/src/claude_settings.rs` for the
//! actual read-modify-write logic; this is a thin wrapper that resolves
//! this binary's own absolute path and reports what happened.
//!
//! Deliberately does **not** wire the optional enforcement gateway's
//! `env.ANTHROPIC_BASE_URL`/`apiKeyHelper` — that is a separate,
//! deliberate opt-in step documented in
//! `integrations/claude-code/README.md`'s "Enforcement gateway" section,
//! not part of the base preview install.
//!
//! `run_codex` (HORO-1157) is the equivalent entry point for `libra-governor
//! install --agent codex`: wires `~/.codex/hooks.json` via
//! `crate::codex_hooks_file` instead of `~/.claude/settings.json`. Codex has
//! no statusline equivalent (verified absent from its config schema — see
//! `docs/adr/0004-agent-adapter-contract.md`), so there is nothing there to
//! wire, and no gateway opt-in either (Codex's `wire_api` only supports
//! `"responses"`, incompatible with the gateway's Anthropic-messages-only
//! surface — a documented non-goal, not an oversight).

use crate::{claude_settings, codex_hooks_file};

/// Marker file name recording that this binary's own `install` command
/// (rather than a manual `cargo install`/settings edit) put a given
/// binary and its state dir in place — read by `uninstall` so it only
/// ever removes a binary/state dir it can prove it created, never
/// something a user separately built or configured by hand. See
/// `uninstall_cmd`'s module docs.
pub const INSTALL_MARKER_FILE_NAME: &str = "install.json";

/// HORO-1599 2026-10-10 incident: `install` wires whatever
/// `std::env::current_exe()` resolves to into the live hook config
/// verbatim -- permanently, until the next `install`. If that path is a
/// cargo build output directory (`target/debug/...` or
/// `target/release/...`, shared or per-worktree), any later `cargo build`
/// into the same target dir silently overwrites the binary every live
/// hook invocation executes, with whatever happens to be on that branch
/// at that moment -- exactly what happened here: an in-progress,
/// unreviewed build replaced the installed hook binary for roughly two
/// hours before detection. This is a loud warning, not a hard refusal:
/// some development workflows genuinely want to install straight from a
/// build directory, but they need to know the risk they're accepting.
fn build_output_path_warning(binary: &std::path::Path) -> Option<String> {
    let path_str = binary.to_string_lossy();
    // Deliberately `target/debug/`/`target/release/` without a leading
    // slash: a shared cross-worktree target dir is commonly named
    // something like `shared-target`, not literally `target` -- the
    // incident this guards against happened at exactly such a path
    // (`~/.cargo/shared-target/debug/libra-governor`).
    if path_str.contains("target/debug/") || path_str.contains("target/release/") {
        Some(format!(
            "libra-governor install: WARNING -- installing from a cargo build output path \
             ({path_str}). Any later `cargo build` into this same target directory will \
             silently replace the binary every live hook invocation executes, including \
             mid-development, unreviewed code. Prefer installing a copy at a stable path \
             outside any cargo target directory if this installation is meant to serve real \
             hook traffic rather than a disposable development session."
        ))
    } else {
        None
    }
}

fn warn_if_build_output_path(binary: &std::path::Path) {
    if let Some(warning) = build_output_path_warning(binary) {
        eprintln!("{warning}");
    }
}

pub fn run() {
    run_claude(true);
}

/// Installs just the Claude Code hooks, without touching `statusLine`.
pub fn run_hooks_only() {
    run_claude(false);
}

fn run_claude(install_statusline: bool) {
    let binary = match std::env::current_exe() {
        Ok(path) => path,
        Err(e) => {
            eprintln!("libra-governor install: could not resolve this binary's own path: {e}");
            std::process::exit(1);
        }
    };
    warn_if_build_output_path(&binary);

    let settings_path = match claude_settings::settings_path() {
        Ok(path) => path,
        Err(e) => {
            eprintln!("libra-governor install: {e}");
            std::process::exit(1);
        }
    };

    let guarded = (|| {
        let root = libra_governor_daemon::paths::state_dir().map_err(|_| {
            libra_governor_daemon::host_runtime::RegistryFailure::new(
                "lifecycle",
                "state_unavailable",
            )
        })?;
        let contract = libra_governor_daemon::host_runtime::contract::HostContract::load()?;
        libra_governor_daemon::host_runtime::config_lifecycle::ConfigLifecycle::new(root, contract)
            .run_legacy_claude(&settings_path, true, |root| {
                let applied = if install_statusline {
                    claude_settings::apply(&settings_path, &binary)
                } else {
                    claude_settings::apply_hooks_only(&settings_path, &binary)
                };
                if applied.is_ok() {
                    write_install_marker_at(&binary, root);
                }
                applied
            })
    })();
    let applied = match guarded {
        Ok(Some(result)) => result,
        Ok(None) => unreachable!("legacy activation always executes or refuses"),
        Err(error) => {
            eprintln!("libra-governor install: legacy configuration refused: {error}");
            std::process::exit(1);
        }
    };
    match applied {
        Ok(applied) => {
            println!(
                "libra-governor install: wired into {}",
                settings_path.display()
            );
            println!("  hooks added:      {}", applied.hooks_added);
            if install_statusline {
                println!("  statusline added: {}", applied.statusline_added);
            } else {
                println!("  statusLine:       left unchanged");
            }
            if applied.statusline_conflict {
                println!(
                    "  statusline:       left untouched — a non-Governor statusLine is already configured"
                );
            }
            if let Some(backup) = &applied.backup_path {
                println!("  backup written:   {}", backup.display());
            }
            println!();
            if applied.statusline_conflict {
                println!(
                    "Note: your existing statusLine was not replaced. Run `libra-governor doctor` for details."
                );
            }
            println!("Next: submit a prompt in Claude Code, then run `libra-governor doctor`.");
        }
        Err(e) => {
            eprintln!(
                "libra-governor install: could not update {}: {e}",
                settings_path.display()
            );
            std::process::exit(1);
        }
    }
}

/// `libra-governor install --agent codex` — wires `~/.codex/hooks.json`
/// (or `$CODEX_HOME/hooks.json`) instead of Claude Code's settings.json.
/// See module docs for what this deliberately does not also wire.
pub fn run_codex() {
    let binary = match std::env::current_exe() {
        Ok(path) => path,
        Err(e) => {
            eprintln!("libra-governor install: could not resolve this binary's own path: {e}");
            std::process::exit(1);
        }
    };
    warn_if_build_output_path(&binary);

    let hooks_path = match codex_hooks_file::hooks_path() {
        Ok(path) => path,
        Err(e) => {
            eprintln!("libra-governor install: {e}");
            std::process::exit(1);
        }
    };

    match codex_hooks_file::apply(&hooks_path, &binary) {
        Ok(applied) => {
            println!(
                "libra-governor install: wired into {}",
                hooks_path.display()
            );
            println!("  hooks added:      {}", applied.hooks_added);
            if let Some(backup) = &applied.backup_path {
                println!("  backup written:   {}", backup.display());
            }
            write_install_marker(&binary);
            println!();
            println!(
                "IMPORTANT: writing hooks.json is not enough for Codex to run these hooks. \
                 Run `/hooks` inside Codex and trust the three `libra-governor` hooks — trust \
                 is recorded by content hash; if you move the binary, you must re-trust."
            );
            println!("Next: submit a prompt in Codex, then run `libra-governor doctor`.");
        }
        Err(e) => {
            eprintln!(
                "libra-governor install: could not update {}: {e}",
                hooks_path.display()
            );
            std::process::exit(1);
        }
    }
}

/// Records that this exact binary path was installed by this command, so
/// `uninstall` can later prove a binary is ours before removing it —
/// never a `PATH` scan, never a name guess. Best-effort: a failure here
/// is logged to stderr but does not fail the install (the settings.json
/// wiring above is the part that actually matters for Claude Code to
/// work).
fn write_install_marker(binary: &std::path::Path) {
    let state_dir = match libra_governor_daemon::paths::ensure_state_dir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!(
                "libra-governor install: could not prepare state dir for install marker: {e}"
            );
            return;
        }
    };
    write_install_marker_at(binary, &state_dir);
}

fn write_install_marker_at(binary: &std::path::Path, state_dir: &std::path::Path) {
    // Best-effort (HORO-1380 S4b): `None` if hashing fails, written as
    // `null`. Not read back by `doctor`'s stale-runtime check, which
    // always hashes the binary at `binary_path` fresh at diagnostic time
    // instead — a cached hash here would itself go stale if the binary at
    // this path were ever replaced in place without a re-`install`.
    let binary_sha256 = libra_governor_daemon::pidfile::hash_file(binary);
    let marker = serde_json::json!({
        "installed_by": "libra-governor",
        "version": env!("CARGO_PKG_VERSION"),
        "binary_path": binary.display().to_string(),
        "binary_sha256": binary_sha256,
    });
    let path = state_dir.join(INSTALL_MARKER_FILE_NAME);
    if let Err(e) = std::fs::write(
        &path,
        serde_json::to_string_pretty(&marker).unwrap_or_default(),
    ) {
        eprintln!(
            "libra-governor install: could not write {}: {e}",
            path.display()
        );
    }
}

#[cfg(test)]
mod build_output_path_warning_tests {
    use super::build_output_path_warning;
    use std::path::Path;

    #[test]
    fn warns_on_debug_target_path() {
        assert!(build_output_path_warning(Path::new(
            "/Users/bryant/.cargo/shared-target/debug/libra-governor"
        ))
        .is_some());
    }

    #[test]
    fn warns_on_release_target_path() {
        assert!(build_output_path_warning(Path::new(
            "/home/ci/repo/target/release/libra-governor"
        ))
        .is_some());
    }

    #[test]
    fn does_not_warn_on_a_stable_non_build_path() {
        assert!(
            build_output_path_warning(Path::new("/Users/bryant/.local/bin/libra-governor"))
                .is_none()
        );
    }
}
