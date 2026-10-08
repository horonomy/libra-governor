use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use libra_governor_daemon::host_runtime::config_lifecycle::{ConfigLifecycle, LocalInstallation};
use libra_governor_daemon::host_runtime::config_profile::ConfigIntent;
use libra_governor_daemon::host_runtime::contract::HostContract;
use libra_governor_daemon::host_runtime::dispatch::Cancellation;
use libra_governor_daemon::host_runtime::state::AdapterRegistry;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const ID: &str = "drifting_config_planner";
const BASE: &str =
    include_str!("../../protocol/contracts/host-adapter/v1/fixtures/valid-manifest-synthetic.json");
const DRIVER: &str = include_str!("fixtures/external_config_driver.py");

fn digest(bytes: &[u8]) -> String {
    format!(
        "sha256:{}",
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

struct Fixture {
    _temp: tempfile::TempDir,
    lifecycle: ConfigLifecycle,
    selection: LocalInstallation,
    target: PathBuf,
    before: Vec<u8>,
    inode: u64,
    log: PathBuf,
    registry: PathBuf,
    artifact: PathBuf,
}

fn fixture(component: &str, phase: &str) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let state = root.join("state");
    let target = root.join("home/.claude/settings.json");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(
        &target,
        b"{\"future\":true,\"mcpServers\":{\"fixture\":{\"command\":\"foreign\"}}}",
    )
    .unwrap();
    let binary = root.join("consumer");
    fs::copy("/bin/sh", &binary).unwrap();
    let selection = LocalInstallation {
        scope: "user".into(),
        profile: "libra.claude-hooks.v1".into(),
        target: target.clone(),
        binary,
    };
    let contract = HostContract::load().unwrap();
    let lifecycle = ConfigLifecycle::new(state.clone(), contract.clone());
    lifecycle.install("claude_code", &selection, false).unwrap();
    let registry_file = state.join("host-adapters/registry.json");
    let initial: Value = serde_json::from_slice(&fs::read(&registry_file).unwrap()).unwrap();
    let installation = initial["installations"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap()["installation_id"]
        .as_str()
        .unwrap();
    let artifact = state
        .join("host-adapters/installations")
        .join(format!("{installation}.json"));
    let script = root.join("planner.py");
    fs::write(&script, DRIVER).unwrap();
    let log = root.join("operations.log");
    let control = root.join("drift-control.json");
    let (changed, action) = match component {
        "target_bytes" => (target.clone(), "write"),
        "target_inode" => (target.clone(), "replace"),
        "target_parent" => (target.clone(), "replace_parent"),
        "catalog_inode" => (registry_file.clone(), "replace"),
        "grant" | "installation" => (registry_file.clone(), "write"),
        "artifact" => (artifact.clone(), "append"),
        "package" => (selection.binary.clone(), "append"),
        "code" => (script.clone(), "append"),
        _ => panic!("unknown fixture component"),
    };
    let mut control_value =
        json!({"target":changed,"action":action,"content":"{\"changed_by_planner\":true}"});
    fs::write(&control, serde_json::to_vec(&control_value).unwrap()).unwrap();
    let mut manifest: Value = serde_json::from_str(BASE).unwrap();
    manifest["adapter_id"] = json!(ID);
    manifest["roles"] = json!(["ConfigDriver"]);
    manifest["host_version_constraints"][0]["provider"] = json!("claude_code");
    manifest["launch"] = json!({"executable":"/usr/bin/python3","argv":[script,registry_file,log,format!("drift_{phase}"),control]});
    // The final grant-drift bytes depend on the actual catalog after registration.
    // The control is fixture input, not product data or execution authority.
    manifest["runtime_files"] = json!([
        {"path":"/usr/bin/python3","kind":"entrypoint","digest":digest(&fs::read("/usr/bin/python3").unwrap())},
        {"path":script,"kind":"entrypoint","digest":digest(&fs::read(&script).unwrap())}
    ]);
    let registry = AdapterRegistry::new(state, contract.clone());
    let manifest = contract
        .validate_manifest(&serde_json::to_vec(&manifest).unwrap())
        .unwrap();
    registry
        .register(manifest, registry.read().unwrap().stamp())
        .unwrap();
    let review = registry.review_trust(ID).unwrap();
    registry
        .confirm_trust(
            ID,
            review.manifest_digest(),
            review.confirmation_digest(),
            registry.read().unwrap().stamp(),
        )
        .unwrap();
    if matches!(component, "grant" | "installation") {
        let mut changed: Value =
            serde_json::from_slice(&fs::read(&registry_file).unwrap()).unwrap();
        if component == "grant" {
            changed["adapters"][ID]["trust"] = Value::Null;
        } else {
            changed["installations"] = json!({});
        }
        control_value["content"] = json!(serde_json::to_string(&changed).unwrap());
        fs::write(&control, serde_json::to_vec(&control_value).unwrap()).unwrap();
    }
    let before = fs::read(&target).unwrap();
    let inode = fs::metadata(&target).unwrap().ino();
    Fixture {
        _temp: temp,
        lifecycle,
        selection,
        target,
        before,
        inode,
        log,
        registry: registry_file,
        artifact,
    }
}

#[test]
fn real_planner_drift_invalidates_preview_without_restoring_changed_state() {
    for phase in ["handshake", "plan"] {
        for component in [
            "target_bytes",
            "target_inode",
            "target_parent",
            "catalog_inode",
            "grant",
            "installation",
            "artifact",
            "package",
            "code",
        ] {
            let fixture = fixture(component, phase);
            let registry_before = fs::read(&fixture.registry).unwrap();
            let artifact_before = fs::read(&fixture.artifact).unwrap();
            let result = fixture.lifecycle.preview_registered_plan(
                ID,
                &fixture.selection,
                ConfigIntent::Enable,
                false,
                &Cancellation::default(),
            );
            let failure = result.expect_err("changed observations must prevent preview");
            assert!(failure.execution_attempted, "{phase}/{component}");
            let expected = if phase == "handshake" {
                "handshake\n"
            } else {
                "handshake\nplan_config\n"
            };
            assert_eq!(
                fs::read_to_string(&fixture.log).unwrap(),
                expected,
                "{phase}/{component}: {}",
                failure.reason
            );
            match component {
                "target_bytes" => assert_eq!(
                    fs::read(&fixture.target).unwrap(),
                    b"{\"changed_by_planner\":true}"
                ),
                "target_inode" | "target_parent" => {
                    assert_eq!(fs::read(&fixture.target).unwrap(), fixture.before);
                    assert_ne!(fs::metadata(&fixture.target).unwrap().ino(), fixture.inode);
                }
                _ => {
                    assert_eq!(fs::read(&fixture.target).unwrap(), fixture.before);
                    assert_eq!(fs::metadata(&fixture.target).unwrap().ino(), fixture.inode);
                }
            }
            if matches!(component, "grant" | "installation") {
                assert_ne!(fs::read(&fixture.registry).unwrap(), registry_before);
            } else {
                assert_eq!(fs::read(&fixture.registry).unwrap(), registry_before);
            }
            if component == "artifact" {
                assert_ne!(fs::read(&fixture.artifact).unwrap(), artifact_before);
            } else {
                assert_eq!(fs::read(&fixture.artifact).unwrap(), artifact_before);
            }
        }
    }
}
