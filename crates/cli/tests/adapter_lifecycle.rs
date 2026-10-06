use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap()
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

fn inventory(root: &Path) -> Vec<(PathBuf, u64, u64, u32, Option<String>)> {
    fn visit(root: &Path, path: &Path, result: &mut Vec<(PathBuf, u64, u64, u32, Option<String>)>) {
        let metadata = fs::symlink_metadata(path).unwrap();
        assert!(
            !metadata.file_type().is_symlink(),
            "unexpected fixture alias"
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
                visit(root, &child, result);
            }
        }
    }
    let mut result = Vec::new();
    visit(root, root, &mut result);
    result
}

fn profile_args(
    operation: &str,
    adapter: &str,
    scope: &str,
    profile: &str,
    dry_run: bool,
) -> Vec<String> {
    let mut args = vec![
        "adapter".into(),
        operation.into(),
        adapter.into(),
        "--scope".into(),
        scope.into(),
        "--profile".into(),
        profile.into(),
    ];
    if dry_run {
        args.push("--dry-run".into());
    }
    args.push("--json".into());
    args
}

fn run_profile(temp: &Path, home: &Path, state: &Path, args: &[String]) -> Output {
    let output = Command::new(env!("CARGO_BIN_EXE_libra-governor"))
        .args(args)
        .env_clear()
        .env("HOME", home)
        .env("LIBRA_GOVERNOR_STATE_DIR", state)
        .current_dir(temp)
        .output()
        .unwrap();
    if !output.status.success() {
        eprintln!(
            "profile fixture product bytes: {}",
            fs::metadata(env!("CARGO_BIN_EXE_libra-governor"))
                .unwrap()
                .len()
        );
    }
    output
}

fn assert_closed_gate_with_blocking_stdin(
    home: &Path,
    state: &Path,
    binding: &str,
    installation: &str,
    slot: &str,
) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_libra-governor"))
        .args([
            "adapter-hook",
            "--state-root",
            state.to_str().unwrap(),
            "--binding",
            binding,
            "--installation",
            installation,
            "--slot",
            slot,
        ])
        .env_clear()
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Keep the parent's pipe writer open and send no bytes. A consumer that
    // reads stdin cannot finish; closing it before exit would make this vacuous.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("closed installed callback waited for stdin");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
}

#[test]
fn builtin_install_creates_a_real_disabled_binding_without_connecting_host_hooks() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let state = temp.path().join("state");
    fs::create_dir(&home).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_libra-governor"))
        .args([
            "adapter",
            "install",
            "claude_code",
            "--scope",
            "user",
            "--profile",
            "libra.claude-hooks.v1",
            "--json",
        ])
        .env_clear()
        .env("HOME", &home)
        .env("LIBRA_GOVERNOR_STATE_DIR", &state)
        .current_dir(temp.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "installation refused: {:?}",
        output.status.code()
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["operation"], "install");
    assert_eq!(result["adapter_id"], "claude_code");
    assert_eq!(result["result"]["installed"], true);
    assert_eq!(result["result"]["desired_enabled"], false);
    assert_eq!(result["result"]["local_connection"], "disconnected");
    assert!(state.is_dir());
    assert!(!home.join(".claude/settings.json").exists());
    assert!(!state.join("host_id").exists());
    let registry: Value =
        serde_json::from_slice(&fs::read(state.join("host-adapters/registry.json")).unwrap())
            .unwrap();
    assert_eq!(registry["schema_version"], 2);
    let records = registry["installations"].as_object().unwrap();
    assert_eq!(records.len(), 1);
    let (binding, record) = records.iter().next().unwrap();
    assert_eq!(record["installed"], true);
    assert_eq!(record["pending"], Value::Null);
    assert_eq!(record["connection"], Value::Null);
    let installation = record["installation_id"].as_str().unwrap();
    let artifact_bytes = fs::read(
        state
            .join("host-adapters/installations")
            .join(format!("{installation}.json")),
    )
    .unwrap();
    assert_eq!(record["artifact_sha256"], digest(&artifact_bytes));
    let artifact: Value = serde_json::from_slice(&artifact_bytes).unwrap();
    assert_eq!(artifact["binding_id"], binding.as_str());
    assert_eq!(artifact["installation_id"], installation);
    assert_eq!(artifact["registry_id"], registry["registry_id"]);
    assert_eq!(artifact["validator_ref"], record["validator_ref"]);
    assert_eq!(artifact["context"], record["context"]);
    assert_eq!(artifact["binary"], record["binary"]);
    assert_eq!(artifact["slots"].as_array().unwrap().len(), 3);
    let state_entries = inventory(&state);
    assert_eq!(state_entries.len(), 5); // root, two owned dirs, catalog, artifact
    let before = inventory(temp.path());
    for slot in ["prompt_submit", "tool_completed", "turn_completed"] {
        assert_closed_gate_with_blocking_stdin(&home, &state, binding, installation, slot);
        assert_eq!(inventory(temp.path()), before);
    }
    let missing = temp.path().join("missing-state");
    assert_closed_gate_with_blocking_stdin(&home, &missing, binding, installation, "prompt_submit");
    assert_eq!(inventory(temp.path()), before);
}

#[test]
fn builtin_lifecycle_preserves_foreign_configuration_and_distinguishes_each_state() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let state = temp.path().join("state");
    let target = home.join(".claude/settings.json");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    let before = serde_json::json!({"model":"foreign-model","statusLine":{"type":"command","command":"foreign-status"},
        "mcpServers":{"foreign":{"command":"foreign-mcp"}},"enabledPlugins":{"foreign@catalog":true},
        "future":{"unknown":[1,true]},"hooks":{"Stop":[{"matcher":"foreign","future":17,
            "hooks":[{"type":"command","command":"foreign-product","future":true}]}]}});
    fs::write(&target, serde_json::to_vec_pretty(&before).unwrap()).unwrap();
    let operate = |operation: &str| {
        let output = Command::new(env!("CARGO_BIN_EXE_libra-governor"))
            .args([
                "adapter",
                operation,
                "claude_code",
                "--scope",
                "user",
                "--profile",
                "libra.claude-hooks.v1",
                "--json",
            ])
            .env_clear()
            .env("HOME", &home)
            .env("LIBRA_GOVERNOR_STATE_DIR", &state)
            .current_dir(temp.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{operation} refused: {:?}; {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout)
        );
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["verification_state"], "unverified");
        result["result"].clone()
    };
    let installed = operate("install");
    assert_eq!(installed["installed"], true);
    assert_eq!(installed["desired_enabled"], false);
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(&target).unwrap()).unwrap(),
        before
    );
    let enabled = operate("enable");
    assert_eq!(enabled["desired_enabled"], true);
    assert_eq!(enabled["local_connection"], "connected");
    let connected: Value = serde_json::from_slice(&fs::read(&target).unwrap()).unwrap();
    for key in [
        "model",
        "statusLine",
        "mcpServers",
        "enabledPlugins",
        "future",
    ] {
        assert_eq!(connected[key], before[key]);
    }
    assert_eq!(connected["hooks"]["Stop"][0], before["hooks"]["Stop"][0]);
    for event in ["UserPromptSubmit", "PostToolUse", "Stop"] {
        let groups = connected["hooks"][event].as_array().unwrap();
        assert_eq!(
            groups
                .iter()
                .flat_map(|group| group["hooks"].as_array().unwrap())
                .filter(|hook| hook["command"]
                    .as_str()
                    .is_some_and(|command| command.contains(" adapter-hook ")))
                .count(),
            1
        );
    }
    let disabled = operate("disable");
    assert_eq!(disabled["installed"], true);
    assert_eq!(disabled["desired_enabled"], false);
    assert_eq!(disabled["local_connection"], "disconnected");
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(&target).unwrap()).unwrap(),
        before
    );
    let removed = operate("uninstall");
    assert_eq!(removed["installed"], false);
    assert_eq!(removed["desired_enabled"], false);
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(&target).unwrap()).unwrap(),
        before
    );
    let catalog: Value =
        serde_json::from_slice(&fs::read(state.join("host-adapters/registry.json")).unwrap())
            .unwrap();
    assert!(catalog["installations"].as_object().unwrap().is_empty());
    assert_eq!(
        fs::read_dir(state.join("host-adapters/installations"))
            .unwrap()
            .count(),
        0
    );
    for path in ["host_id", "daemon.sock", "ledger.sqlite3"] {
        assert!(!state.join(path).exists());
    }
}

#[test]
fn lifecycle_dry_runs_leave_the_full_fixture_inventory_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let state = temp.path().join("state");
    fs::create_dir(&home).unwrap();

    let installed = run_profile(
        temp.path(),
        &home,
        &state,
        &profile_args(
            "install",
            "claude_code",
            "user",
            "libra.claude-hooks.v1",
            false,
        ),
    );
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stdout)
    );
    let before = inventory(temp.path());
    assert!(before
        .iter()
        .any(|entry| entry.0.to_string_lossy().contains("host-adapter-registry")));

    for operation in ["install", "enable", "disable", "uninstall"] {
        let output = run_profile(
            temp.path(),
            &home,
            &state,
            &profile_args(
                operation,
                "claude_code",
                "user",
                "libra.claude-hooks.v1",
                true,
            ),
        );
        assert!(
            output.status.success(),
            "dry-run {operation} refused: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert_eq!(json(&output)["result"]["dry_run"], true);
        let preview = &json(&output)["result"]["preview"];
        assert_eq!(
            preview["artifact_action"],
            if operation == "uninstall" {
                "remove"
            } else {
                "preserve"
            }
        );
        assert_eq!(
            preview["add_callbacks"],
            if operation == "enable" { 3 } else { 0 }
        );
        assert_eq!(preview["remove_callbacks"], 0);
        assert_eq!(
            inventory(temp.path()),
            before,
            "dry-run {operation} changed disk"
        );
    }
}

#[test]
fn repeated_enabled_and_disabled_lifecycle_operations_are_noops() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let state = temp.path().join("state");
    fs::create_dir(&home).unwrap();

    for operation in ["install", "enable"] {
        let output = run_profile(
            temp.path(),
            &home,
            &state,
            &profile_args(
                operation,
                "claude_code",
                "user",
                "libra.claude-hooks.v1",
                false,
            ),
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
    let enabled_snapshot = inventory(temp.path());
    for operation in ["install", "enable"] {
        let output = run_profile(
            temp.path(),
            &home,
            &state,
            &profile_args(
                operation,
                "claude_code",
                "user",
                "libra.claude-hooks.v1",
                false,
            ),
        );
        assert!(output.status.success());
        assert_eq!(json(&output)["result"]["effect"], "no_change");
        assert_eq!(inventory(temp.path()), enabled_snapshot);
    }

    let disabled = run_profile(
        temp.path(),
        &home,
        &state,
        &profile_args(
            "disable",
            "claude_code",
            "user",
            "libra.claude-hooks.v1",
            false,
        ),
    );
    assert!(disabled.status.success());
    let disabled_snapshot = inventory(temp.path());
    for _ in 0..2 {
        let repeated = run_profile(
            temp.path(),
            &home,
            &state,
            &profile_args(
                "disable",
                "claude_code",
                "user",
                "libra.claude-hooks.v1",
                false,
            ),
        );
        assert!(repeated.status.success());
        assert_eq!(json(&repeated)["result"]["effect"], "no_change");
        assert_eq!(inventory(temp.path()), disabled_snapshot);
    }
}

#[test]
fn unsupported_lifecycle_adapter_profile_and_scope_create_no_state() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let state = temp.path().join("state");
    fs::create_dir(&home).unwrap();
    let before = inventory(temp.path());

    for (adapter, scope, profile) in [
        ("unknown_adapter", "user", "libra.claude-hooks.v1"),
        ("claude_code", "user", "future.profile"),
        ("claude_code", "project", "libra.claude-hooks.v1"),
    ] {
        let output = run_profile(
            temp.path(),
            &home,
            &state,
            &profile_args("install", adapter, scope, profile, false),
        );
        assert!(!output.status.success());
        assert_eq!(inventory(temp.path()), before);
    }
    assert!(!state.exists());
}

#[test]
fn edited_duplicate_and_partial_callback_sets_refuse_without_rewriting_target() {
    for corruption in ["edited", "duplicate", "partial"] {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let state = temp.path().join("state");
        let target = home.join(".claude/settings.json");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(
            &target,
            br#"{"model":"foreign","hooks":{"Stop":[{"matcher":"Bash","hooks":[{"type":"command","command":"foreign-hook"}]}]}}"#,
        )
        .unwrap();

        for operation in ["install", "enable"] {
            let output = run_profile(
                temp.path(),
                &home,
                &state,
                &profile_args(
                    operation,
                    "claude_code",
                    "user",
                    "libra.claude-hooks.v1",
                    false,
                ),
            );
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
        }

        let mut document: Value = serde_json::from_slice(&fs::read(&target).unwrap()).unwrap();
        match corruption {
            "edited" => {
                let members = document["hooks"]["UserPromptSubmit"][0]["hooks"]
                    .as_array_mut()
                    .unwrap();
                let command = members[0]["command"].as_str().unwrap().to_owned();
                members[0]["command"] = Value::String(format!("{command} --operator-edit"));
            }
            "duplicate" => {
                let members = document["hooks"]["UserPromptSubmit"][0]["hooks"]
                    .as_array_mut()
                    .unwrap();
                let duplicate = members[0].clone();
                members.push(duplicate);
            }
            "partial" => {
                document["hooks"]
                    .as_object_mut()
                    .unwrap()
                    .remove("UserPromptSubmit");
            }
            _ => unreachable!(),
        }
        fs::write(&target, serde_json::to_vec_pretty(&document).unwrap()).unwrap();
        let changed_target = fs::read(&target).unwrap();

        let output = run_profile(
            temp.path(),
            &home,
            &state,
            &profile_args(
                "disable",
                "claude_code",
                "user",
                "libra.claude-hooks.v1",
                false,
            ),
        );
        assert!(
            !output.status.success(),
            "{corruption} callback state unexpectedly disabled"
        );
        assert_eq!(fs::read(&target).unwrap(), changed_target, "{corruption}");
    }
}

#[test]
fn package_digest_drift_blocks_enable_but_not_disable_or_uninstall_cleanup() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let state = temp.path().join("state");
    let target = home.join(".claude/settings.json");
    fs::create_dir(&home).unwrap();

    for operation in ["install", "enable"] {
        let output = run_profile(
            temp.path(),
            &home,
            &state,
            &profile_args(
                operation,
                "claude_code",
                "user",
                "libra.claude-hooks.v1",
                false,
            ),
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
    let connected = fs::read(&target).unwrap();

    // Model a package upgrade at the same executable path by making the
    // persisted installation/artifact retain its prior byte identity. This
    // avoids replacing the Cargo-owned test executable.
    let registry_path = state.join("host-adapters/registry.json");
    let mut registry: Value = serde_json::from_slice(&fs::read(&registry_path).unwrap()).unwrap();
    let (binding, record) = registry["installations"]
        .as_object()
        .unwrap()
        .iter()
        .next()
        .unwrap();
    let binding = binding.clone();
    let installation = record["installation_id"].as_str().unwrap();
    let old_digest = record["binary"]["sha256"].as_str().unwrap().to_owned();
    let prior_digest = digest(b"previous packaged binary bytes");
    assert_ne!(old_digest, prior_digest);
    let artifact_path = state
        .join("host-adapters/installations")
        .join(format!("{installation}.json"));
    let artifact_text = fs::read_to_string(&artifact_path).unwrap();
    assert_eq!(artifact_text.matches(old_digest.as_str()).count(), 1);
    let prior_artifact = artifact_text.replacen(&old_digest, &prior_digest, 1);
    fs::write(&artifact_path, prior_artifact.as_bytes()).unwrap();
    let record = registry["installations"][binding.as_str()]
        .as_object_mut()
        .unwrap();
    let mut binary = record["binary"].as_object().unwrap().clone();
    binary.insert("sha256".into(), Value::String(prior_digest));
    record.insert("binary".into(), Value::Object(binary));
    record.insert(
        "artifact_sha256".into(),
        Value::String(digest(prior_artifact.as_bytes())),
    );
    fs::write(&registry_path, serde_json::to_vec(&registry).unwrap()).unwrap();

    let enable = run_profile(
        temp.path(),
        &home,
        &state,
        &profile_args(
            "enable",
            "claude_code",
            "user",
            "libra.claude-hooks.v1",
            false,
        ),
    );
    assert!(!enable.status.success());
    assert_eq!(fs::read(&target).unwrap(), connected);

    let disable = run_profile(
        temp.path(),
        &home,
        &state,
        &profile_args(
            "disable",
            "claude_code",
            "user",
            "libra.claude-hooks.v1",
            false,
        ),
    );
    assert!(
        disable.status.success(),
        "{}",
        String::from_utf8_lossy(&disable.stdout)
    );
    let disabled = fs::read(&target).unwrap();
    let disabled_doc: Value = serde_json::from_slice(&disabled).unwrap();
    assert!(!disabled_doc.to_string().contains("adapter-hook"));

    let uninstall = run_profile(
        temp.path(),
        &home,
        &state,
        &profile_args(
            "uninstall",
            "claude_code",
            "user",
            "libra.claude-hooks.v1",
            false,
        ),
    );
    assert!(
        uninstall.status.success(),
        "{}",
        String::from_utf8_lossy(&uninstall.stdout)
    );
    assert_eq!(fs::read(&target).unwrap(), disabled);
}

#[test]
fn passive_status_reports_current_integrity_without_reconciling_or_creating_files() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let state = temp.path().join("state");
    fs::create_dir(&home).unwrap();
    for operation in ["install", "enable"] {
        let output = run_profile(
            temp.path(),
            &home,
            &state,
            &profile_args(
                operation,
                "claude_code",
                "user",
                "libra.claude-hooks.v1",
                false,
            ),
        );
        assert!(output.status.success());
    }
    let before = inventory(temp.path());
    for operation in ["status", "doctor"] {
        let output = run_profile(
            temp.path(),
            &home,
            &state,
            &[
                "adapter".into(),
                operation.into(),
                "claude_code".into(),
                "--json".into(),
            ],
        );
        assert!(output.status.success());
        let value = json(&output);
        assert_eq!(
            value["result"]["local_installations"][0]["local_connection"],
            "connected"
        );
        assert_eq!(
            value["result"]["local_installations"][0]["integrity"],
            "verified"
        );
        assert_eq!(
            value["result"]["local_installations"][0]["native_verification"],
            "unverified"
        );
        assert_eq!(inventory(temp.path()), before);
    }
    let target = home.join(".claude/settings.json");
    fs::write(&target, b"{\"user\":\"removed callbacks\"}").unwrap();
    let before = inventory(temp.path());
    let output = run_profile(
        temp.path(),
        &home,
        &state,
        &[
            "adapter".into(),
            "status".into(),
            "claude_code".into(),
            "--json".into(),
        ],
    );
    assert!(output.status.success());
    assert_eq!(
        json(&output)["result"]["local_installations"][0]["local_connection"],
        "unknown"
    );
    assert_eq!(
        json(&output)["result"]["local_installations"][0]["reason"],
        "connection_changed"
    );
    assert_eq!(inventory(temp.path()), before);
    let output = run_profile(
        temp.path(),
        &home,
        &state,
        &profile_args(
            "enable",
            "claude_code",
            "user",
            "libra.claude-hooks.v1",
            true,
        ),
    );
    assert!(!output.status.success());
    assert_eq!(inventory(temp.path()), before);
}

#[test]
fn legacy_activation_and_removal_preserve_canonical_ownership() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let state = temp.path().join("state");
    fs::create_dir(&home).unwrap();
    for operation in ["install", "enable"] {
        let output = run_profile(
            temp.path(),
            &home,
            &state,
            &profile_args(
                operation,
                "claude_code",
                "user",
                "libra.claude-hooks.v1",
                false,
            ),
        );
        assert!(output.status.success());
    }
    let before = inventory(temp.path());
    let output = run_profile(temp.path(), &home, &state, &["install".into()]);
    assert!(!output.status.success());
    assert_eq!(inventory(temp.path()), before);
    assert!(String::from_utf8_lossy(&output.stderr).contains("canonical_installation_conflict"));
    let target = home.join(".claude/settings.json");
    let before_target: Value = serde_json::from_slice(&fs::read(&target).unwrap()).unwrap();
    let catalog = fs::read(state.join("host-adapters/registry.json")).unwrap();
    let output = run_profile(
        temp.path(),
        &home,
        &state,
        &["uninstall".into(), "--yes".into()],
    );
    // Existing purge guard refuses the canonical namespace; it cannot erase it.
    assert!(!output.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&fs::read(&target).unwrap()).unwrap(),
        before_target
    );
    assert_eq!(
        fs::read(state.join("host-adapters/registry.json")).unwrap(),
        catalog
    );
    assert_eq!(
        fs::read_dir(state.join("host-adapters/installations"))
            .unwrap()
            .count(),
        1
    );
}

#[test]
fn legacy_only_install_keeps_existing_root_permissions_and_is_not_a_canonical_install() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let state = temp.path().join("state");
    fs::create_dir(&home).unwrap();
    fs::create_dir(&state).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
    let output = run_profile(temp.path(), &home, &state, &["install".into()]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::metadata(&state).unwrap().mode() & 0o777, 0o755);
    assert!(state.join("install.json").exists());
    assert!(!state.join("host-adapters").exists());
    let settings: Value =
        serde_json::from_slice(&fs::read(home.join(".claude/settings.json")).unwrap()).unwrap();
    for event in ["UserPromptSubmit", "PostToolUse", "Stop"] {
        assert_eq!(settings["hooks"][event].as_array().unwrap().len(), 1);
    }
    let output = run_profile(
        temp.path(),
        &home,
        &state,
        &profile_args(
            "install",
            "claude_code",
            "user",
            "libra.claude-hooks.v1",
            false,
        ),
    );
    assert!(output.status.success());
    let before = inventory(temp.path());
    let output = run_profile(
        temp.path(),
        &home,
        &state,
        &profile_args(
            "enable",
            "claude_code",
            "user",
            "libra.claude-hooks.v1",
            false,
        ),
    );
    assert!(!output.status.success());
    assert_eq!(inventory(temp.path()), before);
}

fn run_installed_callback(
    home: &Path,
    state: &Path,
    binding: &str,
    installation: &str,
    slot: &str,
    raw: &[u8],
) -> Output {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_libra-governor"))
        .args([
            "adapter-hook",
            "--state-root",
            state.to_str().unwrap(),
            "--binding",
            binding,
            "--installation",
            installation,
            "--slot",
            slot,
        ])
        .env_clear()
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(raw).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("fixture callback did not terminate");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

#[test]
fn enabled_packaged_callbacks_reach_shared_legacy_consumers_but_reject_unbounded_input() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let state = temp.path().join("state");
    fs::create_dir(&home).unwrap();
    for operation in ["install", "enable"] {
        assert!(run_profile(
            temp.path(),
            &home,
            &state,
            &profile_args(
                operation,
                "claude_code",
                "user",
                "libra.claude-hooks.v1",
                false
            )
        )
        .status
        .success());
    }
    let registry: Value =
        serde_json::from_slice(&fs::read(state.join("host-adapters/registry.json")).unwrap())
            .unwrap();
    let (binding, record) = registry["installations"]
        .as_object()
        .unwrap()
        .iter()
        .next()
        .unwrap();
    let installation = record["installation_id"].as_str().unwrap();
    for (slot, expected_log) in [
        ("prompt_submit", "malformed hook payload"),
        ("tool_completed", "post-tool-use: malformed hook payload"),
        ("turn_completed", "stop: malformed hook payload"),
    ] {
        let before = inventory(temp.path());
        for rejected in [vec![b'x'; 1024 * 1024 + 1], vec![0xff]] {
            let output =
                run_installed_callback(&home, &state, binding, installation, slot, &rejected);
            assert!(output.status.success());
            assert!(output.stdout.is_empty());
            assert_eq!(
                output.stderr,
                b"libra-governor: installed adapter input refused\n"
            );
            assert_eq!(inventory(temp.path()), before);
        }
        let output = run_installed_callback(&home, &state, binding, installation, slot, b"{");
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        if slot == "prompt_submit" {
            assert!(String::from_utf8_lossy(&output.stdout)
                .contains("preflight skipped: malformed hook payload"));
        } else {
            assert!(output.stdout.is_empty());
        }
        let log = fs::read_to_string(state.join("daemon.log")).unwrap();
        assert!(log.contains(expected_log));
    }
    assert!(!state.join("host_id").exists());
    assert!(!state.join("daemon.sock").exists());
    assert!(!state.join("ledger.sqlite3").exists());
}

#[test]
fn enabled_callback_rejects_oversized_input_while_producer_stays_open() {
    use std::io::Write;

    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let state = temp.path().join("state");
    fs::create_dir(&home).unwrap();
    for operation in ["install", "enable"] {
        let output = run_profile(
            temp.path(),
            &home,
            &state,
            &profile_args(
                operation,
                "claude_code",
                "user",
                "libra.claude-hooks.v1",
                false,
            ),
        );
        assert!(output.status.success());
    }
    let registry: Value =
        serde_json::from_slice(&fs::read(state.join("host-adapters/registry.json")).unwrap())
            .unwrap();
    let (binding, record) = registry["installations"]
        .as_object()
        .unwrap()
        .iter()
        .next()
        .unwrap();
    let installation = record["installation_id"].as_str().unwrap();
    let before = inventory(temp.path());

    let mut child = Command::new(env!("CARGO_BIN_EXE_libra-governor"))
        .args([
            "adapter-hook",
            "--state-root",
            state.to_str().unwrap(),
            "--binding",
            binding,
            "--installation",
            installation,
            "--slot",
            "turn_completed",
        ])
        .env_clear()
        .env("HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut producer = child.stdin.take().unwrap();
    let mut oversized = vec![b'x'; 1024 * 1024 + 1];
    oversized[..28].copy_from_slice(b"PRIVATE_INPUT_CANARY_9c21XYZ");
    producer.write_all(&oversized).unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() >= deadline {
            drop(producer);
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("enabled callback did not reject oversized input while producer stayed open");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // The writer remains alive until after the process has exited. Closing it
    // before observing termination would make an unbounded read test vacuous.
    drop(producer);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        output.stderr,
        b"libra-governor: installed adapter input refused\n"
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("PRIVATE_INPUT_CANARY_9c21XYZ"));
    assert_eq!(inventory(temp.path()), before);
}

#[test]
fn enabled_stop_callback_sends_legacy_finalize_to_the_configured_local_socket() {
    use std::io::{BufRead, Write};
    use std::os::unix::net::UnixListener;

    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let state = temp.path().join("state");
    fs::create_dir(&home).unwrap();
    for operation in ["install", "enable"] {
        let output = run_profile(
            temp.path(),
            &home,
            &state,
            &profile_args(
                operation,
                "claude_code",
                "user",
                "libra.claude-hooks.v1",
                false,
            ),
        );
        assert!(output.status.success());
    }
    let registry: Value =
        serde_json::from_slice(&fs::read(state.join("host-adapters/registry.json")).unwrap())
            .unwrap();
    let (binding, record) = registry["installations"]
        .as_object()
        .unwrap()
        .iter()
        .next()
        .unwrap();
    let installation = record["installation_id"].as_str().unwrap();

    let listener = UnixListener::bind(state.join("daemon.sock")).unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "Stop callback did not connect");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("fixture socket accept failed: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut request_line = String::new();
        std::io::BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut request_line)
            .unwrap();
        let response = serde_json::json!({
            "protocol_version": libra_governor_protocol::PROTOCOL_VERSION,
            "response": { "kind": "finalize", "state": "no_active_task" },
        });
        writeln!(stream, "{response}").unwrap();
        serde_json::from_str::<Value>(&request_line).unwrap()
    });

    let input = br#"{"session_id":"stop-fixture-session","cwd":"/fixture/repo","model":"fixture-model","hook_event_name":"Stop","stop_hook_active":false}"#;
    let output = run_installed_callback(
        &home,
        &state,
        binding,
        installation,
        "turn_completed",
        input,
    );
    let envelope = server.join().unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert_eq!(
        envelope["protocol_version"],
        libra_governor_protocol::PROTOCOL_VERSION
    );
    let request = &envelope["request"];
    assert_eq!(request.as_object().unwrap().len(), 5);
    assert_eq!(request["kind"], "finalize");
    assert_eq!(request["session_id"], "stop-fixture-session");
    assert_eq!(request["model"], "fixture-model");
    assert_eq!(request["provider"], "claude-code");
    assert!(request["transcript_path"].is_null());
    for field in ["host_id", "task_id", "estimate", "native_verification"] {
        assert!(request.get(field).is_none());
    }
    assert!(!state.join("ledger.sqlite3").exists());
    let log = fs::read_to_string(state.join("daemon.log")).unwrap();
    assert!(log.contains("stop: no active task for this session, safe no-op"));
    assert!(!log.contains("native verified"));
    assert!(!log.contains("canonical attribution"));
}
