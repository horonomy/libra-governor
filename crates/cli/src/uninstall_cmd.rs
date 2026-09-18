//! `libra-governor uninstall` (HORO-1150) — removes only what this
//! integration's own installer put in place:
//!
//! 1. Governor-owned keys from `~/.claude/settings.json`
//!    ([`claude_settings::remove`] — see that module for the safety
//!    guarantees), attempted unconditionally (a user may have wired
//!    settings by hand from the README rather than via `install`).
//! 2. The daemon binary is never deleted directly by this command. When
//!    `install`'s own marker file
//!    ([`crate::install_cmd::INSTALL_MARKER_FILE_NAME`], read *before*
//!    the state directory that holds it is removed — see step 3) names
//!    this exact binary path, `uninstall` prints how to remove it
//!    (`cargo uninstall libra-governor-cli`, since `scripts/install.sh`
//!    installs via `cargo install`) instead of calling
//!    `fs::remove_file` on it directly — a raw filesystem delete would
//!    leave `~/.cargo/.crates.toml`/`.crates2.json` still recording the
//!    crate as installed with its binary missing, corrupting `cargo`'s
//!    own bookkeeping in a way this tool has no business doing on
//!    another tool's behalf.
//! 3. The daemon's state directory (`ledger.sqlite3`, `daemon.sock`,
//!    `daemon.log`, `config.json`, `gateway.token`, the install marker)
//!    — **only** with explicit confirmation (`--yes`, or an interactive
//!    "yes" at the prompt), because it holds the only local record of
//!    estimate-vs-actual calibration history and deleting it is
//!    unrecoverable. Mirrors this repo's own global policy: never delete
//!    without explicit confirmation.

use crate::{claude_settings, install_cmd};

pub fn run(assume_yes: bool) {
    let settings_ok = uninstall_claude_settings();

    let state_dir = libra_governor_daemon::paths::state_dir().ok();
    let marker_binary_path = state_dir
        .as_deref()
        .and_then(read_install_marker_binary_path);

    report_binary(marker_binary_path.as_deref());
    uninstall_state_dir(state_dir.as_deref(), assume_yes);

    if !settings_ok {
        std::process::exit(1);
    }
}

fn uninstall_claude_settings() -> bool {
    let settings_path = match claude_settings::settings_path() {
        Ok(path) => path,
        Err(e) => {
            eprintln!("libra-governor uninstall: {e}");
            return false;
        }
    };
    match claude_settings::remove(&settings_path) {
        Ok(removed) if removed.file_absent => {
            println!(
                "libra-governor uninstall: {} does not exist — nothing to remove",
                settings_path.display()
            );
            true
        }
        Ok(removed) => {
            println!(
                "libra-governor uninstall: removed from {}",
                settings_path.display()
            );
            println!("  hook commands removed: {}", removed.hook_commands_removed);
            println!("  statusline removed:    {}", removed.statusline_removed);
            println!(
                "  apiKeyHelper removed:  {}",
                removed.api_key_helper_removed
            );
            println!("  base URL removed:      {}", removed.base_url_removed);
            if let Some(backup) = &removed.backup_path {
                println!("  backup written:        {}", backup.display());
            }
            true
        }
        Err(e) => {
            eprintln!(
                "libra-governor uninstall: could not update {}: {e} — nothing was changed \
                 there; continuing with the remaining uninstall steps",
                settings_path.display()
            );
            false
        }
    }
}

/// Reads `install`'s marker (if present) and returns the `binary_path`
/// it recorded — never anything else from the file, and never treated
/// as authoritative on its own (see [`uninstall_binary`], which also
/// requires it to equal `current_exe()`).
fn read_install_marker_binary_path(state_dir: &std::path::Path) -> Option<String> {
    let path = state_dir.join(install_cmd::INSTALL_MARKER_FILE_NAME);
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    if value.get("installed_by").and_then(|v| v.as_str()) != Some("libra-governor") {
        return None;
    }
    value
        .get("binary_path")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Reports how to remove the daemon binary, when the install marker's
/// recorded path equals `current_exe()` — the binary running this very
/// uninstall command must be the one the marker names. Never deletes the
/// binary itself: it was installed via `cargo install` (see
/// `scripts/install.sh`), and only `cargo uninstall` keeps cargo's own
/// package bookkeeping (`~/.cargo/.crates.toml`, `.crates2.json`)
/// consistent with the binary's presence on disk.
fn report_binary(marker_binary_path: Option<&str>) {
    let Some(marker_path) = marker_binary_path else {
        return;
    };
    let Ok(current_exe) = std::env::current_exe() else {
        return;
    };
    if current_exe.display().to_string() != marker_path {
        println!(
            "libra-governor uninstall: install marker names {marker_path}, which is not the \
             binary running this command ({}) — leaving both in place",
            current_exe.display()
        );
        return;
    }

    println!(
        "libra-governor uninstall: the daemon binary at {} was installed via `cargo install`.",
        current_exe.display()
    );
    println!("  To remove it, run: cargo uninstall libra-governor-cli");
}

fn uninstall_state_dir(state_dir: Option<&std::path::Path>, assume_yes: bool) {
    let Some(state_dir) = state_dir else {
        return;
    };
    if !state_dir.exists() {
        println!(
            "libra-governor uninstall: state dir {} does not exist — nothing to remove",
            state_dir.display()
        );
        return;
    }

    let confirmed = assume_yes
        || confirm(
            "This will permanently delete the state directory, including ledger.sqlite3 \
             (the only local record of estimate-vs-actual calibration history). This cannot \
             be undone. Continue",
            state_dir,
        );
    if !confirmed {
        println!(
            "libra-governor uninstall: leaving {} in place (not confirmed). Re-run with \
             --yes, or delete it yourself, to remove it.",
            state_dir.display()
        );
        return;
    }

    match std::fs::remove_dir_all(state_dir) {
        Ok(()) => println!("libra-governor uninstall: removed {}", state_dir.display()),
        Err(e) => eprintln!(
            "libra-governor uninstall: could not remove {}: {e}",
            state_dir.display()
        ),
    }
}

/// Interactive confirmation prompt. Defaults to "no" on an empty answer
/// or when stdin is not a terminal (a non-interactive run must never
/// silently delete real data) — see module docs.
fn confirm(prompt: &str, subject: &std::path::Path) -> bool {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        println!(
            "libra-governor uninstall: stdin is not a terminal and --yes was not passed; \
             not deleting {}",
            subject.display()
        );
        return false;
    }

    print!(
        "{prompt} [{}]? Type \"yes\" to continue [no]: ",
        subject.display()
    );
    let _ = std::io::Write::flush(&mut std::io::stdout());

    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    answer.trim().eq_ignore_ascii_case("yes")
}
