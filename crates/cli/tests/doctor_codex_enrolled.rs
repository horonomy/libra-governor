//! HORO-2285 (C5): `doctor` on a Codex-enrolled install, driven through the
//! real binary against a scripted in-test daemon (the real wire protocol).
//!
//! Assertions are per finding id and severity from `--json`, never the exit
//! code of an unrelated finding: the scripted daemon does not implement
//! `Doctor`, which is itself an Error finding.

use std::io::BufReader;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use libra_governor_protocol::{
    wire, Request, RequestEnvelope, Response, ResponseEnvelope, SharedBudgetCheckState,
    SharedBudgetOutcome, PROTOCOL_VERSION,
};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_libra-governor")
}

struct Env {
    _root: tempfile::TempDir,
    state: PathBuf,
    claude: PathBuf,
    home: PathBuf,
    codex_home: PathBuf,
}

const BUDGET: &str = r#"{"schema_version":1,"monthly_cap_usd_cents":5000,"time_zone":"UTC","turn_hold_usd_cents":1000"#;

impl Env {
    /// `overrides` is the JSON body of `pricing_overrides`, if any.
    fn new(budget: Option<&str>, marker: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("s");
        let claude = root.path().join("c");
        let home = root.path().join("h");
        let codex_home = home.join(".codex");
        for dir in [&state, &claude, &home, &codex_home] {
            std::fs::create_dir_all(dir).unwrap();
        }
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        if let Some(extra) = budget {
            std::fs::write(
                state.join("shared-budget.json"),
                format!("{BUDGET}{extra}}}"),
            )
            .unwrap();
        }
        if marker {
            std::fs::write(state.join("shared-budget.codex"), b"").unwrap();
        }
        Self {
            _root: root,
            state,
            claude,
            home,
            codex_home,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(bin());
        cmd.args(args)
            .env_clear()
            .env("HOME", &self.home)
            .env("CODEX_HOME", &self.codex_home)
            .env("LIBRA_GOVERNOR_STATE_DIR", &self.state)
            .env("LIBRA_GOVERNOR_CLAUDE_DIR", &self.claude)
            .env("LIBRA_GOVERNOR_NO_AUTOSPAWN", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }

    /// Claude's six hooks too, so an exit code isolates the Codex findings
    /// (an unwired Claude hook under a budget file is itself an Error).
    fn install_claude_hooks(&self) {
        let out = self.command(&["install"]).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn install_codex_hooks(&self) {
        let out = self
            .command(&["install", "--agent", "codex"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn doctor_with(&self, extra_env: &[(&str, &str)]) -> Doctor {
        let mut cmd = self.command(&["doctor", "--json"]);
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "doctor --json not JSON: {e}\n{}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        });
        Doctor {
            code: out.status.code(),
            json,
        }
    }

    fn doctor(&self) -> Doctor {
        self.doctor_with(&[])
    }
}

struct Doctor {
    code: Option<i32>,
    json: serde_json::Value,
}

impl Doctor {
    fn finding(&self, id: &str) -> Option<(String, String)> {
        self.json["findings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["id"] == id)
            .map(|f| {
                (
                    f["severity"].as_str().unwrap().to_string(),
                    f["message"].as_str().unwrap().to_string(),
                )
            })
    }

    fn expect(&self, id: &str) -> (String, String) {
        self.finding(id)
            .unwrap_or_else(|| panic!("no {id} finding in {}", self.json))
    }

    fn codex_ids(&self) -> Vec<String> {
        self.json["findings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["id"].as_str().unwrap().to_string())
            .filter(|id| id.starts_with("codex_") && id != "codex_hooks")
            .collect()
    }
}

// ---------------------------------------------------------------------
// Scripted daemon: answers a Codex Check as told, errors on everything else.
// ---------------------------------------------------------------------

struct Fake {
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Fake {
    fn start(state: &Path, check_answer: SharedBudgetOutcome) -> Self {
        let listener = UnixListener::bind(state.join("daemon.sock")).unwrap();
        listener.set_nonblocking(true).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (reqs, halt) = (requests.clone(), stop.clone());
        let handle = std::thread::spawn(move || {
            while !halt.load(Ordering::SeqCst) {
                let stream: UnixStream = match listener.accept() {
                    Ok((s, _)) => s,
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let Ok(envelope) = wire::read_message::<_, RequestEnvelope>(BufReader::new(
                    stream.try_clone().unwrap(),
                )) else {
                    continue;
                };
                reqs.lock()
                    .unwrap()
                    .push(serde_json::to_value(&envelope.request).unwrap());
                let response = match &envelope.request {
                    Request::SharedBudget {
                        shared_budget: libra_governor_protocol::SharedBudgetRequest::Check { .. },
                    } => Response::SharedBudget(Box::new(check_answer.clone())),
                    _ => Response::Error {
                        message: "scripted daemon: not implemented".to_string(),
                    },
                };
                let _ = wire::write_message(
                    &stream,
                    &ResponseEnvelope {
                        protocol_version: PROTOCOL_VERSION,
                        response,
                    },
                );
            }
        });
        Self {
            requests,
            stop,
            handle: Some(handle),
        }
    }

    fn requests(&self) -> Vec<serde_json::Value> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn checked() -> SharedBudgetOutcome {
    SharedBudgetOutcome::Checked {
        state: SharedBudgetCheckState::Available,
        remaining_usd_cents: 4_000,
        period_end: "2027-02-01T00:00:00Z".into(),
    }
}

const GPT_OVERRIDE: &str = r#","pricing_overrides":{"gpt-5-codex":{"input_usd_per_mtok":1.25,"cache_creation_input_usd_per_mtok":1.25,"cache_read_input_usd_per_mtok":0.125,"output_usd_per_mtok":10.0}}"#;

// ---------------------------------------------------------------------
// Marker state and the owner route.
// ---------------------------------------------------------------------

#[test]
fn an_enrolled_install_reports_the_marker_state() {
    let env = Env::new(Some(""), true);
    let (severity, message) = env.doctor().expect("codex_enrolment");
    assert_eq!(severity, "ok", "{message}");
    assert!(message.contains("enrolled"), "{message}");
    assert!(message.contains("shared-budget.codex"), "{message}");
}

#[test]
fn a_marker_with_no_budget_file_warns_and_enrols_nothing() {
    let env = Env::new(None, true);
    let (severity, message) = env.doctor().expect("codex_enrolment");
    assert_eq!(severity, "warn", "{message}");
    assert!(message.contains("not enrolled"), "{message}");
}

#[test]
fn no_marker_prints_no_codex_enrolment_finding() {
    let env = Env::new(Some(""), false);
    let doctor = env.doctor();
    assert_eq!(doctor.codex_ids(), Vec::<String>::new(), "{}", doctor.json);
}

#[test]
fn owner_route_with_the_marker_is_an_error_and_fails_the_exit_code() {
    let env = Env::new(Some(GPT_OVERRIDE), true);
    let doctor = env.doctor_with(&[("LIBRA_GOVERNOR_CODEX_EXECUTION_OWNER", "1")]);
    let (severity, message) = doctor.expect("codex_enrolment");
    assert_eq!(severity, "error", "{message}");
    assert!(
        message.contains("LIBRA_GOVERNOR_CODEX_EXECUTION_OWNER"),
        "{message}"
    );
    assert_eq!(doctor.code, Some(1));
}

// ---------------------------------------------------------------------
// Hooks wired and trust.
// ---------------------------------------------------------------------

#[test]
fn enrolled_with_codex_hooks_not_installed_warns_ungated() {
    let env = Env::new(Some(""), true);
    let (severity, message) = env.doctor().expect("codex_enrolled_hooks");
    assert_eq!(severity, "warn", "{message}");
    assert!(message.contains("install --agent codex"), "{message}");
}

#[test]
fn wired_but_unproven_trust_is_likely_not_trusted_and_never_ok() {
    let env = Env::new(Some(""), true);
    env.install_codex_hooks();
    // A readable config with no [hooks.state] section: Codex has nowhere to
    // have recorded trust. (An absent config.toml is `unknown`, not this.)
    std::fs::write(env.codex_home.join("config.toml"), "[features]\n").unwrap();
    let (severity, message) = env.doctor().expect("codex_enrolled_hooks");
    assert_eq!(severity, "warn", "{message}");
    assert!(message.contains("likely not yet trusted"), "{message}");
    assert!(message.contains("/hooks"), "{message}");
}

#[test]
fn a_hooks_state_section_is_unknown_not_trusted() {
    let env = Env::new(Some(""), true);
    env.install_codex_hooks();
    std::fs::write(env.codex_home.join("config.toml"), "[hooks.state]\n").unwrap();
    let (severity, message) = env.doctor().expect("codex_enrolled_hooks");
    assert_eq!(severity, "warn", "{message}");
    assert!(message.contains("unknown"), "{message}");
    assert!(!message.contains("hooks are trusted"), "{message}");
}

// ---------------------------------------------------------------------
// Pricing.
// ---------------------------------------------------------------------

#[test]
fn no_gpt_pricing_override_warns_even_with_the_daemon_down() {
    let env = Env::new(Some(""), true);
    let (severity, message) = env.doctor().expect("codex_pricing");
    assert_eq!(severity, "warn", "{message}");
    assert!(message.contains("flat"), "{message}");
}

#[test]
fn a_non_gpt_override_does_not_count() {
    let other = r#","pricing_overrides":{"claude-x":{"input_usd_per_mtok":1.0,"cache_creation_input_usd_per_mtok":1.0,"cache_read_input_usd_per_mtok":0.1,"output_usd_per_mtok":5.0}}"#;
    let env = Env::new(Some(other), true);
    assert_eq!(env.doctor().expect("codex_pricing").0, "warn");
}

#[test]
fn a_gpt_pricing_override_is_ok() {
    let env = Env::new(Some(GPT_OVERRIDE), true);
    let (severity, message) = env.doctor().expect("codex_pricing");
    assert_eq!(severity, "ok", "{message}");
}

// ---------------------------------------------------------------------
// The stale-daemon probe.
// ---------------------------------------------------------------------

#[test]
fn a_daemon_answering_codex_check_not_configured_is_a_stale_daemon_error() {
    let env = Env::new(Some(GPT_OVERRIDE), true);
    let fake = Fake::start(&env.state, SharedBudgetOutcome::NotConfigured);
    let doctor = env.doctor();
    let (severity, message) = doctor.expect("codex_daemon_admission");
    assert_eq!(severity, "error", "{message}");
    assert!(
        message.contains(
            "enrolled but the running daemon is not honouring Codex admission \
             (restart/upgrade the daemon)"
        ),
        "{message}"
    );
    assert_eq!(doctor.code, Some(1));
    drop(fake);
}

#[test]
fn a_daemon_giving_a_real_codex_check_answer_is_ok() {
    let env = Env::new(Some(GPT_OVERRIDE), true);
    let fake = Fake::start(&env.state, checked());
    let doctor = env.doctor();
    let (severity, message) = doctor.expect("codex_daemon_admission");
    assert_eq!(severity, "ok", "{message}");
    drop(fake);
}

#[test]
fn the_probe_is_exactly_one_read_only_codex_check() {
    let env = Env::new(Some(GPT_OVERRIDE), true);
    let fake = Fake::start(&env.state, checked());
    env.doctor();
    let checks: Vec<serde_json::Value> = fake
        .requests()
        .into_iter()
        .filter(|r| r["shared_budget"]["op"] == "check")
        .collect();
    assert_eq!(checks.len(), 1, "{checks:?}");
    let check = &checks[0]["shared_budget"];
    assert_eq!(check["op"], "check", "{check}");
    assert_eq!(check["provider"], "codex", "{check}");
    assert!(check["agent_id"].is_null(), "{check}");
    assert!(
        check["session_id"]
            .as_str()
            .unwrap()
            .starts_with("libra-doctor-"),
        "a synthetic session id: {check}"
    );
    // Nothing that admits, charges or settles was sent.
    for request in fake.requests() {
        let op = request["shared_budget"]["op"].as_str().unwrap_or("");
        assert!(
            !matches!(op, "admit" | "settle_turn" | "charge_agent" | "settle_tail"),
            "{request}"
        );
    }
}

#[test]
fn an_unreachable_daemon_stays_a_warning_and_exits_zero() {
    let env = Env::new(Some(GPT_OVERRIDE), true);
    env.install_claude_hooks();
    env.install_codex_hooks();
    let doctor = env.doctor();
    let (severity, message) = doctor.expect("codex_daemon_admission");
    assert_eq!(severity, "warn", "{message}");
    assert_eq!(doctor.code, Some(0), "{}", doctor.json);
}

#[test]
fn no_marker_never_probes_the_daemon() {
    let env = Env::new(Some(GPT_OVERRIDE), false);
    let fake = Fake::start(&env.state, SharedBudgetOutcome::NotConfigured);
    let doctor = env.doctor();
    assert!(doctor.finding("codex_daemon_admission").is_none());
    for request in fake.requests() {
        assert_ne!(request["shared_budget"]["provider"], "codex", "{request}");
    }
}

// ---------------------------------------------------------------------
// Known limits, and the status list.
// ---------------------------------------------------------------------

#[test]
fn enrolled_doctor_states_the_known_limits() {
    let env = Env::new(Some(GPT_OVERRIDE), true);
    let (severity, message) = env.doctor().expect("codex_limits");
    assert_eq!(severity, "ok", "{message}");
    for needle in [
        "no heartbeat",
        "subagent",
        "without a turn_id",
        "unmapped",
        "PreToolUse",
        "/hooks",
    ] {
        assert!(message.contains(needle), "{needle}: {message}");
    }
}

#[test]
fn enrolled_status_json_no_longer_says_codex_has_no_gate() {
    let env = Env::new(Some(GPT_OVERRIDE), true);
    let out = env
        .command(&["shared-budget", "status", "--json"])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let joined = v["not_covered"].to_string();
    assert!(!joined.contains("no budget gate"), "{joined}");
    assert!(joined.contains("subagent"), "{joined}");
    assert_eq!(v["guarantee"], "admission-bounded (cooperative)", "{v}");
}

#[test]
fn unenrolled_status_json_keeps_the_original_list() {
    let env = Env::new(Some(GPT_OVERRIDE), false);
    let out = env
        .command(&["shared-budget", "status", "--json"])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["not_covered"].to_string().contains("no budget gate"));
}

/// HORO-2285 C5.AC: enrolment is reporting only. It never promotes the
/// guarantee label, and the Codex shared-budget capability row stays
/// `unavailable` (activation by default is a founder decision).
#[test]
fn codex_enrolment_leaves_the_guarantee_label_and_the_unavailable_agents_row_unchanged() {
    let mut labels = Vec::new();
    for marker in [false, true] {
        let env = Env::new(Some(GPT_OVERRIDE), marker);
        let status = env
            .command(&["shared-budget", "status", "--json"])
            .output()
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
        assert_eq!(v["guarantee"], "admission-bounded (cooperative)", "{v}");
        labels.push(v["guarantee"].clone());

        let agents = env.command(&["agents", "--json"]).output().unwrap();
        let v: serde_json::Value = serde_json::from_slice(&agents.stdout).unwrap();
        let codex = v["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["agent"] == "codex")
            .unwrap_or_else(|| panic!("no codex row: {v}"));
        let gate = &codex["shared_budget_admission_gate"];
        assert!(
            gate.get("unavailable").is_some(),
            "marker={marker}: the Codex gate row must stay unavailable: {gate}"
        );
    }
    assert_eq!(labels[0], labels[1], "the marker does not change the label");
}
