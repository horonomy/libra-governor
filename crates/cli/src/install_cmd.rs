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

use crate::claude_settings;

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
