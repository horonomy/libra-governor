//! End-to-end tests of the real `libra-governor evidence-report` /
//! `evidence-report consent` subcommands (HORO-1154). Mirrors
//! `doctor_uninstall_integration.rs`'s pattern: spawn the real built
//! binary as a subprocess, drive it with real env vars and a real temp
//! state dir, assert on its real stdout/stderr/exit code/files.
//!
//! # What this file is the automated evidence for
//!
//! - **Consent gating**: `evidence-report` refuses (exit 1, explanatory
//!   message, no report files written) with no consent on record, and
//!   succeeds once `evidence-report consent` has run.
//! - **Real aggregate counts**: seeding one real admitted Preflight (via
//!   the same `handle_connection` dispatch `daemon run`'s accept loop
//!   calls — the established pattern in
//!   `crates/daemon/tests/preflight_integration.rs` /
//!   `mvp3_gate_evidence.rs`) into the ledger the CLI will read, and
//!   asserting the exported JSON's aggregate counts match exactly.
//! - **Privacy**: the real Preflight's `task_hint` carries a nonce.
//!   After generating a real local export, this test greps the actual
//!   written JSON and Markdown files (and the CLI's own stdout) for that
//!   nonce and asserts it never appears anywhere — the same nonce-grep
//!   technique `mvp3_gate_evidence.rs` uses for the gateway/prompt path,
//!   applied here to the evidence-report export path specifically.

use std::io::{BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Once;

use libra_governor_daemon::{recon::ReconBudget, DaemonConfig};
use libra_governor_ledger::LedgerStore;
use libra_governor_protocol::{
    wire, Request, RequestEnvelope, Response, ResponseEnvelope, PROTOCOL_VERSION,
};

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

fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../daemon/tests/fixtures/sample_repo")
}

struct Sandbox {
    _state_parent: tempfile::TempDir,
    state_dir: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        warm_up_binary();
        let state_parent = tempfile::tempdir().unwrap();
        let state_dir = state_parent.path().join("state");
        Sandbox {
            _state_parent: state_parent,
            state_dir,
        }
    }

    fn run_with_stdin(&self, args: &[&str], stdin_text: &str) -> std::process::Output {
        let mut child = Command::new(bin())
            .args(args)
            .env("LIBRA_GOVERNOR_STATE_DIR", &self.state_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin_text.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        self.run_with_stdin(args, "")
    }

    /// Seeds the sandbox's ledger with one real, admitted Preflight
    /// through the real daemon dispatch function, whose `task_hint`
    /// carries `nonce`. This is the same file `evidence-report` will
    /// later open at `paths::ledger_path()` for this state dir.
    fn seed_one_real_preflight_with_nonce(&self, nonce: &str) {
        std::fs::create_dir_all(&self.state_dir).unwrap();
        let config = DaemonConfig {
            socket_path: self.state_dir.join("seed.sock"),
            ledger_path: self.state_dir.join("ledger.sqlite3"),
            log_path: self.state_dir.join("daemon.log"),
            recon_budget: ReconBudget::default(),
            replan_hysteresis: libra_governor_domain::ReplanHysteresisConfig::default(),
            policy: libra_governor_daemon::default_admission_policy(),
            reservation_ttl_secs: 900,
            gateway: None,
            gateway_stats: std::sync::Arc::new(Default::default()),
            gateway_session_header: libra_governor_gateway::proxy::DEFAULT_SESSION_HEADER
                .to_string(),
        };
        let listener = libra_governor_daemon::bind_or_detect_running(&config.socket_path).unwrap();
        let socket_path = config.socket_path.clone();
        let ledger_path = config.ledger_path.clone();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut ledger = LedgerStore::open(&ledger_path).unwrap();
            let mut current_task = None;
            libra_governor_daemon::handle_connection(
                stream,
                &mut ledger,
                &mut current_task,
                &config,
            )
            .unwrap();
        });

        let client = UnixStream::connect(&socket_path).unwrap();
        let envelope = RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request: Request::Preflight {
                task_hint: format!("fix the login bug — {nonce}"),
                cwd: fixture_repo(),
                session_id: "evidence-report-privacy-session".to_string(),
            },
        };
        wire::write_message(&client, &envelope).unwrap();
        let response: ResponseEnvelope =
            wire::read_message(BufReader::new(client.try_clone().unwrap())).unwrap();
        match response.response {
            Response::Preflight(_) => {}
            other => panic!("expected a Preflight response, got {other:?}"),
        }
        server.join().unwrap();
    }
}

#[test]
fn evidence_report_refuses_without_consent_and_writes_nothing() {
    let sandbox = Sandbox::new();
    let output = sandbox.run(&["evidence-report"]);

    assert!(
        !output.status.success(),
        "evidence-report must refuse to run without prior consent"
    );
    let stderr = String::from_utf8_lossy(&output.stderr).to_lowercase();
    assert!(
        stderr.contains("consent"),
        "the refusal must explain that consent is required: {stderr}"
    );
    let reports_dir = sandbox.state_dir.join("evidence-reports");
    assert!(
        !reports_dir.exists(),
        "no report should ever be written when consent is missing"
    );
}

#[test]
fn evidence_report_consent_then_evidence_report_succeeds_and_writes_files() {
    let sandbox = Sandbox::new();

    let consent_output = sandbox.run(&["evidence-report", "consent"]);
    assert!(
        consent_output.status.success(),
        "consent must succeed: {}",
        String::from_utf8_lossy(&consent_output.stderr)
    );
    let consent_path = sandbox.state_dir.join("evidence_consent.json");
    assert!(
        consent_path.exists(),
        "consent marker must be written to disk"
    );

    let report_output = sandbox.run(&["evidence-report"]);
    assert!(
        report_output.status.success(),
        "evidence-report must succeed once consent is on record: {}",
        String::from_utf8_lossy(&report_output.stderr)
    );

    let reports_dir = sandbox.state_dir.join("evidence-reports");
    let entries: Vec<_> = std::fs::read_dir(&reports_dir).unwrap().collect();
    assert_eq!(
        entries.len(),
        2,
        "exactly one .json and one .md file should be written per run"
    );
}

#[test]
fn evidence_report_aggregates_match_real_seeded_ledger_state_exactly() {
    let sandbox = Sandbox::new();
    sandbox.seed_one_real_preflight_with_nonce("NONCE-HORO1154-aggregate-count-check");
    sandbox.run(&["evidence-report", "consent"]);
    let output = sandbox.run(&["evidence-report"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let reports_dir = sandbox.state_dir.join("evidence-reports");
    let json_path = std::fs::read_dir(&reports_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .expect("a .json export must exist");
    let json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&json_path).unwrap()).unwrap();

    assert_eq!(json["ledger_present"], true);
    assert_eq!(json["aggregates"]["task_count"], 1);
    assert_eq!(json["aggregates"]["preflight_count"], 1);
    assert_eq!(json["aggregates"]["replan_count"], 0);
    assert_eq!(json["aggregates"]["completed_task_count"], 0);
}

#[test]
fn evidence_report_export_never_contains_the_real_prompt_nonce() {
    let sandbox = Sandbox::new();
    const NONCE: &str = "NONCE-HORO1154-7b2f9c-do-not-leak-this-prompt-text";
    sandbox.seed_one_real_preflight_with_nonce(NONCE);

    let consent_output = sandbox.run(&["evidence-report", "consent"]);
    assert!(consent_output.status.success());

    // The qualitative-answer prompts are answered with real free text
    // that deliberately does NOT contain the nonce, mirroring what a
    // real evaluator would type.
    let answers = "a bit of friction at first, got used to it\n\
                    yes, probably\n\
                    my team hasn't asked yet\n\
                    maybe $20/mo\n\
                    no other notes\n";
    let report_output = sandbox.run_with_stdin(&["evidence-report"], answers);
    assert!(
        report_output.status.success(),
        "{}",
        String::from_utf8_lossy(&report_output.stderr)
    );

    // 1) Never in the CLI's own stdout/stderr.
    let stdout = String::from_utf8_lossy(&report_output.stdout);
    let stderr = String::from_utf8_lossy(&report_output.stderr);
    assert!(
        !stdout.contains(NONCE),
        "nonce leaked into evidence-report stdout"
    );
    assert!(
        !stderr.contains(NONCE),
        "nonce leaked into evidence-report stderr"
    );

    // 2) Never in any file under the state dir — the exported JSON/MD,
    // the ledger itself (sanity — the schema has no column for this),
    // or the daemon log.
    fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap().filter_map(|e| e.ok()) {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                walk(&path, out);
            } else if file_type.is_file() {
                // Skip non-regular files (the seeding helper's Unix
                // domain socket, in particular — reading it errors with
                // "Operation not supported").
                out.push(path);
            }
        }
    }
    let mut all_files = Vec::new();
    walk(&sandbox.state_dir, &mut all_files);
    assert!(
        !all_files.is_empty(),
        "sanity: the state dir must actually contain files to check"
    );
    for path in &all_files {
        let bytes = std::fs::read(path).unwrap();
        assert!(
            !bytes.windows(NONCE.len()).any(|w| w == NONCE.as_bytes()),
            "the real prompt nonce leaked into {}",
            path.display()
        );
    }
}
