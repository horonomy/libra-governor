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

pub fn run() {
    let binary = match std::env::current_exe() {
        Ok(path) => path,
        Err(e) => {
            eprintln!("libra-governor install: could not resolve this binary's own path: {e}");
            std::process::exit(1);
        }
    };

    let settings_path = match claude_settings::settings_path() {
        Ok(path) => path,
        Err(e) => {
            eprintln!("libra-governor install: {e}");
            std::process::exit(1);
        }
    };

    match claude_settings::apply(&settings_path, &binary) {
        Ok(applied) => {
            println!(
                "libra-governor install: wired into {}",
                settings_path.display()
            );
            println!("  hooks added:      {}", applied.hooks_added);
            println!("  statusline added: {}", applied.statusline_added);
            if applied.statusline_conflict {
                println!(
                    "  statusline:       left untouched — a non-Governor statusLine is already configured"
                );
            }
            if let Some(backup) = &applied.backup_path {
                println!("  backup written:   {}", backup.display());
            }
            write_install_marker(&binary);
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
    let marker = serde_json::json!({
        "installed_by": "libra-governor",
        "version": env!("CARGO_PKG_VERSION"),
        "binary_path": binary.display().to_string(),
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
