//! HORO-2248: the first-run path of the local monthly shared budget
//! (ADR-0026) through the REAL `libra-governor` binary in an isolated state
//! directory: `shared-budget init` -> `status` -> `doctor`, and the refusals
//! and hints around it.
//!
//! Evidence class: TestOwnedLive (real CLI and daemon processes, no real
//! Claude Code host).

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_libra-governor")
}

#[test]
fn init_configures_warn_only_soft_limits_with_exact_cents() {
    let env = Env::new();
    let out = env.run(&[
        "shared-budget",
        "init",
        "--monthly-usd",
        "200",
        "--time-zone",
        "Asia/Taipei",
        "--turn-hold-usd",
        "1.25",
        "--per-session-soft-usd",
        "12.50",
        "--per-agent-soft-usd",
        "5.25",
    ]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let doc: Value = serde_json::from_slice(&std::fs::read(env.file()).unwrap()).unwrap();
    assert_eq!(doc["monthly_cap_usd_cents"], 20_000);
    assert_eq!(doc["turn_hold_usd_cents"], 125);
    assert_eq!(doc["soft_limits"]["per_session_usd_cents"], 1_250);
    assert_eq!(doc["soft_limits"]["per_agent_usd_cents"], 525);
    assert!(matches!(
        libra_governor_daemon::shared_budget_file::parse(&std::fs::read(env.file()).unwrap()),
        libra_governor_daemon::shared_budget_file::SharedBudgetFileState::Valid(_)
    ));
    for flag in ["--per-session-soft-usd", "--per-agent-soft-usd"] {
        for bad in ["0", "-1", "NaN", "0.001"] {
            let invalid = Env::new();
            let out = invalid.run(&[
                "shared-budget",
                "init",
                "--monthly-usd",
                "200",
                "--time-zone",
                "UTC",
                flag,
                bad,
            ]);
            assert_eq!(out.status.code(), Some(2), "{flag} {bad}");
            assert!(
                !invalid.file().exists(),
                "bad soft limit must not create config"
            );
        }
    }
}

struct Env {
    /// Private parent of the state dir. The host adapter registry refuses a
    /// state dir whose parent is group/other-writable or not owned by the
    /// user (`unsafe_state_parent`), which a bare `$TMPDIR/.tmpXXXX` is on
    /// Linux (the shared sticky tmp). The state dir is `<parent>/state`,
    /// created by the commands under test, as the other install tests do.
    _parent: tempfile::TempDir,
    state_dir: PathBuf,
    home: tempfile::TempDir,
    daemon: Option<Child>,
}

/// Mode applied to the private parent; the registry requires `mode & 0o022 == 0`.
const PARENT_MODE: u32 = 0o700;

impl Env {
    fn new() -> Self {
        let parent = tempfile::tempdir().unwrap();
        std::fs::set_permissions(parent.path(), std::fs::Permissions::from_mode(PARENT_MODE))
            .unwrap();
        let mode = std::fs::metadata(parent.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o022,
            0,
            "state parent must not be group/other-writable"
        );
        let state_dir = parent.path().join("state");
        Self {
            _parent: parent,
            state_dir,
            home: tempfile::tempdir().unwrap(),
            daemon: None,
        }
    }

    fn state_path(&self) -> &Path {
        &self.state_dir
    }

    fn file(&self) -> PathBuf {
        self.state_path().join("shared-budget.json")
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(bin())
            .args(args)
            .env("LIBRA_GOVERNOR_STATE_DIR", self.state_path())
            .env(
                "LIBRA_GOVERNOR_CLAUDE_DIR",
                self.home.path().join("dot-claude"),
            )
            .env("HOME", self.home.path())
            .env("LIBRA_GOVERNOR_NO_AUTOSPAWN", "1")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// A prompt through the real `UserPromptSubmit` hook; returns its stdout.
    fn prompt(&self, session: &str, repo: &Path) -> String {
        let mut child = Command::new(bin())
            .args(["hook", "user-prompt-submit"])
            .env("LIBRA_GOVERNOR_STATE_DIR", self.state_path())
            .env("HOME", self.home.path())
            .env("LIBRA_GOVERNOR_NO_AUTOSPAWN", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let payload = serde_json::json!({"session_id": session, "cwd": repo, "prompt": "go"});
        let _ = child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes());
        text(&child.wait_with_output().unwrap().stdout)
    }

    fn start_daemon(&mut self) {
        self.daemon = Some(
            Command::new(bin())
                .args(["daemon", "run"])
                .env("LIBRA_GOVERNOR_STATE_DIR", self.state_path())
                .env("HOME", self.home.path())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn daemon run"),
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let status = self.run(&["shared-budget", "status", "--json"]);
            let doc: Value = serde_json::from_slice(&status.stdout).unwrap_or(Value::Null);
            if doc["state"] == "reported" {
                return;
            }
            assert!(Instant::now() < deadline, "daemon never answered: {doc}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if let Some(mut child) = self.daemon.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn init_args<'a>(cap: &'a str, zone: &'a str) -> Vec<&'a str> {
    vec![
        "shared-budget",
        "init",
        "--monthly-usd",
        cap,
        "--time-zone",
        zone,
    ]
}

#[test]
fn init_then_status_then_doctor_is_a_working_budget() {
    let mut env = Env::new();
    let out = env.run(&init_args("200", "Asia/Taipei"));
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(stdout.contains("shared-budget.json"), "{stdout}");
    assert!(stdout.contains("$200.00"), "{stdout}");
    assert!(stdout.contains("Asia/Taipei"), "{stdout}");
    assert!(
        stdout.contains("$10.00"),
        "the default turn hold is stated: {stdout}"
    );
    assert!(
        stdout.contains("admission-bounded (cooperative)"),
        "{stdout}"
    );
    assert!(stdout.contains("shared-budget status"), "{stdout}");

    let mode = std::fs::metadata(env.file()).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let dir_mode = std::fs::metadata(env.state_path())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(dir_mode, 0o700);

    env.start_daemon();
    let status: Value =
        serde_json::from_slice(&env.run(&["shared-budget", "status", "--json"]).stdout).unwrap();
    assert_eq!(status["state"], "reported");
    assert_eq!(status["config"]["state"], "valid");
    assert_eq!(status["config"]["values"]["monthly_cap_usd_cents"], 20_000);
    assert_eq!(status["config"]["values"]["time_zone"], "Asia/Taipei");

    let doctor = env.run(&["doctor", "--json"]);
    let doc: Value = serde_json::from_slice(&doctor.stdout).unwrap();
    let finding = doc["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == "shared_budget_config")
        .expect("doctor reports the budget config");
    assert_eq!(finding["severity"], "ok", "{finding}");
    assert!(finding["message"].as_str().unwrap().contains("valid"));
}

#[test]
fn init_requires_a_time_zone_and_says_why_there_is_no_default() {
    let env = Env::new();
    let out = env.run(&["shared-budget", "init", "--monthly-usd", "200"]);
    assert_eq!(out.status.code(), Some(2));
    let err = text(&out.stderr);
    assert!(err.contains("--time-zone"), "{err}");
    assert!(err.contains("no UTC default"), "{err}");
    assert!(!env.file().exists());
}

#[test]
fn init_rejects_bad_amounts_and_zones_without_creating_anything() {
    let env = Env::new();
    for (cap, zone) in [
        ("0", "UTC"),
        ("-5", "UTC"),
        ("1.234", "UTC"),
        ("NaN", "UTC"),
        ("1e3", "UTC"),
        ("999999999", "UTC"),
        ("200", "Mars/Olympus"),
    ] {
        let out = env.run(&init_args(cap, zone));
        assert_eq!(out.status.code(), Some(2), "{cap} {zone}: {out:?}");
    }
    assert!(!env.state_path().join("shared-budget.json").exists());
    // A cap below the default $10 hold would be an invalid file: refused with advice.
    let out = env.run(&init_args("5", "UTC"));
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("--turn-hold-usd"));
    assert!(!env.file().exists());
}

#[test]
fn init_never_overwrites_without_force() {
    let env = Env::new();
    assert!(env.run(&init_args("200", "UTC")).status.success());
    let before = std::fs::read(env.file()).unwrap();

    let refused = env.run(&init_args("300", "UTC"));
    assert_eq!(refused.status.code(), Some(1));
    assert!(text(&refused.stderr).contains("--force"));
    assert_eq!(std::fs::read(env.file()).unwrap(), before);

    let mut forced = init_args("300", "UTC");
    forced.extend(["--turn-hold-usd", "2.50", "--force"]);
    let out = env.run(&forced);
    assert!(out.status.success(), "{out:?}");
    let doc: Value = serde_json::from_slice(&std::fs::read(env.file()).unwrap()).unwrap();
    assert_eq!(doc["monthly_cap_usd_cents"], 30_000);
    assert_eq!(doc["turn_hold_usd_cents"], 250);
    assert!(text(&out.stdout).contains("Replaced"));
}

#[test]
fn help_forms_print_usage_to_stdout_and_exit_zero() {
    let env = Env::new();
    for form in ["--help", "-h", "help"] {
        let out = env.run(&["shared-budget", form]);
        assert_eq!(out.status.code(), Some(0), "{form}: {out:?}");
        let stdout = text(&out.stdout);
        assert!(stdout.contains("shared-budget init"), "{form}: {stdout}");
        assert!(stdout.contains("shared-budget status"), "{form}: {stdout}");
        assert!(stdout.contains("no backup is kept"), "{form}: {stdout}");
        assert!(out.stderr.is_empty(), "{form}: help is not an error");
    }
    // Asking for help must not create the state directory.
    assert!(!env.state_path().join("shared-budget.json").exists());
}

#[test]
fn a_missing_or_unknown_subcommand_is_still_a_usage_error() {
    let env = Env::new();
    assert_eq!(env.run(&["shared-budget"]).status.code(), Some(2));
    assert_eq!(env.run(&["shared-budget", "bogus"]).status.code(), Some(2));
}

#[test]
fn not_configured_status_and_doctor_say_how_to_enable_it() {
    let env = Env::new();
    let status = text(&env.run(&["shared-budget", "status"]).stdout);
    assert!(
        status.starts_with("Shared budget: not configured"),
        "{status}"
    );
    assert!(
        status.contains("shared-budget init --monthly-usd"),
        "{status}"
    );
    let json: Value =
        serde_json::from_slice(&env.run(&["shared-budget", "status", "--json"]).stdout).unwrap();
    assert_eq!(json["state"], "not_configured");
    assert!(json["how_to_enable"].as_str().unwrap().contains("init"));

    let doctor: Value = serde_json::from_slice(&env.run(&["doctor", "--json"]).stdout).unwrap();
    let finding = doctor["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["id"] == "shared_budget_config")
        .unwrap();
    assert!(finding["message"]
        .as_str()
        .unwrap()
        .contains("shared-budget init --monthly-usd"));
}

#[test]
fn install_mentions_the_budget_only_when_none_is_configured() {
    let env = Env::new();
    let first = env.run(&["install"]);
    assert!(first.status.success(), "{first:?}");
    assert!(
        text(&first.stdout).contains("shared-budget init --monthly-usd"),
        "{}",
        text(&first.stdout)
    );

    assert!(env.run(&init_args("200", "UTC")).status.success());
    let second = env.run(&["install"]);
    assert!(second.status.success(), "{second:?}");
    assert!(
        !text(&second.stdout).contains("shared-budget init"),
        "{}",
        text(&second.stdout)
    );
}

#[test]
fn a_blocked_prompt_names_the_zone_the_reservation_and_the_remedy() {
    let mut env = Env::new();
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();
    let mut args = init_args("0.99", "Asia/Taipei");
    args.extend(["--turn-hold-usd", "0.50"]);
    assert!(env.run(&args).status.success());
    env.start_daemon();

    // The first session's hold leaves $0.49, less than the $0.50 a turn needs.
    let admitted = env.prompt("session-a", repo.path());
    let admitted_doc: Value = serde_json::from_str(admitted.trim()).expect("hook output is JSON");
    assert!(admitted_doc.get("decision").is_none(), "{admitted}");
    let status: Value =
        serde_json::from_slice(&env.run(&["shared-budget", "status", "--json"]).stdout).unwrap();
    assert_eq!(
        status["amounts_usd_cents"]["held"], 50,
        "the admitted turn holds $0.50: {status}"
    );
    assert_eq!(status["amounts_usd_cents"]["remaining"], 49);
    let blocked = env.prompt("session-b", repo.path());
    let doc: Value = serde_json::from_str(blocked.trim()).expect("a block decision");
    assert_eq!(doc["decision"], "block");
    let reason = doc["reason"].as_str().unwrap();
    assert!(
        reason.contains("$0.49 remaining is less than the $0.50 a turn reserves"),
        "{reason}"
    );
    assert!(reason.contains("(Asia/Taipei)"), "{reason}");
    assert!(
        reason.contains("+08:00"),
        "period end is zone-local: {reason}"
    );
    assert!(
        reason.contains("raise monthly_cap_usd_cents in")
            && reason.contains("shared-budget.json (applies to the next prompt)"),
        "{reason}"
    );
    assert!(
        reason.contains("finish or stop other active sessions"),
        "{reason}"
    );
    assert!(!reason.contains("wait for the period"), "{reason}");
}

#[test]
fn the_top_level_usage_lists_init() {
    let env = Env::new();
    let out = env.run(&["--help"]);
    assert!(out.status.success());
    assert!(text(&out.stdout).contains("shared-budget init --monthly-usd"));
}
