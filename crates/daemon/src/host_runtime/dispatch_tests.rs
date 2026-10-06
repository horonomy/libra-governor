use super::*;

use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use tempfile::TempDir;

const BASE_MANIFEST: &str = include_str!(
    "../../../protocol/contracts/host-adapter/v1/fixtures/valid-manifest-synthetic.json"
);
const SNAPSHOT: &[u8] = include_bytes!(
    "../../../protocol/contracts/host-adapter/v1/fixtures/valid-snapshot-codex-shape-only-unknown.json"
);
const DRIVER: &str = include_str!("../../tests/fixtures/external_probe_driver.py");
const PYTHON: &str = "/usr/bin/python3";

fn digest(bytes: &[u8]) -> String {
    let value = Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("sha256:{value}")
}

fn path_string(path: &Path) -> String {
    path.to_str().expect("temporary paths are UTF-8").to_owned()
}

fn operation_log(path: &Path) -> Vec<String> {
    if !path.exists() {
        return Vec::new();
    }
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn registered_fixture(
    temp: &TempDir,
    contract: &HostContract,
) -> (AdapterRegistry, PathBuf, PathBuf) {
    let driver = temp.path().join("external_probe_driver.py");
    let snapshot = temp.path().join("candidate_snapshot.json");
    let log = temp.path().join("driver-operations.log");
    let registry_file = temp.path().join("state/host-adapters/registry.json");
    fs::write(&driver, DRIVER).unwrap();
    fs::write(&snapshot, SNAPSHOT).unwrap();

    let python_bytes = fs::read(PYTHON).expect("selected Python interpreter exists");
    let driver_bytes = fs::read(&driver).unwrap();
    let snapshot_bytes = fs::read(&snapshot).unwrap();
    let mut manifest: Value = serde_json::from_str(BASE_MANIFEST).unwrap();
    manifest["adapter_id"] = json!("external_probe_fixture");
    manifest["launch"] = json!({
        "executable": PYTHON,
        "argv": [path_string(&driver), path_string(&registry_file), path_string(&log), path_string(&snapshot), "normal"]
    });
    manifest["runtime_files"] = json!([
        {"path": PYTHON, "kind": "entrypoint", "digest": digest(&python_bytes)},
        {"path": path_string(&driver), "kind": "entrypoint", "digest": digest(&driver_bytes)},
        {"path": path_string(&snapshot), "kind": "dependency", "digest": digest(&snapshot_bytes)}
    ]);
    let manifest = contract
        .validate_manifest(&serde_json::to_vec(&manifest).unwrap())
        .unwrap();

    let registry = AdapterRegistry::new(temp.path().join("state"), contract.clone());
    let snapshot = registry.read().unwrap();
    assert_eq!(
        registry.register(manifest, snapshot.stamp()).unwrap(),
        super::super::RegistryEffect::AppliedVerified
    );
    let review = registry.review_trust("external_probe_fixture").unwrap();
    let snapshot = registry.read().unwrap();
    assert_eq!(
        registry
            .confirm_trust(
                "external_probe_fixture",
                review.manifest_digest(),
                review.confirmation_digest(),
                snapshot.stamp(),
            )
            .unwrap(),
        super::super::RegistryEffect::AppliedVerified
    );
    (registry, log, registry_file)
}

#[test]
fn public_probe_final_cancellation_after_candidate_copy_refuses_return() {
    let temp = TempDir::new().unwrap();
    let contract = HostContract::load().unwrap();
    let (registry, log, registry_file) = registered_fixture(&temp, &contract);
    let registry_before = fs::read(&registry_file).unwrap();
    let grant_before = registry.read().unwrap().document().unwrap().adapters()
        ["external_probe_fixture"]
        .trust()
        .unwrap()
        .confirmation_digest()
        .to_owned();

    let cancellation = Cancellation::default();
    let hook_called = Rc::new(Cell::new(false));
    let hook_observation = Rc::clone(&hook_called);
    let cancellation_from_hook = cancellation.clone();
    let mut dispatcher = DiagnosticDispatcher::new(
        AdapterRegistry::new(temp.path().join("state"), contract.clone()),
        contract,
    );
    dispatcher.after_copy = Some(Box::new(move || {
        hook_observation.set(true);
        cancellation_from_hook.cancel();
    }));

    let result = dispatcher.probe_registered(
        "external_probe_fixture",
        DiagnosticSelection {
            scope: DiagnosticScope::User,
            configuration: b"{}".to_vec(),
        },
        &cancellation,
    );
    let failure = match result {
        Ok(_) => panic!("final cancellation must prevent returning the candidate"),
        Err(failure) => failure,
    };

    assert!(hook_called.get());
    assert_eq!(failure.stage, "execution");
    assert_eq!(failure.reason, "diagnostic cancelled");
    assert!(failure.execution_attempted);
    assert_eq!(operation_log(&log), ["handshake", "probe"]);
    assert_eq!(
        operation_log(&PathBuf::from(format!("{}.starts", log.display()))),
        ["start", "start"]
    );
    assert_eq!(fs::read(&registry_file).unwrap(), registry_before);
    assert_eq!(
        registry.read().unwrap().document().unwrap().adapters()["external_probe_fixture"]
            .trust()
            .unwrap()
            .confirmation_digest(),
        grant_before
    );
}

#[test]
fn public_normalize_final_cancellation_after_binding_refuses_entire_batch() {
    assert_normalize_final_refusal(false);
}

#[test]
fn public_normalize_final_expiry_after_binding_refuses_entire_batch() {
    assert_normalize_final_refusal(true);
}

fn assert_normalize_final_refusal(expire: bool) {
    let temp = TempDir::new().unwrap();
    let contract = HostContract::load().unwrap();
    let (registry, log, registry_file) = registered_fixture(&temp, &contract);
    let registry_before = fs::read(&registry_file).unwrap();
    let grant_before = registry.read().unwrap().document().unwrap().adapters()
        ["external_probe_fixture"]
        .trust()
        .unwrap()
        .confirmation_digest()
        .to_owned();
    let cancellation = Cancellation::default();
    let hook_called = Rc::new(Cell::new(false));
    let hook_observation = Rc::clone(&hook_called);
    let cancellation_from_hook = cancellation.clone();
    let mut dispatcher = DiagnosticDispatcher::new(
        AdapterRegistry::new(temp.path().join("state"), contract.clone()),
        contract,
    );
    dispatcher.after_copy = Some(Box::new(move || {
        hook_observation.set(true);
        if !expire {
            cancellation_from_hook.cancel();
        }
    }));
    dispatcher.expire_after_copy = expire;
    let result = dispatcher.normalize_registered(
        "external_probe_fixture",
        DiagnosticSelection {
            scope: DiagnosticScope::User,
            configuration: b"{}".to_vec(),
        },
        NativeNormalizationInput {
            host_id: "diagnostic-host".into(),
            observed_at: "2026-10-06T00:00:00Z".into(),
            source: serde_json::from_value(json!({
                "kind":"hook", "native_event_name":"SyntheticAfter"
            }))
            .unwrap(),
            native_payload: b"{}".to_vec(),
        },
        &cancellation,
    );
    let failure = match result {
        Ok(_) => panic!("final cancellation must prevent returning any bound candidates"),
        Err(failure) => failure,
    };
    assert!(
        hook_called.get(),
        "normalization reached final batch mapping"
    );
    assert_eq!(failure.stage, if expire { "context" } else { "execution" });
    assert_eq!(
        failure.reason,
        if expire {
            "diagnostic context expired"
        } else {
            "diagnostic cancelled"
        }
    );
    assert!(failure.execution_attempted);
    assert_eq!(operation_log(&log), ["handshake", "probe", "normalize"]);
    assert_eq!(
        operation_log(&PathBuf::from(format!("{}.starts", log.display()))),
        ["start", "start", "start"]
    );
    assert_eq!(fs::read(&registry_file).unwrap(), registry_before);
    assert_eq!(
        registry.read().unwrap().document().unwrap().adapters()["external_probe_fixture"]
            .trust()
            .unwrap()
            .confirmation_digest(),
        grant_before
    );
}
