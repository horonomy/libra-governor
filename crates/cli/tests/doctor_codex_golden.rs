//! HORO-2285 (C5): `doctor` prints nothing Codex-enrolment-related, and exits
//! exactly as before, when Codex is not enrolled.
//!
//! The goldens in `tests/fixtures/doctor_codex/` were recorded from the binary
//! **before** any C5 code existed (`C5_BLESS=1`, at the tests-only commit whose
//! `src/` is the C3 head), so a later byte difference is a real change of the
//! operator-visible output, not a re-blessed one. A claim of "unchanged when
//! disabled" holds only for these fixtures: a budget file with no Codex marker
//! (daemon not running), and no budget file at all.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_libra-governor")
}

struct Env {
    _root: tempfile::TempDir,
    state: PathBuf,
    claude: PathBuf,
    home: PathBuf,
}

impl Env {
    fn new(budget_file: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let claude = root.path().join("claude");
        let home = root.path().join("home");
        for dir in [&state, &claude, &home] {
            std::fs::create_dir_all(dir).unwrap();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        if budget_file {
            std::fs::write(
                state.join("shared-budget.json"),
                r#"{"schema_version":1,"monthly_cap_usd_cents":5000,"time_zone":"UTC","turn_hold_usd_cents":1000}"#,
            )
            .unwrap();
        }
        Self {
            _root: root,
            state,
            claude,
            home,
        }
    }

    /// Runs `doctor` with a cleared environment so nothing from the machine
    /// (CODEX_HOME, CLAUDE_*, PATH shims) can leak into the golden.
    fn doctor(&self, json: bool) -> String {
        let mut cmd = Command::new(bin());
        cmd.arg("doctor");
        if json {
            cmd.arg("--json");
        }
        let out = cmd
            .env_clear()
            .env("HOME", &self.home)
            .env("LIBRA_GOVERNOR_STATE_DIR", &self.state)
            .env("LIBRA_GOVERNOR_CLAUDE_DIR", &self.claude)
            .env("LIBRA_GOVERNOR_NO_AUTOSPAWN", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap();
        let stdout = String::from_utf8(out.stdout).unwrap();
        let normalized = stdout
            .replace(&self.state.display().to_string(), "<STATE>")
            .replace(&self.claude.display().to_string(), "<CLAUDE>")
            .replace(&self.home.display().to_string(), "<HOME>");
        format!("exit={}\n{normalized}", out.status.code().unwrap_or(-1))
    }
}

fn golden_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/doctor_codex")
        .join(name)
}

fn assert_golden(name: &str, actual: &str) {
    let path = golden_path(name);
    if std::env::var_os("C5_BLESS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing golden {}: {e}", path.display()));
    assert_eq!(
        actual, expected,
        "doctor output for a non-enrolled Codex setup changed ({name})"
    );
}

#[test]
fn budget_present_marker_absent_human_output_is_unchanged() {
    let env = Env::new(true);
    assert_golden("budget_marker_absent.txt", &env.doctor(false));
}

#[test]
fn budget_present_marker_absent_json_output_is_unchanged() {
    let env = Env::new(true);
    assert_golden("budget_marker_absent.json", &env.doctor(true));
}

#[test]
fn no_budget_file_output_is_unchanged() {
    let env = Env::new(false);
    assert_golden("no_budget.txt", &env.doctor(false));
}

#[test]
fn marker_absent_output_mentions_no_codex_enrolment() {
    let env = Env::new(true);
    let human = env.doctor(false);
    assert!(!human.contains("codex_enrolment"), "{human}");
    assert!(!human.contains("shared-budget.codex"), "{human}");
}
