use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

fn context() -> (TempDir, PathBuf, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    (temp, state, home)
}

fn cli(state: &Path, home: &Path, args: &[&str]) -> Output {
    let mut command_args = vec!["adapter"];
    command_args.extend_from_slice(args);
    root_cli(state, home, &command_args)
}

fn root_cli(state: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_libra-governor"))
        .args(args)
        .env("LIBRA_GOVERNOR_STATE_DIR", state)
        .env("HOME", home)
        .env("CODEX_HOME", home.join(".codex"))
        .env("XDG_STATE_HOME", state.parent().unwrap().join("xdg"))
        .output()
        .unwrap()
}

fn root_cli_in(
    cwd: &Path,
    home: &Path,
    state_override: Option<&str>,
    xdg_state_home: Option<&str>,
    args: &[&str],
) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_libra-governor"));
    command
        .args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env("CODEX_HOME", home.join(".codex"));
    if let Some(path) = state_override {
        command.env("LIBRA_GOVERNOR_STATE_DIR", path);
    } else {
        command.env_remove("LIBRA_GOVERNOR_STATE_DIR");
    }
    if let Some(path) = xdg_state_home {
        command.env("XDG_STATE_HOME", path);
    } else {
        command.env_remove("XDG_STATE_HOME");
    }
    command.output().unwrap()
}

fn cli_default_home(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_libra-governor"))
        .args(["adapter"])
        .args(args)
        .env("HOME", home)
        .env("CODEX_HOME", home.join(".codex"))
        .env_remove("LIBRA_GOVERNOR_STATE_DIR")
        .env_remove("XDG_STATE_HOME")
        .output()
        .unwrap()
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
            let bytes = if file_type.is_file() {
                Some(fs::read(&child).unwrap())
            } else {
                None
            };
            entries.push(EntrySnapshot {
                path: child.strip_prefix(root).unwrap().to_owned(),
                mode: metadata.mode() & 0o7777,
                device: metadata.dev(),
                inode: metadata.ino(),
                bytes,
            });
            if file_type.is_dir() {
                collect(root, &child, entries);
            }
        }
    }
    let mut entries = Vec::new();
    collect(root, root, &mut entries);
    entries
}

fn json(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "stdout was not JSON: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn manifest(temp: &Path) -> (PathBuf, PathBuf) {
    let program = temp.join("adapter_payload.bin");
    fs::write(&program, b"synthetic passive adapter bytes").unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    let digest_hex = Sha256::digest(b"synthetic passive adapter bytes")
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let digest = format!("sha256:{digest_hex}");
    let value = serde_json::json!({
        "manifest_kind":"host-adapter",
        "manifest_version":1,
        "adapter_id":"fixture_adapter",
        "adapter_version":"1.2.3",
        "protocol_versions":[1],
        "contract_version_range":{"minimum":1,"maximum":1},
        "roles":["LifecycleSource"],
        "capabilities":["lifecycle.session"],
        "host_version_constraints":[{"provider":"fixture_host","minimum":null,"maximum":null}],
        "configuration_schema":{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{},"additionalProperties":false},
        "launch":{"executable":program,"argv":[]},
        "runtime_files":[{"path":program,"kind":"entrypoint","digest":digest}],
        "input_limits":{"max_bytes":65536},
        "needs":{"environment":[],"read_paths":[],"write_paths":[]}
    });
    let path = temp.join("manifest.json");
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    (path, program)
}

fn registry_path(state: &Path) -> PathBuf {
    state.join("host-adapters/registry.json")
}

fn rewrite_registry(state: &Path, update: impl FnOnce(&mut Value)) {
    let path = registry_path(state);
    let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    update(&mut value);
    fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
}

fn distinct_manifest(temp: &Path, id: &str) -> PathBuf {
    let (path, _) = manifest(temp);
    let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value["adapter_id"] = Value::String(id.to_owned());
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    path
}

fn populate_registry_to_128(state: &Path) {
    rewrite_registry(state, |registry| {
        let existing = registry["adapters"]["fixture_adapter"].clone();
        let original_manifest: Value =
            serde_json::from_str(existing["manifest_json"].as_str().unwrap()).unwrap();
        let mut adapters = serde_json::Map::new();
        for index in 0..128 {
            let id = if index == 0 {
                "fixture_adapter".to_owned()
            } else {
                format!("boundary_{index:03}")
            };
            let mut manifest = original_manifest.clone();
            manifest["adapter_id"] = Value::String(id.clone());
            let manifest_json = serde_json::to_string(&manifest).unwrap();
            let digest = format!(
                "sha256:{}",
                Sha256::digest(manifest_json.as_bytes())
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            );
            let mut record = existing.clone();
            record["manifest_json"] = Value::String(manifest_json);
            record["manifest_digest"] = Value::String(digest);
            record["registration_revision"] = Value::from(index + 1);
            adapters.insert(id, record);
        }
        registry["revision"] = Value::from(128);
        registry["adapters"] = Value::Object(adapters);
    });
}

#[test]
fn help_is_state_free_and_list_is_a_passive_empty_snapshot() {
    let (_temp, state, home) = context();
    let help = cli(&state, &home, &["--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("adapter register"));
    assert!(!state.exists());

    let listed = cli(&state, &home, &["list", "--json"]);
    assert!(listed.status.success());
    let value = json(&listed);
    assert_eq!(value["operation"], "list");
    assert_eq!(value["result"]["registry_initialized"], false);
    assert_eq!(value["result"]["adapters"].as_array().unwrap().len(), 2);
    assert!(!state.exists());
}

#[test]
fn dry_run_and_invalid_scope_do_not_initialize_registry() {
    let (temp, state, home) = context();
    let (path, _) = manifest(temp.path());
    let dry = cli(
        &state,
        &home,
        &["register", path.to_str().unwrap(), "--dry-run", "--json"],
    );
    assert!(dry.status.success());
    assert_eq!(json(&dry)["result"]["dry_run"], true);
    assert!(!state.exists());

    let invalid = cli(
        &state,
        &home,
        &[
            "register",
            path.to_str().unwrap(),
            "--scope",
            "user",
            "--json",
        ],
    );
    assert!(!invalid.status.success());
    assert!(!state.exists());
}

#[test]
fn register_restart_review_confirm_and_unregister_remain_separate() {
    let (temp, state, home) = context();
    let (manifest_path, program) = manifest(temp.path());
    let path_arg = manifest_path.to_str().unwrap();

    let registered = cli(&state, &home, &["register", path_arg, "--json"]);
    assert!(
        registered.status.success(),
        "{}",
        String::from_utf8_lossy(&registered.stderr)
    );
    assert_eq!(json(&registered)["result"]["registration_changed"], true);
    assert!(state.join("host-adapters/registry.json").is_file());

    let listed_after_restart = cli(&state, &home, &["list", "--json"]);
    assert!(listed_after_restart.status.success());
    assert_eq!(
        json(&listed_after_restart)["result"]["adapters"]
            .as_array()
            .unwrap()
            .len(),
        3
    );

    let inspected = cli(&state, &home, &["inspect", "fixture_adapter", "--json"]);
    assert!(inspected.status.success());
    assert_eq!(json(&inspected)["result"]["code_trust"], "not_reviewed");
    assert!(!String::from_utf8_lossy(&inspected.stdout).contains(path_arg));

    let reviewed = cli(
        &state,
        &home,
        &[
            "inspect",
            "fixture_adapter",
            "--review-code-trust",
            "--json",
        ],
    );
    assert!(
        reviewed.status.success(),
        "{}",
        String::from_utf8_lossy(&reviewed.stderr)
    );
    let reviewed_value = json(&reviewed);
    let digest = reviewed_value["result"]["confirmation_digest"]
        .as_str()
        .unwrap();
    assert_eq!(
        reviewed_value["result"]["implementation_digest"]
            .as_str()
            .unwrap()
            .len(),
        71
    );
    assert!(!String::from_utf8_lossy(&reviewed.stdout).contains(program.to_str().unwrap()));

    let confirmed = cli(
        &state,
        &home,
        &[
            "register",
            path_arg,
            "--confirm-code-digest",
            digest,
            "--json",
        ],
    );
    assert!(
        confirmed.status.success(),
        "{}",
        String::from_utf8_lossy(&confirmed.stderr)
    );
    assert_eq!(json(&confirmed)["result"]["registration_changed"], false);
    assert_eq!(
        json(&confirmed)["result"]["action"],
        "confirm_adapter_code_trust"
    );

    let trusted = cli(&state, &home, &["inspect", "fixture_adapter", "--json"]);
    assert!(trusted.status.success());
    assert_eq!(json(&trusted)["result"]["code_trust"], "recorded");

    let removed = cli(&state, &home, &["unregister", "fixture_adapter", "--json"]);
    assert!(removed.status.success());
    assert_eq!(json(&removed)["result"]["registration_changed"], true);
    let empty = cli(&state, &home, &["list", "--json"]);
    assert!(empty.status.success());
    assert_eq!(
        json(&empty)["result"]["adapters"].as_array().unwrap().len(),
        2
    );
}

#[test]
fn lifecycle_probe_and_unknown_adapter_are_truthful_refusals() {
    let (_temp, state, home) = context();
    for args in [
        vec!["install", "claude_code", "--json"],
        vec!["doctor", "claude_code", "--probe", "--json"],
        vec!["inspect", "future_adapter", "--json"],
    ] {
        let output = cli(&state, &home, &args);
        assert!(!output.status.success());
        let value = json(&output);
        assert!(matches!(value["outcome"].as_str(), Some("refused")));
        assert!(value["reasons"].as_array().unwrap().len() == 1);
    }
    assert!(!state.exists());
}

#[test]
fn registration_does_not_require_or_measure_runtime_files_until_explicit_review() {
    let (temp, state, home) = context();
    let (manifest_path, program) = manifest(temp.path());
    fs::remove_file(program).unwrap();
    let path_arg = manifest_path.to_str().unwrap();

    let registered = cli(&state, &home, &["register", path_arg, "--json"]);
    assert!(
        registered.status.success(),
        "{}",
        String::from_utf8_lossy(&registered.stderr)
    );

    let ordinary = cli(&state, &home, &["inspect", "fixture_adapter", "--json"]);
    assert!(ordinary.status.success());
    assert_eq!(json(&ordinary)["result"]["code_trust"], "not_reviewed");

    let reviewed = cli(
        &state,
        &home,
        &[
            "inspect",
            "fixture_adapter",
            "--review-code-trust",
            "--json",
        ],
    );
    assert!(!reviewed.status.success());
    assert_eq!(json(&reviewed)["reasons"][0], "identity_unavailable");
    assert!(!String::from_utf8_lossy(&reviewed.stdout).contains(path_arg));
}

#[test]
fn manifest_symlink_is_refused_without_creating_registry() {
    use std::os::unix::fs::symlink;

    let (temp, state, home) = context();
    let (manifest_path, _) = manifest(temp.path());
    let link = temp.path().join("manifest-link.json");
    symlink(&manifest_path, &link).unwrap();
    let output = cli(
        &state,
        &home,
        &["register", link.to_str().unwrap(), "--json"],
    );
    assert!(!output.status.success());
    assert_eq!(json(&output)["reasons"][0], "invalid_manifest");
    assert!(!state.exists());
}

#[test]
fn legacy_uninstall_refuses_to_delete_adapter_registry_or_ledger() {
    let (temp, state, home) = context();
    fs::create_dir(&state).unwrap();
    let ledger = state.join("ledger.sqlite3");
    fs::write(&ledger, b"legacy ledger sentinel").unwrap();
    let (manifest_path, _) = manifest(temp.path());
    let registered = cli(
        &state,
        &home,
        &["register", manifest_path.to_str().unwrap(), "--json"],
    );
    assert!(registered.status.success());
    let registry = state.join("host-adapters/registry.json");
    let registry_before = fs::read(&registry).unwrap();
    let ledger_before = fs::read(&ledger).unwrap();

    for args in [
        vec!["uninstall", "--yes"],
        vec!["uninstall", "--agent", "codex", "--yes"],
    ] {
        let output = root_cli(&state, &home, &args);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("state deletion refused"));
        assert_eq!(fs::read(&registry).unwrap(), registry_before);
        assert_eq!(fs::read(&ledger).unwrap(), ledger_before);
    }
}

#[test]
fn dynamic_manifest_text_is_safe_for_terminal_rendering() {
    let (temp, state, home) = context();
    let (manifest_path, _) = manifest(temp.path());
    let mut value: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    value["adapter_version"] = Value::String("1.2\u{202e}3".to_owned());
    fs::write(&manifest_path, serde_json::to_vec(&value).unwrap()).unwrap();

    let registered = cli(
        &state,
        &home,
        &["register", manifest_path.to_str().unwrap(), "--json"],
    );
    assert!(registered.status.success());
    let inspected = cli(&state, &home, &["inspect", "fixture_adapter"]);
    assert!(inspected.status.success());
    let text = String::from_utf8(inspected.stdout).unwrap();
    assert!(text.contains("\\u{202e}"));
    assert!(!text.contains('\u{202e}'));
    assert!(!text.contains(manifest_path.to_str().unwrap()));
}

#[test]
fn default_home_passive_dry_run_and_absent_unregister_preserve_inventory() {
    let (temp, _state, home) = context();
    let (manifest_path, _) = manifest(temp.path());
    let before = inventory(&home);

    let listed = cli_default_home(&home, &["list", "--json"]);
    assert!(listed.status.success());
    assert_eq!(json(&listed)["result"]["registry_initialized"], false);

    let dry_run = cli_default_home(
        &home,
        &[
            "register",
            manifest_path.to_str().unwrap(),
            "--dry-run",
            "--json",
        ],
    );
    assert!(dry_run.status.success());
    assert_eq!(json(&dry_run)["result"]["dry_run"], true);

    let absent = cli_default_home(
        &home,
        &["unregister", "missing_adapter", "--dry-run", "--json"],
    );
    assert!(!absent.status.success());
    assert_eq!(json(&absent)["reasons"][0], "unknown_adapter");
    assert_eq!(inventory(&home), before);
    assert!(!home.join(".local").exists());
}

#[test]
fn default_home_registration_creates_owned_chain_and_restart_preserves_it() {
    let (temp, _state, home) = context();
    let (manifest_path, _) = manifest(temp.path());
    let state = home.join(".local/state/libra-governor");
    let registry = state.join("host-adapters/registry.json");

    let registered = cli_default_home(
        &home,
        &["register", manifest_path.to_str().unwrap(), "--json"],
    );
    assert!(
        registered.status.success(),
        "{}",
        String::from_utf8_lossy(&registered.stderr)
    );
    assert!(registry.is_file());
    let registry_metadata = fs::metadata(&registry).unwrap();
    let host_adapters_metadata = fs::metadata(registry.parent().unwrap()).unwrap();
    let state_metadata = fs::metadata(&state).unwrap();
    let registry_bytes = fs::read(&registry).unwrap();
    assert_eq!(registry_metadata.mode() & 0o777, 0o600);
    assert_eq!(registry_metadata.uid(), rustix::process::getuid().as_raw());
    assert_eq!(registry_metadata.nlink(), 1);
    assert_eq!(host_adapters_metadata.mode() & 0o777, 0o700);
    assert_eq!(
        host_adapters_metadata.uid(),
        rustix::process::getuid().as_raw()
    );
    assert_eq!(state_metadata.uid(), rustix::process::getuid().as_raw());
    assert_eq!(state_metadata.mode() & 0o022, 0);

    let listed = cli_default_home(&home, &["list", "--json"]);
    assert!(listed.status.success());
    let inspected = cli_default_home(&home, &["inspect", "fixture_adapter", "--json"]);
    assert!(inspected.status.success());
    assert_eq!(
        json(&inspected)["result"]["manifest_digest"],
        json(&registered)["result"]["manifest_digest"]
    );

    let registry_after = fs::metadata(&registry).unwrap();
    let host_adapters_after = fs::metadata(registry.parent().unwrap()).unwrap();
    let state_after = fs::metadata(&state).unwrap();
    assert_eq!(fs::read(&registry).unwrap(), registry_bytes);
    assert_eq!(registry_after.ino(), registry_metadata.ino());
    assert_eq!(registry_after.mode(), registry_metadata.mode());
    assert_eq!(host_adapters_after.ino(), host_adapters_metadata.ino());
    assert_eq!(host_adapters_after.mode(), host_adapters_metadata.mode());
    assert_eq!(state_after.ino(), state_metadata.ino());
    assert_eq!(state_after.mode(), state_metadata.mode());
}

#[test]
fn passive_default_home_access_preserves_preexisting_local_directory() {
    let (temp, _state, home) = context();
    let local = home.join(".local");
    fs::create_dir(&local).unwrap();
    fs::set_permissions(&local, fs::Permissions::from_mode(0o755)).unwrap();
    let before = fs::metadata(&local).unwrap();
    let (manifest_path, _) = manifest(temp.path());

    let listed = cli_default_home(&home, &["list", "--json"]);
    assert!(listed.status.success());
    let dry_run = cli_default_home(
        &home,
        &[
            "register",
            manifest_path.to_str().unwrap(),
            "--dry-run",
            "--json",
        ],
    );
    assert!(dry_run.status.success());
    let absent = cli_default_home(
        &home,
        &["unregister", "missing_adapter", "--dry-run", "--json"],
    );
    assert!(!absent.status.success());

    let after = fs::metadata(&local).unwrap();
    assert_eq!(after.mode() & 0o777, 0o755);
    assert_eq!(after.ino(), before.ino());
    assert_eq!(inventory(&home).len(), 1);
    assert!(!home.join(".local/state").exists());
}

#[test]
fn duplicate_registration_refuses_and_future_contract_is_reported_without_activation() {
    let (temp, state, home) = context();
    let (manifest_path, _) = manifest(temp.path());
    let mut value: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    value["protocol_versions"] = serde_json::json!([2]);
    value["contract_version_range"] = serde_json::json!({"minimum":2,"maximum":2});
    fs::write(&manifest_path, serde_json::to_vec(&value).unwrap()).unwrap();
    let args = ["register", manifest_path.to_str().unwrap(), "--json"];
    assert!(cli(&state, &home, &args).status.success());
    let before = inventory(&state);
    let inspected = cli(&state, &home, &["inspect", "fixture_adapter", "--json"]);
    assert!(inspected.status.success());
    assert_eq!(json(&inspected)["result"]["contract_compatible"], false);
    assert_eq!(json(&inspected)["result"]["effective_support"], "unknown");
    for dry_run in [false, true] {
        let mut duplicate = args.to_vec();
        if dry_run {
            duplicate.push("--dry-run");
        }
        let output = cli(&state, &home, &duplicate);
        assert!(!output.status.success());
        assert_eq!(json(&output)["reasons"][0], "adapter_conflict");
        assert_eq!(inventory(&state), before);
    }
}

#[test]
fn reserved_builtin_registration_dry_run_refuses_without_state_creation() {
    let (temp, state, home) = context();
    let (manifest_path, _) = manifest(temp.path());
    let mut value: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    for id in ["claude_code", "codex"] {
        value["adapter_id"] = serde_json::json!(id);
        fs::write(&manifest_path, serde_json::to_vec(&value).unwrap()).unwrap();
        let output = cli(
            &state,
            &home,
            &[
                "register",
                manifest_path.to_str().unwrap(),
                "--json",
                "--dry-run",
            ],
        );
        assert!(!output.status.success());
        assert_eq!(json(&output)["reasons"][0], "operation_refused");
        assert!(!state.exists());
    }
}

#[test]
fn exhausted_revision_refuses_register_and_unregister_plans_and_apply_without_changes() {
    let (temp, state, home) = context();
    let (initial_manifest, _) = manifest(temp.path());
    let registered = cli(
        &state,
        &home,
        &["register", initial_manifest.to_str().unwrap(), "--json"],
    );
    assert!(registered.status.success());
    let baseline = cli(&state, &home, &["list", "--json"]);
    assert!(baseline.status.success());
    assert_eq!(
        json(&baseline)["result"]["adapters"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    let candidate = distinct_manifest(temp.path(), "revision_boundary");
    rewrite_registry(&state, |registry| {
        registry["revision"] = Value::from(9_007_199_254_740_991u64);
    });
    let boundary_list = cli(&state, &home, &["list", "--json"]);
    assert!(boundary_list.status.success());
    assert_eq!(
        json(&boundary_list)["result"]["adapters"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    let before = inventory(&state);

    for (operation, dry_run) in [
        (vec!["register", candidate.to_str().unwrap()], true),
        (vec!["register", candidate.to_str().unwrap()], false),
        (vec!["unregister", "fixture_adapter"], true),
        (vec!["unregister", "fixture_adapter"], false),
    ] {
        let mut args = operation;
        if dry_run {
            args.push("--dry-run");
        }
        args.push("--json");
        let output = cli(&state, &home, &args);
        assert!(!output.status.success());
        assert_eq!(json(&output)["outcome"], "refused");
        assert_eq!(inventory(&state), before);
    }
}

#[test]
fn full_registry_refuses_new_registration_but_allows_no_change_unregister_preview() {
    let (temp, state, home) = context();
    let (initial_manifest, _) = manifest(temp.path());
    let registered = cli(
        &state,
        &home,
        &["register", initial_manifest.to_str().unwrap(), "--json"],
    );
    assert!(registered.status.success());
    let baseline = cli(&state, &home, &["list", "--json"]);
    assert!(baseline.status.success());
    assert_eq!(
        json(&baseline)["result"]["adapters"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    populate_registry_to_128(&state);
    let full_list = cli(&state, &home, &["list", "--json"]);
    assert!(full_list.status.success());
    assert_eq!(
        json(&full_list)["result"]["adapters"]
            .as_array()
            .unwrap()
            .len(),
        130
    );
    let candidate = distinct_manifest(temp.path(), "one_beyond_registry_limit");
    let before = inventory(&state);

    for dry_run in [true, false] {
        let mut args = vec!["register", candidate.to_str().unwrap()];
        if dry_run {
            args.push("--dry-run");
        }
        args.push("--json");
        let output = cli(&state, &home, &args);
        assert!(!output.status.success());
        assert_eq!(json(&output)["outcome"], "refused");
        assert_eq!(inventory(&state), before);
    }

    let preview = cli(
        &state,
        &home,
        &["unregister", "fixture_adapter", "--dry-run", "--json"],
    );
    assert!(
        preview.status.success(),
        "{}",
        String::from_utf8_lossy(&preview.stderr)
    );
    assert_eq!(json(&preview)["result"]["dry_run"], true);
    assert_eq!(inventory(&state), before);
}

#[test]
fn confirmation_dry_run_observes_revision_exhaustion_and_existing_grant_noop() {
    let (temp, state, home) = context();
    let (manifest_path, _) = manifest(temp.path());
    let manifest_arg = manifest_path.to_str().unwrap();
    let registered = cli(&state, &home, &["register", manifest_arg, "--json"]);
    assert!(registered.status.success());
    rewrite_registry(&state, |registry| {
        registry["revision"] = Value::from(9_007_199_254_740_991u64);
    });
    let valid_max_list = cli(&state, &home, &["list", "--json"]);
    assert!(valid_max_list.status.success());
    assert_eq!(
        json(&valid_max_list)["result"]["adapters"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    let review = cli(
        &state,
        &home,
        &[
            "inspect",
            "fixture_adapter",
            "--review-code-trust",
            "--json",
        ],
    );
    assert!(review.status.success());
    let review_value = json(&review);
    let digest = review_value["result"]["confirmation_digest"]
        .as_str()
        .unwrap();
    let before = inventory(&state);
    let refused = cli(
        &state,
        &home,
        &[
            "register",
            manifest_arg,
            "--confirm-code-digest",
            digest,
            "--dry-run",
            "--json",
        ],
    );
    assert!(!refused.status.success());
    assert_eq!(json(&refused)["outcome"], "refused");
    assert_eq!(inventory(&state), before);

    let (temp, state, home) = context();
    let (manifest_path, _) = manifest(temp.path());
    let manifest_arg = manifest_path.to_str().unwrap();
    let registered = cli(&state, &home, &["register", manifest_arg, "--json"]);
    assert!(registered.status.success());
    let first_review = cli(
        &state,
        &home,
        &[
            "inspect",
            "fixture_adapter",
            "--review-code-trust",
            "--json",
        ],
    );
    assert!(first_review.status.success());
    let first_review_value = json(&first_review);
    let digest = first_review_value["result"]["confirmation_digest"]
        .as_str()
        .unwrap();
    let granted = cli(
        &state,
        &home,
        &[
            "register",
            manifest_arg,
            "--confirm-code-digest",
            digest,
            "--json",
        ],
    );
    assert!(granted.status.success());
    rewrite_registry(&state, |registry| {
        registry["revision"] = Value::from(9_007_199_254_740_991u64);
    });
    let valid_max_list = cli(&state, &home, &["list", "--json"]);
    assert!(valid_max_list.status.success());
    let current_review = cli(
        &state,
        &home,
        &[
            "inspect",
            "fixture_adapter",
            "--review-code-trust",
            "--json",
        ],
    );
    assert!(current_review.status.success());
    let current_review_value = json(&current_review);
    let digest = current_review_value["result"]["confirmation_digest"]
        .as_str()
        .unwrap();
    let before = inventory(&state);
    let noop_preview = cli(
        &state,
        &home,
        &[
            "register",
            manifest_arg,
            "--confirm-code-digest",
            digest,
            "--dry-run",
            "--json",
        ],
    );
    assert!(
        noop_preview.status.success(),
        "{}",
        String::from_utf8_lossy(&noop_preview.stderr)
    );
    assert_eq!(inventory(&state), before);
    let noop_apply = cli(
        &state,
        &home,
        &[
            "register",
            manifest_arg,
            "--confirm-code-digest",
            digest,
            "--json",
        ],
    );
    assert!(noop_apply.status.success());
    assert_eq!(inventory(&state), before);
}

#[test]
fn relative_state_override_resolves_from_invocation_directory_once() {
    let (temp, _state, home) = context();
    let cwd = temp.path().join("working");
    fs::create_dir(&cwd).unwrap();
    let (manifest_path, _) = manifest(temp.path());
    let target = temp.path().join("relative-state");
    let before = inventory(temp.path());
    let listed = root_cli_in(
        &cwd,
        &home,
        Some("../relative-state"),
        None,
        &["adapter", "list", "--json"],
    );
    assert!(listed.status.success());
    assert_eq!(inventory(temp.path()), before);
    assert!(!target.exists());

    let registered = root_cli_in(
        &cwd,
        &home,
        Some("../relative-state"),
        None,
        &[
            "adapter",
            "register",
            manifest_path.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(
        registered.status.success(),
        "{}",
        String::from_utf8_lossy(&registered.stderr)
    );
    assert!(registry_path(&target).is_file());
    assert!(!cwd.join("relative-state").exists());
}

#[test]
fn relative_xdg_state_home_is_resolved_from_invocation_directory() {
    let (temp, _state, home) = context();
    let cwd = temp.path().join("working");
    fs::create_dir(&cwd).unwrap();
    let (manifest_path, _) = manifest(temp.path());
    let target = temp.path().join("xdg/libra-governor");
    let before = inventory(temp.path());
    let listed = root_cli_in(
        &cwd,
        &home,
        None,
        Some("../xdg"),
        &["adapter", "list", "--json"],
    );
    assert!(listed.status.success());
    assert_eq!(inventory(temp.path()), before);
    assert!(!target.exists());

    let registered = root_cli_in(
        &cwd,
        &home,
        None,
        Some("../xdg"),
        &[
            "adapter",
            "register",
            manifest_path.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(
        registered.status.success(),
        "{}",
        String::from_utf8_lossy(&registered.stderr)
    );
    assert!(registry_path(&target).is_file());
    assert!(!cwd.join("xdg/libra-governor").exists());
}

#[cfg(unix)]
#[test]
fn relative_state_override_preserves_symlink_then_parent_resolution() {
    use std::os::unix::fs::symlink;

    let (temp, _state, home) = context();
    let real_nested = temp.path().join("real/nested");
    fs::create_dir_all(&real_nested).unwrap();
    symlink(&real_nested, temp.path().join("alias")).unwrap();
    let (manifest_path, _) = manifest(temp.path());
    let cwd = temp.path();
    let target = temp.path().join("real/selected-state");
    let lexical_target = temp.path().join("selected-state");
    let before = inventory(temp.path());
    let listed = root_cli_in(
        cwd,
        &home,
        Some("alias/../selected-state"),
        None,
        &["adapter", "list", "--json"],
    );
    assert!(listed.status.success());
    assert_eq!(inventory(temp.path()), before);
    assert!(!target.exists());
    assert!(!lexical_target.exists());

    let registered = root_cli_in(
        cwd,
        &home,
        Some("alias/../selected-state"),
        None,
        &[
            "adapter",
            "register",
            manifest_path.to_str().unwrap(),
            "--json",
        ],
    );
    assert!(
        registered.status.success(),
        "{}",
        String::from_utf8_lossy(&registered.stderr)
    );
    assert!(registry_path(&target).is_file());
    assert!(!lexical_target.exists());
}
