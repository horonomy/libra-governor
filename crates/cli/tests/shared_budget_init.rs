//! HORO-2248: the first-run path of the local monthly shared budget
//! (ADR-0026) through the REAL `libra-governor` binary in an isolated state
//! directory: `shared-budget init` -> `status` -> `doctor`, and the refusals
//! and hints around it.
//!
//! Evidence class: TestOwnedLive (real CLI and daemon processes, no real
//! Claude Code host).

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

mod support;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_libra-governor")
}

struct Env {
    state: support::DaemonState,
    home: tempfile::TempDir,
    daemon: Option<Child>,
}

impl Env {
    fn new() -> Self {
        Self {
            state: support::DaemonState::new(),
            home: tempfile::tempdir().unwrap(),
            daemon: None,
        }
    }

    fn file(&self) -> PathBuf {
        self.state.path().join("shared-budget.json")
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(bin())
            .args(args)
            .env("LIBRA_GOVERNOR_STATE_DIR", self.state.path())
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

    fn start_daemon(&mut self) {
        self.daemon = Some(
            Command::new(bin())
                .args(["daemon", "run"])
                .env("LIBRA_GOVERNOR_STATE_DIR", self.state.path())
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
    let dir_mode = std::fs::metadata(env.state.path())
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
    assert!(!env.state.path().join("shared-budget.json").exists());
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
        assert!(out.stderr.is_empty(), "{form}: help is not an error");
    }
    // Asking for help must not create the state directory.
    assert!(!env.state.path().join("shared-budget.json").exists());
}

#[test]
fn a_missing_or_unknown_subcommand_is_still_a_usage_error() {
    let env = Env::new();
    assert_eq!(env.run(&["shared-budget"]).status.code(), Some(2));
    assert_eq!(env.run(&["shared-budget", "bogus"]).status.code(), Some(2));
}
