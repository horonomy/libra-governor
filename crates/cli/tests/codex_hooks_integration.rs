//! End-to-end tests of the real `libra-governor` binary's `install
//! --agent codex` / `uninstall --agent codex [--yes]` subcommands
//! (HORO-1157). Mirrors `doctor_uninstall_integration.rs`'s pattern:
//! spawn the real built binary as a subprocess, drive it with real env
//! vars and a real temp `$CODEX_HOME`, assert on its real
//! stdout/stderr/exit code and on the resulting `hooks.json` bytes.

use std::process::{Command, Stdio};
use std::sync::Once;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_libra-governor")
}

fn warm_up_binary() {
    static WARM_UP: Once = Once::new();
    WARM_UP.call_once(|| {
        let _ = Command::new(bin())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
}

struct Sandbox {
    _state_parent: tempfile::TempDir,
    _codex_parent: tempfile::TempDir,
    state_dir: std::path::PathBuf,
    codex_dir: std::path::PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        warm_up_binary();
        let state_parent = tempfile::tempdir().unwrap();
        let codex_parent = tempfile::tempdir().unwrap();
        let state_dir = state_parent.path().join("state");
        let codex_dir = codex_parent.path().join("codex");
        std::fs::create_dir_all(&codex_dir).unwrap();

        Sandbox {
            _state_parent: state_parent,
            _codex_parent: codex_parent,
            state_dir,
            codex_dir,
        }
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(bin())
            .args(args)
            .env("LIBRA_GOVERNOR_STATE_DIR", &self.state_dir)
            .env("LIBRA_GOVERNOR_CODEX_HOME", &self.codex_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap()
    }

    fn hooks_path(&self) -> std::path::PathBuf {
        self.codex_dir.join("hooks.json")
    }
}

#[test]
fn install_agent_codex_writes_three_hook_groups_with_a_trust_reminder() {
    let sandbox = Sandbox::new();

    let output = sandbox.run(&["install", "--agent", "codex"]);
    assert!(
        output.status.success(),
        "install --agent codex failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("/hooks") && stdout.to_lowercase().contains("trust"),
        "install --agent codex must remind the user to run /hooks and trust the hooks: {stdout}"
    );

    let hooks_json = std::fs::read_to_string(sandbox.hooks_path()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&hooks_json).unwrap();
    assert!(
        value.get("UserPromptSubmit").is_none(),
        "hook groups must live under the top-level \"hooks\" key, never at the top level \
         (Codex's real schema only accepts \"description\"/\"hooks\" as top-level keys)"
    );
    for event in ["UserPromptSubmit", "PostToolUse", "Stop"] {
        let command = value["hooks"][event][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(
            command.ends_with(match event {
                "UserPromptSubmit" => "codex-hook user-prompt-submit",
                "PostToolUse" => "codex-hook post-tool-use",
                "Stop" => "codex-hook stop",
                _ => unreachable!(),
            }),
            "unexpected command for {event}: {command}"
        );
        assert_eq!(value["hooks"][event][0]["hooks"][0]["timeout"], 15);
    }
    assert_eq!(value["hooks"]["PostToolUse"][0]["hooks"][0]["async"], true);
    assert!(value["hooks"]["UserPromptSubmit"][0]["hooks"][0]
        .get("async")
        .is_none());
}

#[test]
fn install_agent_codex_is_idempotent() {
    let sandbox = Sandbox::new();
    sandbox.run(&["install", "--agent", "codex"]);
    let first = std::fs::read_to_string(sandbox.hooks_path()).unwrap();

    let second_output = sandbox.run(&["install", "--agent", "codex"]);
    assert!(second_output.status.success());
    let second = std::fs::read_to_string(sandbox.hooks_path()).unwrap();
    assert_eq!(
        first, second,
        "re-running install --agent codex must not change an already-wired hooks.json"
    );
}

#[test]
fn install_agent_codex_preserves_a_foreign_hook_group() {
    let sandbox = Sandbox::new();
    std::fs::create_dir_all(&sandbox.codex_dir).unwrap();
    let seed = serde_json::json!({
        "description": "my own hooks file",
        "hooks": {
            "PreCompact": [
                { "hooks": [{ "type": "command", "command": "/usr/local/bin/my-own-tool precompact" }] }
            ]
        }
    });
    std::fs::write(
        sandbox.hooks_path(),
        serde_json::to_string_pretty(&seed).unwrap(),
    )
    .unwrap();

    let output = sandbox.run(&["install", "--agent", "codex"]);
    assert!(output.status.success());

    let hooks_json = std::fs::read_to_string(sandbox.hooks_path()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&hooks_json).unwrap();
    assert_eq!(value["description"], "my own hooks file");
    assert_eq!(
        value["hooks"]["PreCompact"][0]["hooks"][0]["command"],
        "/usr/local/bin/my-own-tool precompact",
        "a foreign, unrelated hook group (and the foreign description field) must survive \
         install --agent codex byte-for-byte"
    );
}

#[test]
fn install_then_uninstall_agent_codex_returns_to_an_empty_file() {
    let sandbox = Sandbox::new();
    sandbox.run(&["install", "--agent", "codex"]);

    let uninstall_output = sandbox.run(&["uninstall", "--agent", "codex", "--yes"]);
    assert!(
        uninstall_output.status.success(),
        "uninstall --agent codex failed: {}",
        String::from_utf8_lossy(&uninstall_output.stderr)
    );

    let hooks_json = std::fs::read_to_string(sandbox.hooks_path()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&hooks_json).unwrap();
    assert!(
        value.as_object().unwrap().is_empty(),
        "an emptied hooks.json must contain no leftover Governor event keys: {value}"
    );
}

#[test]
fn uninstall_agent_codex_without_yes_never_deletes_the_state_dir() {
    let sandbox = Sandbox::new();
    sandbox.run(&["install", "--agent", "codex"]);
    std::fs::create_dir_all(&sandbox.state_dir).unwrap();
    std::fs::write(sandbox.state_dir.join("ledger.sqlite3"), b"pretend-ledger").unwrap();

    let output = sandbox.run(&["uninstall", "--agent", "codex"]);
    assert!(output.status.success());
    assert!(
        sandbox.state_dir.join("ledger.sqlite3").exists(),
        "a non-interactive uninstall without --yes must never delete real ledger data"
    );
}

#[test]
fn uninstall_agent_codex_on_a_missing_hooks_file_is_a_safe_no_op() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["uninstall", "--agent", "codex", "--yes"]);
    assert!(output.status.success());
    assert!(!sandbox.hooks_path().exists());
}
