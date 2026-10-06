use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const ADAPTER_ID: &str = "external_probe_fixture";
const PYTHON: &str = "/usr/bin/python3";
const BASE_MANIFEST: &str =
    include_str!("../../protocol/contracts/host-adapter/v1/fixtures/valid-manifest-synthetic.json");
const SNAPSHOT: &[u8] = include_bytes!(
    "../../protocol/contracts/host-adapter/v1/fixtures/valid-snapshot-codex-shape-only-unknown.json"
);
const DRIVER: &str = include_str!("../../daemon/tests/fixtures/external_probe_driver.py");
const SOURCE_CANARY: &str = "file:///private/candidate-evidence/never-print-this-canary";

struct Fixture {
    _temp: TempDir,
    state: PathBuf,
    home: PathBuf,
    cwd: PathBuf,
    manifest: PathBuf,
    driver: PathBuf,
    snapshot: PathBuf,
    registry: PathBuf,
    operation_log: PathBuf,
}

fn digest(bytes: &[u8]) -> String {
    format!(
        "sha256:{}",
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

fn path_string(path: &Path) -> String {
    path.to_str().expect("temporary paths are UTF-8").to_owned()
}

fn fixture(mode: &str) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let home = temp.path().join("home");
    let cwd = temp.path().join("unrelated-project");
    fs::create_dir(&home).unwrap();
    fs::create_dir(&cwd).unwrap();

    let driver = temp.path().join("external_probe_driver.py");
    let snapshot_path = temp.path().join("candidate_snapshot.json");
    let operation_log = temp.path().join("driver-operations.log");
    let registry = state.join("host-adapters/registry.json");
    fs::write(&driver, DRIVER).unwrap();
    let mut snapshot: Value = serde_json::from_slice(SNAPSHOT).unwrap();
    snapshot["capabilities"][0]["evidence"][0]["source_ref"] =
        Value::String(SOURCE_CANARY.to_owned());
    fs::write(&snapshot_path, serde_json::to_vec(&snapshot).unwrap()).unwrap();

    let python_bytes = fs::read(PYTHON).expect("the selected Python interpreter exists");
    let driver_bytes = fs::read(&driver).unwrap();
    let snapshot_bytes = fs::read(&snapshot_path).unwrap();
    let mut manifest_value: Value = serde_json::from_str(BASE_MANIFEST).unwrap();
    manifest_value["adapter_id"] = json!(ADAPTER_ID);
    manifest_value["launch"] = json!({
        "executable": PYTHON,
        "argv": [path_string(&driver), path_string(&registry), path_string(&operation_log), path_string(&snapshot_path), mode]
    });
    manifest_value["runtime_files"] = json!([
        {"path":PYTHON,"kind":"entrypoint","digest":digest(&python_bytes)},
        {"path":path_string(&driver),"kind":"entrypoint","digest":digest(&driver_bytes)},
        {"path":path_string(&snapshot_path),"kind":"dependency","digest":digest(&snapshot_bytes)}
    ]);
    let manifest_path = temp.path().join("external-adapter-manifest.json");
    fs::write(&manifest_path, serde_json::to_vec(&manifest_value).unwrap()).unwrap();

    Fixture {
        _temp: temp,
        state,
        home,
        cwd,
        manifest: manifest_path,
        driver,
        snapshot: snapshot_path,
        registry,
        operation_log,
    }
}

fn binary() -> PathBuf {
    std::env::var_os("LIBRA_ADAPTER_PROBE_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_libra-governor")))
}

fn cli(fixture: &Fixture, args: &[&str]) -> Output {
    Command::new(binary())
        .args(args)
        .current_dir(&fixture.cwd)
        .env("LIBRA_GOVERNOR_STATE_DIR", &fixture.state)
        .env("HOME", &fixture.home)
        .env("CODEX_HOME", fixture.home.join(".codex"))
        .env_remove("XDG_STATE_HOME")
        .output()
        .unwrap()
}

fn adapter_cli(fixture: &Fixture, args: &[&str]) -> Output {
    let mut all = vec!["adapter"];
    all.extend_from_slice(args);
    cli(fixture, &all)
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "stdout was not JSON: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn output_context(output: &Output) -> String {
    let bounded = |bytes: &[u8]| {
        let text = String::from_utf8_lossy(bytes).replace(SOURCE_CANARY, "<candidate-canary>");
        text.chars().take(2048).collect::<String>()
    };
    format!(
        "status={:?} stdout={:?} stderr={:?}",
        output.status,
        bounded(&output.stdout),
        bounded(&output.stderr)
    )
}

fn fixture_counts(fixture: &Fixture) -> String {
    let starts = fs::read_to_string(format!("{}.starts", fixture.operation_log.display()))
        .map(|text| text.lines().count());
    let operations = fs::read_to_string(&fixture.operation_log).map(|text| {
        (
            text.lines().filter(|line| *line == "handshake").count(),
            text.lines().filter(|line| *line == "probe").count(),
        )
    });
    format!("recorded_starts={starts:?} handshake_probe_counts={operations:?}")
}

fn review_and_confirm(fixture: &Fixture) -> String {
    let manifest = fixture.manifest.to_str().unwrap();
    let review = adapter_cli(
        fixture,
        &["inspect", ADAPTER_ID, "--review-code-trust", "--json"],
    );
    assert!(review.status.success(), "{}", output_context(&review));
    let digest = json(&review)["result"]["confirmation_digest"]
        .as_str()
        .expect("review exposes the actual confirmation digest")
        .to_owned();
    let confirmed = adapter_cli(
        fixture,
        &[
            "register",
            manifest,
            "--confirm-code-digest",
            &digest,
            "--json",
        ],
    );
    assert!(confirmed.status.success(), "{}", output_context(&confirmed));
    digest
}

fn enroll_and_confirm(fixture: &Fixture) -> String {
    let manifest = fixture.manifest.to_str().unwrap();
    let registered = adapter_cli(fixture, &["register", manifest, "--json"]);
    assert!(
        registered.status.success(),
        "{}",
        output_context(&registered)
    );
    review_and_confirm(fixture)
}

fn operation_log(fixture: &Fixture) -> Vec<String> {
    let operations: Vec<String> = fs::read_to_string(&fixture.operation_log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    let starts = fs::read_to_string(format!("{}.starts", fixture.operation_log.display()))
        .unwrap_or_default()
        .lines()
        .count();
    assert_eq!(starts, operations.len(), "fixture start/request mismatch");
    operations
}

fn stop_direct_cli(child: &mut std::process::Child) {
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
    }
    let _ = child.wait();
}

fn persisted_confirmation(fixture: &Fixture) -> Value {
    let registry: Value = serde_json::from_slice(&fs::read(&fixture.registry).unwrap()).unwrap();
    registry["adapters"][ADAPTER_ID]["trust"]["confirmation_digest"].clone()
}

#[test]
fn probe_is_passive_until_real_cli_trust_then_reports_only_conservative_summary() {
    let fixture = fixture("normal");
    let initial = adapter_cli(&fixture, &["doctor", "missing_adapter", "--json"]);
    assert!(!initial.status.success());
    assert_eq!(json(&initial)["reasons"][0], "unknown_adapter");
    assert!(operation_log(&fixture).is_empty());
    assert!(!fixture.state.exists());

    let manifest = fixture.manifest.to_str().unwrap();
    let registered = adapter_cli(&fixture, &["register", manifest, "--json"]);
    assert!(registered.status.success());
    let before = fs::read(&fixture.registry).unwrap();
    let untrusted = adapter_cli(&fixture, &["doctor", ADAPTER_ID, "--probe", "--json"]);
    assert!(!untrusted.status.success());
    assert_eq!(json(&untrusted)["outcome"], "refused");
    assert!(operation_log(&fixture).is_empty());
    assert_eq!(fs::read(&fixture.registry).unwrap(), before);
    let inspect = adapter_cli(&fixture, &["inspect", ADAPTER_ID, "--json"]);
    assert_eq!(json(&inspect)["result"]["code_trust"], "not_reviewed");

    let _confirmation = review_and_confirm(&fixture);
    let before_probe = fs::read(&fixture.registry).unwrap();
    let trust_before = persisted_confirmation(&fixture);
    let probe = adapter_cli(
        &fixture,
        &[
            "doctor", ADAPTER_ID, "--probe", "--scope", "project", "--json",
        ],
    );
    assert!(
        probe.status.success(),
        "{} {}",
        fixture_counts(&fixture),
        output_context(&probe)
    );
    let result = json(&probe);
    assert_eq!(result["operation"], "doctor");
    assert_eq!(result["outcome"], "partial");
    assert_eq!(result["reasons"], json!(["candidate_protocol_validated"]));
    assert_eq!(result["verification_state"], "unverified");
    assert_eq!(result["scope"], "project");
    assert_eq!(result["result"]["adapter_id"], ADAPTER_ID);
    assert_eq!(result["result"]["capability_count"], 1);
    assert_eq!(result["result"]["candidate_protocol"], "validated");
    assert_eq!(result["result"]["native_effect"], "unverified");
    assert_eq!(result["result"]["host_trust"], "unknown");
    assert_eq!(result["result"]["effective_support"], "unknown");
    let output = String::from_utf8_lossy(&probe.stdout);
    assert!(!output.contains(SOURCE_CANARY));
    assert!(!output.contains("installed"));
    assert!(!output.contains("trusted"));
    assert!(!output.contains("host_id"));
    assert!(!output.contains("ledger"));
    assert!(!output.contains("socket"));
    assert!(!output.contains("host_config"));
    assert!(!output.contains("source_ref"));
    assert!(!output.contains(fixture.manifest.to_str().unwrap()));
    assert!(!output.contains(fixture.driver.to_str().unwrap()));
    assert!(!output.contains(fixture.snapshot.to_str().unwrap()));
    assert!(!output.contains(fixture.operation_log.to_str().unwrap()));
    assert_eq!(operation_log(&fixture), ["handshake", "probe"]);
    assert_eq!(fs::read(&fixture.registry).unwrap(), before_probe);
    assert_eq!(persisted_confirmation(&fixture), trust_before);
    let state_entries = fs::read_dir(&fixture.state)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(state_entries, ["host-adapters"]);
    let catalog_entries = fs::read_dir(fixture.state.join("host-adapters"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(catalog_entries, ["registry.json"]);
    assert!(!fixture.home.join(".config").exists());
    assert!(!fixture.home.join(".codex").exists());

    let removed = adapter_cli(&fixture, &["unregister", ADAPTER_ID, "--json"]);
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
}

#[test]
fn malformed_probe_command_matrix_is_state_and_process_free() {
    let fixture = fixture("normal");
    enroll_and_confirm(&fixture);
    let before = fs::read(&fixture.registry).unwrap();
    let commands: &[&[&str]] = &[
        &["doctor", "--probe", "--json"],
        &["doctor", ADAPTER_ID, "--probe", "--probe", "--json"],
        &[
            "doctor", ADAPTER_ID, "--probe", "--scope", "user", "--scope", "project", "--json",
        ],
        &[
            "doctor", ADAPTER_ID, "--probe", "--scope", "invalid", "--json",
        ],
        &["doctor", ADAPTER_ID, "--probe", "--scope", "--json"],
        &["doctor", ADAPTER_ID, "--scope", "user", "--json"],
        &["list", "--scope", "user", "--json"],
        &["doctor", ADAPTER_ID, "other", "--probe", "--json"],
        &["doctor", ADAPTER_ID, "--probe", "--dry-run", "--json"],
        &[
            "doctor",
            ADAPTER_ID,
            "--probe",
            "--profile",
            "default",
            "--json",
        ],
        &["doctor", ADAPTER_ID, "--probe", "--yes", "--json"],
    ];
    for args in commands {
        let output = adapter_cli(&fixture, args);
        assert!(!output.status.success(), "accepted {args:?}");
        assert!(operation_log(&fixture).is_empty(), "spawned for {args:?}");
        assert_eq!(fs::read(&fixture.registry).unwrap(), before, "{args:?}");
    }
}

#[test]
fn trusted_wrong_handshake_correlation_fails_after_one_child_without_candidate_leak() {
    let fixture = fixture("wrong_handshake_id");
    enroll_and_confirm(&fixture);
    let before = fs::read(&fixture.registry).unwrap();
    let probe = adapter_cli(&fixture, &["doctor", ADAPTER_ID, "--probe", "--json"]);
    assert!(!probe.status.success());
    let result = json(&probe);
    assert_eq!(result["outcome"], "failed");
    assert_eq!(result["reasons"][0], "protocol_refused");
    assert_eq!(result["result"]["execution_attempted"], true);
    assert_eq!(result["result"]["filesystem_effect"], "not_asserted");
    assert_eq!(operation_log(&fixture), ["handshake"]);
    assert_eq!(fs::read(&fixture.registry).unwrap(), before);
    assert!(!String::from_utf8_lossy(&probe.stdout).contains(SOURCE_CANARY));
    assert!(!String::from_utf8_lossy(&probe.stderr).contains(SOURCE_CANARY));
}

#[test]
fn trusted_text_probe_does_not_render_candidate_evidence_or_paths() {
    let fixture = fixture("normal");
    enroll_and_confirm(&fixture);
    let before = fs::read(&fixture.registry).unwrap();
    let probe = adapter_cli(&fixture, &["doctor", ADAPTER_ID, "--probe"]);
    assert!(
        probe.status.success(),
        "{} {}",
        fixture_counts(&fixture),
        output_context(&probe)
    );
    let stdout = String::from_utf8_lossy(&probe.stdout);
    let stderr = String::from_utf8_lossy(&probe.stderr);
    assert!(stdout.contains("partial"));
    assert!(stdout.contains("candidate_protocol_validated"));
    assert!(!stdout.contains(SOURCE_CANARY));
    assert!(!stderr.contains(SOURCE_CANARY));
    assert!(!stdout.contains(fixture.manifest.to_str().unwrap()));
    assert!(!stdout.contains(fixture.driver.to_str().unwrap()));
    assert!(!stdout.contains(fixture.snapshot.to_str().unwrap()));
    assert!(!stdout.contains(fixture.operation_log.to_str().unwrap()));
    assert_eq!(operation_log(&fixture), ["handshake", "probe"]);
    assert_eq!(fs::read(&fixture.registry).unwrap(), before);
}

#[cfg(unix)]
#[test]
fn cli_sigint_cancels_probe_and_waits_for_fixture_process_group_cleanup() {
    use rustix::process::{self, Pid, Signal};

    let fixture = fixture("wait_for_signal");
    enroll_and_confirm(&fixture);
    let stdout_path = fixture._temp.path().join("probe-stdout.json");
    let stderr_path = fixture._temp.path().join("probe-stderr.txt");
    let stdout_file = fs::File::create(&stdout_path).unwrap();
    let stderr_file = fs::File::create(&stderr_path).unwrap();
    let mut child = Command::new(binary())
        .args(["adapter", "doctor", ADAPTER_ID, "--probe", "--json"])
        .current_dir(&fixture.cwd)
        .env("LIBRA_GOVERNOR_STATE_DIR", &fixture.state)
        .env("HOME", &fixture.home)
        .env("CODEX_HOME", fixture.home.join(".codex"))
        .env_remove("XDG_STATE_HOME")
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .unwrap();
    let cli_pid = Pid::from_raw(child.id() as i32).unwrap();
    let marker_path = PathBuf::from(format!("{}.pid", fixture.operation_log.display()));
    let marker_deadline = Instant::now() + Duration::from_secs(2);
    while !marker_path.exists() && Instant::now() < marker_deadline {
        if child.try_wait().unwrap().is_some() {
            panic!("probe CLI exited before the fixture announced its PID");
        }
        thread::sleep(Duration::from_millis(5));
    }
    if !marker_path.exists() {
        stop_direct_cli(&mut child);
        let stdout = fs::read(&stdout_path).unwrap_or_default();
        let stderr = fs::read(&stderr_path).unwrap_or_default();
        let starts = fs::read_to_string(format!("{}.starts", fixture.operation_log.display()))
            .unwrap_or_default()
            .lines()
            .count();
        // Failure diagnostics must not invoke the normal log-correlation
        // assertions: a started process may not have parsed its request yet.
        let operations = fs::read_to_string(&fixture.operation_log).unwrap_or_default();
        let handshakes = operations
            .lines()
            .filter(|line| *line == "handshake")
            .count();
        let probes = operations.lines().filter(|line| *line == "probe").count();
        panic!(
            "fixture did not announce its handshake PID: recorded_starts={starts} handshakes={handshakes} probes={probes} {}",
            output_context(&Output {
                status: child.wait().unwrap(),
                stdout,
                stderr,
            })
        );
    }
    let fixture_pid_raw = match fs::read_to_string(&marker_path)
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
    {
        Some(pid) => pid,
        None => {
            stop_direct_cli(&mut child);
            panic!("fixture PID marker was malformed");
        }
    };
    let fixture_pid = Pid::from_raw(fixture_pid_raw).expect("fixture PID is positive");
    if child.try_wait().unwrap().is_some() {
        panic!("probe CLI exited before cancellation was requested");
    }

    process::kill_process(cli_pid, Signal::INT).unwrap();
    thread::sleep(Duration::from_millis(20));
    if child.try_wait().unwrap().is_none() {
        let _ = process::kill_process(cli_pid, Signal::TERM);
    }

    let deadline = Instant::now() + Duration::from_secs(8);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            stop_direct_cli(&mut child);
            panic!("probe CLI did not finish within the bounded cancellation window");
        }
        thread::sleep(Duration::from_millis(10));
    };
    assert!(!status.success());

    assert_eq!(
        process::test_kill_process(fixture_pid),
        Err(rustix::io::Errno::SRCH)
    );
    assert_eq!(
        process::test_kill_process_group(fixture_pid),
        Err(rustix::io::Errno::SRCH)
    );
    let output = fs::read(&stdout_path).unwrap();
    let stdout: Value = serde_json::from_slice(&output).expect("cancellation emits JSON");
    assert_eq!(stdout["outcome"], "failed");
    assert_eq!(stdout["reasons"][0], "probe_cancelled");
    assert_eq!(stdout["result"]["execution_attempted"], true);
    assert_eq!(stdout["result"]["filesystem_effect"], "not_asserted");
    assert_eq!(fs::read(&stderr_path).unwrap(), b"");
    assert_eq!(operation_log(&fixture), ["handshake"]);
}

#[test]
fn trusted_execution_failures_have_fixed_categories_without_candidate_output() {
    for (mode, category) in [
        ("nonzero_exit", "nonzero_exit"),
        ("wait_for_signal", "timeout"),
    ] {
        let fixture = fixture(mode);
        enroll_and_confirm(&fixture);
        let before = fs::read(&fixture.registry).unwrap();
        let probe = adapter_cli(&fixture, &["doctor", ADAPTER_ID, "--probe", "--json"]);
        assert!(!probe.status.success());
        let result = json(&probe);
        assert_eq!(result["reasons"][0], "probe_execution_failed");
        assert_eq!(
            result["result"]["execution_failure"],
            category,
            "{} {}",
            fixture_counts(&fixture),
            output_context(&probe)
        );
        assert_eq!(result["result"]["execution_attempted"], true);
        assert_eq!(result["result"]["filesystem_effect"], "not_asserted");
        assert_eq!(operation_log(&fixture), ["handshake"]);
        assert_eq!(fs::read(&fixture.registry).unwrap(), before);
        assert!(!String::from_utf8_lossy(&probe.stdout).contains(SOURCE_CANARY));
        assert!(!String::from_utf8_lossy(&probe.stderr).contains(SOURCE_CANARY));
    }
}
