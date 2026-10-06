use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const PROFILE: &str = "libra.claude-hooks.v1";
const PYTHON: &str = "/usr/bin/python3";
const EXTERNAL_ID: &str = "fixture_operator_source";

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    state: PathBuf,
    home: PathBuf,
    cwd: PathBuf,
    target: PathBuf,
    marker: PathBuf,
    manifest: PathBuf,
    program: PathBuf,
}

fn fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_owned();
    let state = root.join("state");
    let home = root.join("home");
    let cwd = root.join("cwd");
    fs::create_dir(&home).unwrap();
    fs::create_dir(&cwd).unwrap();
    let target = home.join(".claude/settings.json");
    let marker = root.join("external-execution.marker");
    let program = root.join("passive_external.py");
    let source = r#"import pathlib
import sys
pathlib.Path(sys.argv[1]).write_text("external code executed", encoding="utf-8")
"#;
    fs::write(&program, source).unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    let manifest = root.join("external-manifest.json");

    Fixture {
        _temp: temp,
        root: root.clone(),
        state: state.clone(),
        home,
        cwd,
        target,
        marker,
        manifest,
        program,
    }
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

fn manifest(fixture: &Fixture) {
    let python_bytes = fs::read(PYTHON).expect("fixture Python interpreter is available");
    let source_bytes = fs::read(&fixture.program).unwrap();
    let value = json!({
        "manifest_kind": "host-adapter",
        "manifest_version": 1,
        "adapter_id": EXTERNAL_ID,
        "adapter_version": "1.2.3",
        "protocol_versions": [1],
        "contract_version_range": {"minimum": 1, "maximum": 1},
        "roles": ["LifecycleSource"],
        "capabilities": ["lifecycle.session"],
        "host_version_constraints": [{"provider": "fixture_host", "minimum": null, "maximum": null}],
        "configuration_schema": {
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {},
            "additionalProperties": false
        },
        "launch": {
            "executable": PYTHON,
            "argv": [fixture.program, fixture.marker]
        },
        "runtime_files": [
            {"path": PYTHON, "kind": "entrypoint", "digest": digest(&python_bytes)},
            {"path": fixture.program, "kind": "entrypoint", "digest": digest(&source_bytes)}
        ],
        "input_limits": {"max_bytes": 65536},
        "needs": {"environment": [], "read_paths": [], "write_paths": []}
    });
    fs::write(&fixture.manifest, serde_json::to_vec(&value).unwrap()).unwrap();
}

fn cli(fixture: &Fixture, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_libra-governor"))
        .args(args)
        .current_dir(&fixture.cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &fixture.home)
        .env("CODEX_HOME", fixture.home.join(".codex"))
        .env("LIBRA_GOVERNOR_STATE_DIR", &fixture.state)
        .output()
        .unwrap()
}

fn adapter_cli(fixture: &Fixture, args: &[&str]) -> Output {
    let mut full = vec!["adapter"];
    full.extend_from_slice(args);
    cli(fixture, &full)
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "expected a JSON operator response (exit={:?}, stdout bytes={}, stderr bytes={})",
            output.status.code(),
            output.stdout.len(),
            output.stderr.len()
        )
    })
}

#[derive(Debug, PartialEq, Eq)]
struct EntrySnapshot {
    path: PathBuf,
    mode: u32,
    device: u64,
    inode: u64,
    bytes: Option<Vec<u8>>,
}

fn inventory(root: &Path) -> Vec<EntrySnapshot> {
    fn collect(root: &Path, current: &Path, entries: &mut Vec<EntrySnapshot>) {
        let mut children = fs::read_dir(current)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        children.sort();
        for child in children {
            let metadata = fs::symlink_metadata(&child).unwrap();
            let file_type = metadata.file_type();
            assert!(
                !file_type.is_symlink(),
                "unexpected temporary fixture symlink"
            );
            entries.push(EntrySnapshot {
                path: child.strip_prefix(root).unwrap().to_owned(),
                mode: metadata.mode() & 0o7777,
                device: metadata.dev(),
                inode: metadata.ino(),
                bytes: file_type.is_file().then(|| fs::read(&child).unwrap()),
            });
            if file_type.is_dir() {
                collect(root, &child, entries);
            }
        }
    }
    let mut entries = Vec::new();
    if root.exists() {
        collect(root, root, &mut entries);
    }
    entries
}

fn assert_common_envelope(value: &Value, operation: &str) {
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["operation"], operation);
    assert!(value["result"].is_object());
    assert!(value["outcome"].is_string());
    assert!(value["reasons"].is_array());
    assert_eq!(value["verification_state"], "unverified");
}

fn assert_no_private_text(fixture: &Fixture, output: &Output) {
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for private in [
        fixture.root.to_str().unwrap(),
        fixture.target.to_str().unwrap(),
        fixture.program.to_str().unwrap(),
        fixture.marker.to_str().unwrap(),
        "external code executed",
    ] {
        assert!(
            !text.contains(private),
            "operator output exposed private fixture data"
        );
    }
}

fn assert_local_state(value: &Value, expected: &str) {
    let rows = value["result"]["local_installations"]
        .as_array()
        .expect("every operator projection returns local_installations");
    if expected == "uninstalled" {
        assert!(rows.is_empty());
        return;
    }
    let local = rows
        .iter()
        .find(|row| row["profile"] == PROFILE)
        .expect("builtin profile status is present");
    assert_eq!(local["installed"], true);
    assert_eq!(local["native_verification"], "unverified");
    assert_eq!(local["host_trust"], "unknown");
    assert_eq!(local["observed_execution"], "unknown");
    assert_eq!(local["effective_support"], "unknown");
    match expected {
        "disabled" => {
            assert_eq!(local["desired_enabled"], false);
            assert_eq!(local["local_connection"], "disconnected");
            assert_eq!(local["integrity"], "verified");
        }
        "connected" => {
            assert_eq!(local["desired_enabled"], true);
            assert_eq!(local["local_connection"], "connected");
            assert_eq!(local["integrity"], "verified");
        }
        "unknown" => {
            assert_eq!(local["desired_enabled"], true);
            assert_eq!(local["local_connection"], "unknown");
            assert_eq!(local["integrity"], "unknown");
        }
        _ => unreachable!(),
    }
}

fn assert_passive_command(fixture: &Fixture, command: &str, state: &str) -> Value {
    let args = match command {
        "status" => vec!["status", "--json"],
        "doctor" => vec!["doctor", "--json"],
        "explain" => vec!["explain", "claude_code", "--json"],
        _ => unreachable!(),
    };
    let output = adapter_cli(fixture, &args);
    assert!(output.status.success(), "passive {command} failed");
    assert_no_private_text(fixture, &output);
    let value = json(&output);
    assert_common_envelope(&value, command);
    assert_local_state(&value, state);
    value
}

fn assert_safe_text(fixture: &Fixture, command: &str, args: &[&str]) {
    let output = adapter_cli(fixture, args);
    assert!(output.status.success(), "text {command} failed");
    assert!(output.stdout.len() < 4096, "text {command} was not bounded");
    assert_no_private_text(fixture, &output);
}

fn register_and_confirm_external(fixture: &Fixture) {
    manifest(fixture);
    let manifest_path = fixture.manifest.to_str().unwrap();
    let registered = adapter_cli(fixture, &["register", manifest_path, "--json"]);
    assert!(
        registered.status.success(),
        "external metadata registration failed"
    );
    assert_no_private_text(fixture, &registered);
    assert!(!fixture.marker.exists());

    let before_review = inventory(&fixture.root);
    for operation in ["status", "doctor", "explain"] {
        let passive = adapter_cli(fixture, &[operation, EXTERNAL_ID, "--json"]);
        assert!(passive.status.success());
        assert_no_private_text(fixture, &passive);
        let value = json(&passive);
        assert_common_envelope(&value, operation);
        let metadata = if operation == "explain" {
            &value["result"]
        } else {
            adapter_metadata(&value, EXTERNAL_ID)
        };
        assert_eq!(metadata["code_trust"], "not_reviewed");
        assert_eq!(metadata["host_trust"], "unknown");
        assert_eq!(metadata["observed_execution"], "unknown");
        assert!(!fixture.marker.exists());
        assert_eq!(inventory(&fixture.root), before_review);
    }

    let reviewed = adapter_cli(
        fixture,
        &["inspect", EXTERNAL_ID, "--review-code-trust", "--json"],
    );
    assert!(reviewed.status.success(), "actual code-trust review failed");
    let digest = json(&reviewed)["result"]["confirmation_digest"]
        .as_str()
        .expect("review provides the current confirmation digest")
        .to_owned();
    let confirmed = adapter_cli(
        fixture,
        &[
            "register",
            manifest_path,
            "--confirm-code-digest",
            &digest,
            "--json",
        ],
    );
    assert!(
        confirmed.status.success(),
        "actual code-trust confirmation failed"
    );
    assert_no_private_text(fixture, &confirmed);
    assert!(!fixture.marker.exists());
}

fn adapter_metadata<'a>(value: &'a Value, id: &str) -> &'a Value {
    value["result"]["adapters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["adapter_id"] == id)
        .expect("registered adapter metadata appears in passive result")
}

#[test]
fn an_actual_failed_disable_remains_pending_during_all_operator_queries() {
    let fixture = fixture();
    for operation in ["install", "enable"] {
        let output = adapter_cli(
            &fixture,
            &[
                operation,
                "claude_code",
                "--scope",
                "user",
                "--profile",
                PROFILE,
                "--json",
            ],
        );
        assert!(output.status.success());
    }
    // Closing the gate precedes observing the changed target. A real failed
    // disable therefore leaves owned pending state, without forged metadata.
    fs::write(&fixture.target, b"{invalid-config").unwrap();
    let disabled = adapter_cli(
        &fixture,
        &[
            "disable",
            "claude_code",
            "--scope",
            "user",
            "--profile",
            PROFILE,
            "--json",
        ],
    );
    assert!(!disabled.status.success());
    let before = inventory(&fixture.root);
    for operation in ["status", "doctor", "explain"] {
        let output = adapter_cli(&fixture, &[operation, "claude_code", "--json"]);
        assert!(output.status.success());
        assert_no_private_text(&fixture, &output);
        let value = json(&output);
        assert_common_envelope(&value, operation);
        let rows = value["result"]["local_installations"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row["installed"], true);
        assert_eq!(row["desired_enabled"], false);
        assert_eq!(row["pending_operation"], "disable");
        assert_eq!(row["local_connection"], "pending");
        assert_eq!(row["integrity"], "unknown");
        assert_eq!(row["reason"], "pending_operation");
        assert_eq!(row["host_trust"], "unknown");
        assert_eq!(row["observed_execution"], "unknown");
        assert_eq!(row["native_verification"], "unverified");
        if operation == "explain" {
            assert!(value["result"]["summary"]
                .as_str()
                .unwrap()
                .contains("pending"));
        }
        assert_eq!(inventory(&fixture.root), before);
        assert!(!fixture.marker.exists());
    }
    assert_eq!(fs::read(&fixture.target).unwrap(), b"{invalid-config");
}

#[test]
fn fresh_missing_state_passive_json_and_text_commands_create_nothing() {
    let fixture = fixture();
    let before = inventory(&fixture.root);
    for command in ["status", "doctor", "explain"] {
        let value = assert_passive_command(&fixture, command, "uninstalled");
        if command == "explain" {
            assert_eq!(value["result"]["host_trust"], "unknown");
            assert_eq!(value["result"]["observed_execution"], "unknown");
        }
        assert_eq!(inventory(&fixture.root), before);
    }
    assert_safe_text(&fixture, "status", &["status"]);
    assert_safe_text(&fixture, "doctor", &["doctor"]);
    assert_safe_text(&fixture, "explain", &["explain", "claude_code"]);
    assert_eq!(inventory(&fixture.root), before);
    assert!(!fixture.state.exists());
    assert!(!fixture.home.join(".claude/settings.json").exists());
}

#[test]
fn lifecycle_status_doctor_and_explain_track_actual_install_enable_disable_uninstall() {
    let fixture = fixture();
    let install = adapter_cli(
        &fixture,
        &[
            "install",
            "claude_code",
            "--scope",
            "user",
            "--profile",
            PROFILE,
            "--json",
        ],
    );
    assert!(install.status.success());
    for command in ["status", "doctor", "explain"] {
        let value = assert_passive_command(&fixture, command, "disabled");
        if command == "explain" {
            let summary = value["result"]["summary"].as_str().unwrap().to_lowercase();
            assert!(summary.contains("disabled") || summary.contains("disconnected"));
        }
    }

    for (operation, expected) in [("enable", "connected"), ("disable", "disabled")] {
        let changed = adapter_cli(
            &fixture,
            &[
                operation,
                "claude_code",
                "--scope",
                "user",
                "--profile",
                PROFILE,
                "--json",
            ],
        );
        assert!(changed.status.success(), "builtin {operation} failed");
        for command in ["status", "doctor", "explain"] {
            let value = assert_passive_command(&fixture, command, expected);
            if command == "explain" {
                let summary = value["result"]["summary"].as_str().unwrap().to_lowercase();
                assert!(summary.contains(expected));
            }
        }
    }

    let removed = adapter_cli(
        &fixture,
        &[
            "uninstall",
            "claude_code",
            "--scope",
            "user",
            "--profile",
            PROFILE,
            "--json",
        ],
    );
    assert!(removed.status.success());
    for command in ["status", "doctor", "explain"] {
        let value = assert_passive_command(&fixture, command, "uninstalled");
        if command == "explain" {
            let summary = value["result"]["summary"].as_str().unwrap().to_lowercase();
            assert!(summary.contains("uninstall") || summary.contains("not installed"));
        }
    }
}

#[test]
fn registered_and_reviewed_external_metadata_never_becomes_host_trust_or_execution() {
    let fixture = fixture();
    let install = adapter_cli(
        &fixture,
        &[
            "install",
            "claude_code",
            "--scope",
            "user",
            "--profile",
            PROFILE,
            "--json",
        ],
    );
    assert!(install.status.success());
    register_and_confirm_external(&fixture);

    let before = inventory(&fixture.root);
    for command in ["status", "doctor"] {
        let output = adapter_cli(&fixture, &[command, EXTERNAL_ID, "--json"]);
        assert!(
            output.status.success(),
            "passive {command} failed for external id"
        );
        assert_no_private_text(&fixture, &output);
        let value = json(&output);
        assert_common_envelope(&value, command);
        let external = adapter_metadata(&value, EXTERNAL_ID);
        assert_eq!(external["origin"], "registered");
        assert_eq!(external["code_trust"], "recorded");
        assert_eq!(external["host_trust"], "unknown");
        assert_eq!(external["observed_execution"], "unknown");
        assert_eq!(external["effective_support"], "unknown");
        // Selecting a registered planner never attributes the builtin
        // consumer's installation to that unrelated runtime adapter ID.
        assert!(value["result"]["local_installations"]
            .as_array()
            .unwrap()
            .is_empty());
        assert!(!fixture.marker.exists());
        assert_eq!(inventory(&fixture.root), before);
    }

    let explain = adapter_cli(&fixture, &["explain", EXTERNAL_ID, "--json"]);
    assert!(explain.status.success());
    assert_no_private_text(&fixture, &explain);
    let value = json(&explain);
    assert_common_envelope(&value, "explain");
    assert_eq!(value["result"]["code_trust"], "recorded");
    assert_eq!(value["result"]["host_trust"], "unknown");
    assert_eq!(value["result"]["observed_execution"], "unknown");
    assert_eq!(value["result"]["effective_support"], "unknown");
    assert_eq!(value["result"]["native_verification"], "unverified");
    assert!(value["result"]["local_installations"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(!fixture.marker.exists());
    assert_eq!(inventory(&fixture.root), before);

    for (command, args) in [
        ("status", vec!["status"]),
        ("doctor", vec!["doctor"]),
        ("explain", vec!["explain", EXTERNAL_ID]),
    ] {
        assert_safe_text(&fixture, command, &args);
        assert!(!fixture.marker.exists());
    }
}

#[test]
fn target_drift_reports_unknown_integrity_and_preserves_recorded_enabled_intent() {
    let fixture = fixture();
    let install = adapter_cli(
        &fixture,
        &[
            "install",
            "claude_code",
            "--scope",
            "user",
            "--profile",
            PROFILE,
            "--json",
        ],
    );
    assert!(install.status.success());
    let enable = adapter_cli(
        &fixture,
        &[
            "enable",
            "claude_code",
            "--scope",
            "user",
            "--profile",
            PROFILE,
            "--json",
        ],
    );
    assert!(enable.status.success());
    let canary = "PRIVATE_TARGET_CONFIGURATION_DRIFT";
    fs::write(
        &fixture.target,
        serde_json::to_vec(&json!({"foreign": canary, "hooks": {"Stop": []}})).unwrap(),
    )
    .unwrap();
    let changed = fs::read(&fixture.target).unwrap();
    let before = inventory(&fixture.root);

    for command in ["status", "doctor", "explain"] {
        let output = if command == "explain" {
            adapter_cli(&fixture, &["explain", "claude_code", "--json"])
        } else {
            adapter_cli(&fixture, &[command, "--json"])
        };
        assert!(
            output.status.success(),
            "passive {command} failed on target drift"
        );
        assert_no_private_text(&fixture, &output);
        let value = json(&output);
        assert_common_envelope(&value, command);
        let local = value["result"]["local_installations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["profile"] == PROFILE)
            .unwrap();
        assert_eq!(local["installed"], true);
        assert_eq!(local["desired_enabled"], true);
        assert_eq!(local["local_connection"], "unknown");
        assert_eq!(local["integrity"], "unknown");
        assert_eq!(local["reason"], "connection_changed");
        assert_eq!(local["host_trust"], "unknown");
        assert_eq!(local["observed_execution"], "unknown");
        assert_eq!(local["native_verification"], "unverified");
        assert!(!fixture.marker.exists());
        assert_eq!(fs::read(&fixture.target).unwrap(), changed);
        assert_eq!(inventory(&fixture.root), before);
    }
}
