use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const PLANNER: &str = "external_config_fixture";
const PROFILE: &str = "libra.claude-hooks.v1";
const PYTHON: &str = "/usr/bin/python3";
const BASE_MANIFEST: &str =
    include_str!("../../protocol/contracts/host-adapter/v1/fixtures/valid-manifest-synthetic.json");
const DRIVER: &str = include_str!("../../daemon/tests/fixtures/external_config_driver.py");
const PRIVATE_CANARY: &str = "PRIVATE_CONFIG_DRIVER_ERROR";

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
    state: PathBuf,
    home: PathBuf,
    cwd: PathBuf,
    target: PathBuf,
    registry: PathBuf,
    operation_log: PathBuf,
    driver: PathBuf,
    foreign: Value,
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
    path.to_str().expect("fixture path is UTF-8").to_owned()
}

fn make_fixture() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_owned();
    let state = root.join("state");
    let home = root.join("home");
    let cwd = root.join("unrelated-worktree");
    fs::create_dir(&state).unwrap();
    fs::create_dir(&home).unwrap();
    fs::create_dir(&cwd).unwrap();
    let target = home.join(".claude/settings.json");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    let foreign = json!({
        "model": "fixture-model",
        "statusLine": {"type": "command", "command": "fixture-status"},
        "mcpServers": {"fixture": {"command": "fixture-mcp"}},
        "future": {"nested": [1, true, null]}
    });
    fs::write(&target, serde_json::to_vec_pretty(&foreign).unwrap()).unwrap();

    let driver = root.join("external_config_driver.py");
    let operation_log = root.join("config-driver-operations.log");
    fs::write(&driver, DRIVER).unwrap();

    Fixture {
        _temp: temp,
        root: root.clone(),
        state: state.clone(),
        home,
        cwd,
        target,
        registry: state.join("host-adapters/registry.json"),
        operation_log,
        driver,
        foreign,
    }
}

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_libra-governor"))
}

fn cli(fixture: &Fixture, args: &[&str]) -> Output {
    let mut child = Command::new(binary())
        .args(args)
        .current_dir(&fixture.cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &fixture.home)
        .env("CODEX_HOME", fixture.home.join(".codex"))
        .env("LIBRA_GOVERNOR_STATE_DIR", &fixture.state)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("CLI process exceeded fixture watchdog");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

fn adapter_cli(fixture: &Fixture, args: &[&str]) -> Output {
    let mut all = vec!["adapter"];
    all.extend_from_slice(args);
    cli(fixture, &all)
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "expected JSON response (exit={:?}, stdout bytes={}, stderr bytes={})",
            output.status.code(),
            output.stdout.len(),
            output.stderr.len()
        )
    })
}

fn assert_private_output(fixture: &Fixture, output: &Output) {
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for private in [
        path_string(&fixture.root),
        path_string(&fixture.driver),
        path_string(&fixture.registry),
        path_string(&fixture.operation_log),
        PRIVATE_CANARY.to_owned(),
    ] {
        assert!(
            !combined.contains(&private),
            "CLI output exposed private fixture data"
        );
    }
}

fn inventory(root: &Path, operation_log: &Path) -> Vec<(PathBuf, u64, u64, u32, Option<String>)> {
    let excluded = BTreeSet::from([
        operation_log.to_owned(),
        PathBuf::from(format!("{}.entry", operation_log.display())),
        PathBuf::from(format!("{}.starts", operation_log.display())),
        PathBuf::from(format!("{}.handshake.pid", operation_log.display())),
        PathBuf::from(format!("{}.plan_config.pid", operation_log.display())),
    ]);
    fn visit(
        root: &Path,
        path: &Path,
        excluded: &BTreeSet<PathBuf>,
        result: &mut Vec<(PathBuf, u64, u64, u32, Option<String>)>,
    ) {
        if excluded.contains(path) {
            return;
        }
        let metadata = fs::symlink_metadata(path).unwrap();
        assert!(
            !metadata.file_type().is_symlink(),
            "unexpected fixture symlink"
        );
        result.push((
            path.strip_prefix(root).unwrap().to_path_buf(),
            metadata.dev(),
            metadata.ino(),
            metadata.mode(),
            metadata.is_file().then(|| digest(&fs::read(path).unwrap())),
        ));
        if metadata.is_dir() {
            let mut children = fs::read_dir(path)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect::<Vec<_>>();
            children.sort();
            for child in children {
                visit(root, &child, excluded, result);
            }
        }
    }
    let mut result = Vec::new();
    visit(root, root, &excluded, &mut result);
    result
}

fn operations(fixture: &Fixture) -> Vec<String> {
    fs::read_to_string(&fixture.operation_log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn assert_markers(fixture: &Fixture, expected: &[&str]) {
    assert_eq!(operations(fixture), expected);
    let count = expected.len();
    let starts = fs::read_to_string(format!("{}.starts", fixture.operation_log.display()))
        .unwrap_or_default();
    let entries = fs::read_to_string(format!("{}.entry", fixture.operation_log.display()))
        .unwrap_or_default();
    assert_eq!(starts.lines().count(), count);
    assert_eq!(entries.lines().count(), count);
    for operation in ["handshake", "plan_config"] {
        let marker = PathBuf::from(format!(
            "{}.{}.pid",
            fixture.operation_log.display(),
            operation
        ));
        if expected.contains(&operation) {
            let pid = fs::read_to_string(marker).unwrap();
            assert!(pid.parse::<u32>().is_ok());
        }
    }
}

fn register_external(fixture: &Fixture, id: &str, mode: &str, confirm: bool) {
    register_external_variant(
        fixture,
        id,
        mode,
        confirm,
        &["ConfigDriver"],
        "claude_code",
        &[1],
    );
}

fn register_external_variant(
    fixture: &Fixture,
    id: &str,
    mode: &str,
    confirm: bool,
    roles: &[&str],
    provider: &str,
    protocol_versions: &[u32],
) {
    let manifest_path = fixture.root.join(format!("{id}.manifest.json"));
    let python_bytes = fs::read(PYTHON).expect("fixture interpreter must be installed");
    let driver_bytes = fs::read(&fixture.driver).unwrap();
    let mut manifest: Value = serde_json::from_str(BASE_MANIFEST).unwrap();
    manifest["adapter_id"] = json!(id);
    manifest["adapter_version"] = json!("1.0.0");
    manifest["roles"] = json!(roles);
    manifest["capabilities"] = json!([]);
    manifest["protocol_versions"] = json!(protocol_versions);
    manifest["host_version_constraints"] = json!([{
        "provider": provider,
        "minimum": null,
        "maximum": null
    }]);
    manifest["launch"] = json!({
        "executable": PYTHON,
        "argv": [
            path_string(&fixture.driver),
            path_string(&fixture.registry),
            path_string(&fixture.operation_log),
            mode
        ]
    });
    manifest["runtime_files"] = json!([
        {"path": PYTHON, "kind": "entrypoint", "digest": digest(&python_bytes)},
        {"path": path_string(&fixture.driver), "kind": "entrypoint", "digest": digest(&driver_bytes)}
    ]);
    manifest["needs"] = json!({"environment": [], "read_paths": [], "write_paths": []});
    manifest["input_limits"]["max_bytes"] = json!(1_048_576);
    fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let manifest_argument = path_string(&manifest_path);
    let registered = adapter_cli(fixture, &["register", &manifest_argument, "--json"]);
    assert!(registered.status.success(), "external registration refused");
    assert_private_output(fixture, &registered);
    if confirm {
        let reviewed = adapter_cli(fixture, &["inspect", id, "--review-code-trust", "--json"]);
        assert!(reviewed.status.success(), "code review refused");
        let confirmation = json(&reviewed)["result"]["confirmation_digest"]
            .as_str()
            .expect("review exposes confirmation digest")
            .to_owned();
        let confirmed = adapter_cli(
            fixture,
            &[
                "register",
                &manifest_argument,
                "--confirm-code-digest",
                &confirmation,
                "--json",
            ],
        );
        assert!(
            confirmed.status.success(),
            "actual trust confirmation refused"
        );
        assert_private_output(fixture, &confirmed);
    }
}

fn install_builtin(fixture: &Fixture, operation: &str) -> Output {
    adapter_cli(
        fixture,
        &[
            operation,
            "claude_code",
            "--scope",
            "user",
            "--profile",
            PROFILE,
            "--json",
        ],
    )
}

fn plan(fixture: &Fixture, id: &str, intent: &str, dry_run: bool) -> Output {
    let mut args = vec!["plan", id, "--profile", PROFILE, "--intent", intent];
    if dry_run {
        args.push("--dry-run");
    }
    args.push("--json");
    adapter_cli(fixture, &args)
}

fn assert_valid_preview(fixture: &Fixture, output: &Output, intent: &str, actions: &[&str]) {
    assert!(output.status.success(), "validated preview did not succeed");
    assert_private_output(fixture, output);
    let envelope = json(output);
    assert_eq!(envelope["operation"], "plan");
    assert_eq!(envelope["adapter_id"], PLANNER);
    assert_eq!(envelope["scope"], "user");
    assert_eq!(envelope["outcome"], "partial");
    assert_eq!(envelope["reasons"], json!(["plan_validated"]));
    assert_eq!(envelope["verification_state"], "unverified");
    let result = &envelope["result"];
    assert_eq!(result["profile"], PROFILE);
    assert_eq!(result["consumer_adapter_id"], "claude_code");
    assert_eq!(result["planned_intent"], intent);
    assert_eq!(result["plan_validation"], "validated");
    assert_eq!(result["configuration_effect"], "not_applied");
    assert_eq!(
        result["configuration_change_required"],
        actions
            .iter()
            .any(|action| *action == "add" || *action == "remove")
    );
    assert_eq!(result["apply_available"], false);
    assert_eq!(result["execution_attempted"], true);
    assert_eq!(result["filesystem_effect"], "not_asserted");
    assert_eq!(result["native_verification"], "unverified");
    let slots = result["slots"].as_array().unwrap();
    assert_eq!(slots.len(), 3);
    let mut found = slots
        .iter()
        .map(|slot| slot["action"].as_str().unwrap())
        .collect::<Vec<_>>();
    found.sort_unstable();
    let mut expected = actions.to_vec();
    expected.sort_unstable();
    assert_eq!(found, expected);
}

fn assert_attempted_refusal(fixture: &Fixture, output: &Output) {
    assert!(
        !output.status.success(),
        "invalid external plan unexpectedly succeeded"
    );
    assert_private_output(fixture, output);
    let envelope = json(output);
    assert_eq!(envelope["result"]["execution_attempted"], true);
    assert_eq!(envelope["result"]["filesystem_effect"], "not_asserted");
    assert_eq!(envelope["verification_state"], "failed");
}

#[test]
fn real_config_driver_preview_adds_preserves_and_removes_without_applying() {
    let fixture = make_fixture();
    let installed = install_builtin(&fixture, "install");
    assert!(installed.status.success());
    register_external(&fixture, PLANNER, "positive", true);

    let before = inventory(&fixture.root, &fixture.operation_log);
    let enable_add = plan(&fixture, PLANNER, "enable", false);
    assert_valid_preview(&fixture, &enable_add, "enable", &["add", "add", "add"]);
    assert_markers(&fixture, &["handshake", "plan_config"]);
    assert_eq!(inventory(&fixture.root, &fixture.operation_log), before);

    let enabled = install_builtin(&fixture, "enable");
    assert!(enabled.status.success());
    assert_eq!(json(&enabled)["result"]["desired_enabled"], true);

    let before = inventory(&fixture.root, &fixture.operation_log);
    let enable_preserve = plan(&fixture, PLANNER, "enable", false);
    assert_valid_preview(
        &fixture,
        &enable_preserve,
        "enable",
        &["preserve", "preserve", "preserve"],
    );
    assert_eq!(
        operations(&fixture),
        ["handshake", "plan_config", "handshake", "plan_config"]
    );
    assert_eq!(inventory(&fixture.root, &fixture.operation_log), before);

    let before = inventory(&fixture.root, &fixture.operation_log);
    let disable_remove = plan(&fixture, PLANNER, "disable", false);
    assert_valid_preview(
        &fixture,
        &disable_remove,
        "disable",
        &["remove", "remove", "remove"],
    );
    assert_eq!(inventory(&fixture.root, &fixture.operation_log), before);

    let disabled = install_builtin(&fixture, "disable");
    assert!(disabled.status.success());
    assert_eq!(json(&disabled)["result"]["desired_enabled"], false);
    let before = inventory(&fixture.root, &fixture.operation_log);
    let uninstall = plan(&fixture, PLANNER, "uninstall", false);
    assert_valid_preview(
        &fixture,
        &uninstall,
        "uninstall",
        &["preserve", "preserve", "preserve"],
    );
    assert_eq!(inventory(&fixture.root, &fixture.operation_log), before);
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(&fixture.target).unwrap()).unwrap()["model"],
        fixture.foreign["model"]
    );
}

#[test]
fn plan_preserves_absent_target_revision_for_a_missing_settings_file() {
    let fixture = make_fixture();
    fs::remove_file(&fixture.target).unwrap();
    assert!(install_builtin(&fixture, "install").status.success());
    register_external(&fixture, PLANNER, "positive", true);

    let before = inventory(&fixture.root, &fixture.operation_log);
    let preview = plan(&fixture, PLANNER, "enable", false);
    assert_valid_preview(&fixture, &preview, "enable", &["add", "add", "add"]);
    assert_markers(&fixture, &["handshake", "plan_config"]);
    assert_eq!(inventory(&fixture.root, &fixture.operation_log), before);
    assert!(!fixture.target.exists());
}

#[test]
fn valid_dry_run_and_invalid_cli_or_preflight_cases_start_no_driver() {
    let fixture = make_fixture();
    let installed = install_builtin(&fixture, "install");
    assert!(installed.status.success());
    register_external(&fixture, PLANNER, "positive", true);

    let before = inventory(&fixture.root, &fixture.operation_log);
    let dry_run = plan(&fixture, PLANNER, "enable", true);
    assert!(
        dry_run.status.success(),
        "dry-run did not return eligibility result"
    );
    assert_private_output(&fixture, &dry_run);
    let envelope = json(&dry_run);
    assert_eq!(envelope["outcome"], "partial");
    assert_eq!(envelope["result"]["profile"], PROFILE);
    assert_eq!(envelope["reasons"], json!(["external_plan_required"]));
    assert_eq!(
        envelope["result"]["plan_validation"],
        "external_plan_required"
    );
    assert!(envelope["result"]["configuration_change_required"].is_null());
    assert_eq!(envelope["result"]["execution_attempted"], false);
    assert_eq!(envelope["result"]["filesystem_effect"], "unchanged");
    assert_eq!(envelope["result"]["slots"], json!([]));
    assert_markers(&fixture, &[]);
    assert_eq!(inventory(&fixture.root, &fixture.operation_log), before);

    for args in [
        vec!["plan", PLANNER, "--profile", PROFILE, "--json"],
        vec![
            "plan",
            PLANNER,
            "--profile",
            PROFILE,
            "--intent",
            "install",
            "--json",
        ],
        vec![
            "plan",
            PLANNER,
            "--profile",
            "future.profile",
            "--intent",
            "enable",
            "--json",
        ],
        vec![
            "plan",
            PLANNER,
            "--profile",
            PROFILE,
            "--intent",
            "enable",
            "--scope",
            "project",
            "--json",
        ],
        vec![
            "plan",
            "unknown_config_driver",
            "--profile",
            PROFILE,
            "--intent",
            "enable",
            "--json",
        ],
    ] {
        let output = adapter_cli(&fixture, &args);
        assert!(
            !output.status.success(),
            "unsupported plan arguments unexpectedly succeeded"
        );
        assert_private_output(&fixture, &output);
        assert_markers(&fixture, &[]);
        assert_eq!(inventory(&fixture.root, &fixture.operation_log), before);
    }

    let untrusted = make_fixture();
    assert!(install_builtin(&untrusted, "install").status.success());
    register_external(&untrusted, PLANNER, "positive", false);
    let before = inventory(&untrusted.root, &untrusted.operation_log);
    let refused = plan(&untrusted, PLANNER, "enable", false);
    assert!(!refused.status.success());
    assert_private_output(&untrusted, &refused);
    assert_markers(&untrusted, &[]);
    assert_eq!(inventory(&untrusted.root, &untrusted.operation_log), before);

    let no_installation = make_fixture();
    register_external(&no_installation, PLANNER, "positive", true);
    let before = inventory(&no_installation.root, &no_installation.operation_log);
    let refused = plan(&no_installation, PLANNER, "enable", false);
    assert!(!refused.status.success());
    assert_private_output(&no_installation, &refused);
    assert_markers(&no_installation, &[]);
    assert_eq!(
        inventory(&no_installation.root, &no_installation.operation_log),
        before
    );

    let incompatible = make_fixture();
    assert!(install_builtin(&incompatible, "install").status.success());
    for (index, roles, provider, versions) in [
        (0, vec!["LifecycleSource"], "claude_code", vec![1]),
        (1, vec!["ConfigDriver"], "unrelated_host", vec![1]),
        (2, vec!["ConfigDriver"], "claude_code", vec![2]),
    ] {
        let id = format!("external_config_incompatible_{index}");
        register_external_variant(
            &incompatible,
            &id,
            "positive",
            true,
            &roles,
            provider,
            &versions,
        );
        let before = inventory(&incompatible.root, &incompatible.operation_log);
        let refused = plan(&incompatible, &id, "enable", false);
        assert!(!refused.status.success());
        assert_private_output(&incompatible, &refused);
        assert_markers(&incompatible, &[]);
        assert_eq!(
            inventory(&incompatible.root, &incompatible.operation_log),
            before
        );
    }
}

#[test]
fn profile_schema_accepts_mathematical_integer_float_but_rejects_bool_and_bad_versions() {
    let fixture = make_fixture();
    assert!(install_builtin(&fixture, "install").status.success());
    register_external(&fixture, PLANNER, "versions_float", true);
    let before = inventory(&fixture.root, &fixture.operation_log);
    let preview = plan(&fixture, PLANNER, "enable", false);
    assert_valid_preview(&fixture, &preview, "enable", &["add", "add", "add"]);
    assert_markers(&fixture, &["handshake", "plan_config"]);
    assert_eq!(inventory(&fixture.root, &fixture.operation_log), before);

    for (index, mode) in [
        "invalid_bool_version",
        "invalid_version",
        "invalid_validator_bool",
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("external_config_invalid_version_{index}");
        register_external(&fixture, &id, mode, true);
        let before = inventory(&fixture.root, &fixture.operation_log);
        let output = plan(&fixture, &id, "enable", false);
        assert_attempted_refusal(&fixture, &output);
        let expected = operations(&fixture);
        assert_eq!(
            &expected[expected.len() - 2..],
            &["handshake", "plan_config"]
        );
        assert_eq!(inventory(&fixture.root, &fixture.operation_log), before);
    }
}

#[test]
fn independently_invalid_driver_plans_are_refused_after_correlated_two_call_exchange() {
    let fixture = make_fixture();
    assert!(install_builtin(&fixture, "install").status.success());
    let modes = [
        "wrong_product",
        "wrong_binding",
        "wrong_installation",
        "wrong_plan_request_id",
        "wrong_response_class",
        "duplicate_json",
        "wrong_validator_ref",
        "wrong_validator_digest",
        "wrong_target_revision",
        "wrong_callback_ref",
        "wrong_owned_digest",
        "wrong_action",
        "wrong_event",
        "wrong_matcher",
        "duplicate_slot",
        "extra_field",
    ];
    let mut prior_operations = Vec::new();
    for (index, mode) in modes.into_iter().enumerate() {
        let id = format!("external_config_invalid_plan_{index}");
        register_external(&fixture, &id, mode, true);
        let before = inventory(&fixture.root, &fixture.operation_log);
        let output = plan(&fixture, &id, "enable", false);
        assert_attempted_refusal(&fixture, &output);
        let recorded = operations(&fixture);
        assert_eq!(recorded.len(), prior_operations.len() + 2);
        assert_eq!(
            &recorded[prior_operations.len()..],
            &["handshake", "plan_config"]
        );
        assert_eq!(inventory(&fixture.root, &fixture.operation_log), before);
        prior_operations = recorded;
    }
}

#[test]
fn handshake_and_runner_failures_remain_redacted_and_preserve_local_files() {
    let fixture = make_fixture();
    assert!(install_builtin(&fixture, "install").status.success());
    for (index, mode, expected) in [
        (0, "handshake_error", vec!["handshake"]),
        (1, "wrong_handshake_id", vec!["handshake"]),
        (2, "timeout", vec!["handshake", "plan_config"]),
        (3, "nonzero", vec!["handshake", "plan_config"]),
        (4, "stdout_flood", vec!["handshake", "plan_config"]),
        (5, "stderr_flood", vec!["handshake", "plan_config"]),
    ] {
        let id = format!("external_config_runner_failure_{index}");
        register_external(&fixture, &id, mode, true);
        let before = inventory(&fixture.root, &fixture.operation_log);
        let output = plan(&fixture, &id, "enable", false);
        assert!(
            !output.status.success(),
            "runner failure mode unexpectedly succeeded"
        );
        assert_private_output(&fixture, &output);
        let recorded = operations(&fixture);
        assert_eq!(&recorded[recorded.len() - expected.len()..], expected);
        if expected.len() == 2 {
            assert_attempted_refusal(&fixture, &output);
        }
        assert_eq!(inventory(&fixture.root, &fixture.operation_log), before);
    }
}

#[test]
fn local_builtin_cleanup_does_not_depend_on_registered_planner() {
    let fixture = make_fixture();
    assert!(install_builtin(&fixture, "install").status.success());
    assert!(install_builtin(&fixture, "enable").status.success());
    register_external(&fixture, PLANNER, "positive", true);
    let preview = plan(&fixture, PLANNER, "enable", false);
    assert_valid_preview(
        &fixture,
        &preview,
        "enable",
        &["preserve", "preserve", "preserve"],
    );
    let before_unregister = operations(&fixture);

    let removed = adapter_cli(&fixture, &["unregister", PLANNER, "--json"]);
    assert!(removed.status.success(), "planner unregister failed");
    let disabled = install_builtin(&fixture, "disable");
    assert!(
        disabled.status.success(),
        "builtin disable depended on planner"
    );
    let uninstalled = install_builtin(&fixture, "uninstall");
    assert!(
        uninstalled.status.success(),
        "builtin uninstall depended on planner"
    );
    assert_eq!(operations(&fixture), before_unregister);
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(&fixture.target).unwrap()).unwrap(),
        fixture.foreign
    );
    let registry: Value = serde_json::from_slice(&fs::read(&fixture.registry).unwrap()).unwrap();
    assert!(registry["adapters"].get(PLANNER).is_none());
    assert!(registry["installations"].as_object().unwrap().is_empty());
    assert!(!fixture.state.join("ledger.sqlite3").exists());
}

#[test]
fn complete_plan_request_limit_is_checked_before_the_smaller_handshake() {
    let handshake = json!({"protocol":"horonom.host-adapter","offered_versions":[1],
        "request_id":"12345678-1234-4234-8234-123456789abc","operation":"handshake"});
    assert!(serde_json::to_vec(&handshake).unwrap().len() < 512);
    let fixture = make_fixture();
    assert!(install_builtin(&fixture, "install").status.success());
    register_external(&fixture, PLANNER, "positive", true);
    assert!(adapter_cli(&fixture, &["unregister", PLANNER, "--json"])
        .status
        .success());
    let path = fixture.root.join(format!("{PLANNER}.manifest.json"));
    let mut manifest: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    manifest["input_limits"]["max_bytes"] = json!(512);
    fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let path = path_string(&path);
    assert!(adapter_cli(&fixture, &["register", &path, "--json"])
        .status
        .success());
    let review = adapter_cli(
        &fixture,
        &["inspect", PLANNER, "--review-code-trust", "--json"],
    );
    assert!(review.status.success());
    let confirmation = json(&review)["result"]["confirmation_digest"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(adapter_cli(
        &fixture,
        &[
            "register",
            &path,
            "--confirm-code-digest",
            &confirmation,
            "--json"
        ]
    )
    .status
    .success());
    let before = inventory(&fixture.root, &fixture.operation_log);
    for dry_run in [false, true] {
        let refused = plan(&fixture, PLANNER, "enable", dry_run);
        assert!(!refused.status.success());
        let result = json(&refused);
        assert_eq!(result["result"]["execution_attempted"], false);
        assert_eq!(result["result"]["execution_failure"], "input_limit");
        assert_private_output(&fixture, &refused);
        assert_markers(&fixture, &[]);
        assert_eq!(inventory(&fixture.root, &fixture.operation_log), before);
    }
}
