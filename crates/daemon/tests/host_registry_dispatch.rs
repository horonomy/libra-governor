use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use libra_governor_daemon::host_runtime::contract::{HostContract, ValidatedManifest};
use libra_governor_daemon::host_runtime::dispatch::{
    Cancellation, DiagnosticDispatcher, DiagnosticScope, DiagnosticSelection,
};
use libra_governor_daemon::host_runtime::state::AdapterRegistry;
use libra_governor_daemon::host_runtime::RegistryEffect;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const BASE_MANIFEST: &str =
    include_str!("../../protocol/contracts/host-adapter/v1/fixtures/valid-manifest-synthetic.json");
const SNAPSHOT: &[u8] = include_bytes!(
    "../../protocol/contracts/host-adapter/v1/fixtures/valid-snapshot-codex-shape-only-unknown.json"
);
const PYTHON: &str = "/usr/bin/python3";

const DRIVER: &str = include_str!("fixtures/external_probe_driver.py");

#[derive(Clone, Copy)]
enum ManifestVariant {
    Normal,
    NumericCompatible,
    RequiresSetting,
    UngrantedRead,
    FutureOnly,
    AmbiguousProviders,
}

struct Fixture {
    manifest: ValidatedManifest,
    log: PathBuf,
    script: PathBuf,
    registry_file: PathBuf,
}

fn contract() -> &'static HostContract {
    static CONTRACT: OnceLock<HostContract> = OnceLock::new();
    CONTRACT.get_or_init(|| HostContract::load().expect("pinned host contracts load"))
}

fn registry(temp: &TempDir) -> AdapterRegistry {
    AdapterRegistry::new(temp.path().join("state"), contract().clone())
}

fn registry_view(temp: &TempDir) -> AdapterRegistry {
    AdapterRegistry::new(temp.path().join("state"), contract().clone())
}

fn digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("sha256:{digest}")
}

fn path_string(path: &Path) -> String {
    path.to_str().expect("temporary paths are UTF-8").to_owned()
}

fn fixture(temp: &TempDir, mode: &str, variant: ManifestVariant) -> Fixture {
    let script = temp.path().join("inert_adapter_driver.py");
    let log = temp.path().join("operations.log");
    let registry_file = temp.path().join("state/host-adapters/registry.json");
    let snapshot = temp.path().join("candidate_snapshot.json");
    fs::write(&script, DRIVER).unwrap();
    fs::write(&snapshot, SNAPSHOT).unwrap();

    let python_bytes = fs::read(PYTHON).expect("selected Python interpreter exists");
    let script_bytes = fs::read(&script).unwrap();
    let snapshot_bytes = fs::read(&snapshot).unwrap();
    let mut manifest: Value = serde_json::from_str(BASE_MANIFEST).unwrap();
    manifest["adapter_id"] = json!("external_probe_fixture");
    manifest["launch"] = json!({
        "executable": PYTHON,
        "argv": [path_string(&script), path_string(&registry_file), path_string(&log), path_string(&snapshot), mode]
    });
    manifest["runtime_files"] = json!([
        {"path": PYTHON, "kind":"entrypoint", "digest":digest(&python_bytes)},
        {"path":path_string(&script), "kind":"entrypoint", "digest":digest(&script_bytes)},
        {"path":path_string(&snapshot), "kind":"dependency", "digest":digest(&snapshot_bytes)}
    ]);
    match variant {
        ManifestVariant::Normal => {}
        ManifestVariant::NumericCompatible => {
            // JSON Schema treats an integral decimal as an integer value.
            manifest["input_limits"]["max_bytes"] = json!(1024.0);
        }
        ManifestVariant::RequiresSetting => {
            manifest["configuration_schema"]["properties"] =
                json!({"required_setting":{"type":"string"}});
            manifest["configuration_schema"]["required"] = json!(["required_setting"]);
        }
        ManifestVariant::UngrantedRead => {
            manifest["needs"]["read_paths"] = json!(["/private/input"]);
        }
        ManifestVariant::FutureOnly => {
            manifest["protocol_versions"] = json!([2]);
            manifest["contract_version_range"] = json!({"minimum":2,"maximum":3});
        }
        ManifestVariant::AmbiguousProviders => {
            let mut providers = manifest["host_version_constraints"]
                .as_array()
                .unwrap()
                .clone();
            let mut second = providers[0].clone();
            second["provider"] = json!("other_fixture_host");
            providers.push(second);
            manifest["host_version_constraints"] = json!(providers);
        }
    }
    let manifest = contract()
        .validate_manifest(&serde_json::to_vec(&manifest).unwrap())
        .unwrap();
    Fixture {
        manifest,
        log,
        script,
        registry_file,
    }
}

fn register(registry: &AdapterRegistry, manifest: ValidatedManifest) {
    let snapshot = registry.read().unwrap();
    assert_eq!(
        registry.register(manifest, snapshot.stamp()).unwrap(),
        RegistryEffect::AppliedVerified
    );
}

fn explicitly_trust(registry: &AdapterRegistry, id: &str) {
    let review = registry.review_trust(id).unwrap();
    assert!(!review.recorded_match());
    let snapshot = registry.read().unwrap();
    assert_eq!(
        registry
            .confirm_trust(
                id,
                review.manifest_digest(),
                review.confirmation_digest(),
                snapshot.stamp(),
            )
            .unwrap(),
        RegistryEffect::AppliedVerified
    );
    assert!(registry.review_trust(id).unwrap().recorded_match());
}

fn selection() -> DiagnosticSelection {
    DiagnosticSelection {
        scope: DiagnosticScope::User,
        configuration: b"{}".to_vec(),
    }
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

fn start_log(path: &Path) -> Vec<String> {
    operation_log(&PathBuf::from(format!("{}.starts", path.display())))
}

#[test]
fn trusted_registered_unknown_adapter_runs_two_correlated_operations_without_registry_mutation() {
    let temp = TempDir::new().unwrap();
    let registry = registry(&temp);
    let fixture = fixture(&temp, "normal", ManifestVariant::NumericCompatible);
    register(&registry, fixture.manifest.clone());
    explicitly_trust(&registry, "external_probe_fixture");
    let before = fs::read(&fixture.registry_file).unwrap();
    let trusted = registry.read().unwrap().document().unwrap().adapters()["external_probe_fixture"]
        .trust()
        .unwrap()
        .confirmation_digest()
        .to_owned();

    let mut dispatcher = DiagnosticDispatcher::new(registry_view(&temp), contract().clone());
    let candidate = dispatcher
        .probe_registered(
            "external_probe_fixture",
            selection(),
            &Cancellation::default(),
        )
        .unwrap();

    assert_eq!(candidate.adapter_id(), "external_probe_fixture");
    assert_eq!(candidate.capability_count(), 1);
    assert_eq!(operation_log(&fixture.log), vec!["handshake", "probe"]);
    assert_eq!(start_log(&fixture.log), vec!["start", "start"]);
    assert_eq!(fs::read(&fixture.registry_file).unwrap(), before);
    let current = registry.read().unwrap();
    assert_eq!(
        current.document().unwrap().adapters()["external_probe_fixture"]
            .trust()
            .unwrap()
            .confirmation_digest(),
        trusted
    );
    assert_eq!(
        registry
            .unregister("external_probe_fixture", current.stamp())
            .unwrap(),
        RegistryEffect::AppliedVerified
    );
    assert!(registry
        .read()
        .unwrap()
        .document()
        .unwrap()
        .adapters()
        .is_empty());
}

#[test]
fn passive_and_refused_diagnostics_start_no_child() {
    let cases = [
        (
            "external_probe_fixture",
            "unregistered",
            None,
            None,
            "selection",
            "unknown adapter",
        ),
        (
            "external_probe_fixture",
            "untrusted",
            Some(ManifestVariant::Normal),
            None,
            "trust",
            "explicit code trust required",
        ),
        (
            "codex",
            "builtin",
            None,
            None,
            "selection",
            "builtin diagnostics unavailable",
        ),
        (
            "external_probe_fixture",
            "invalid_settings",
            Some(ManifestVariant::RequiresSetting),
            Some(true),
            "selection",
            "configuration refused",
        ),
        (
            "external_probe_fixture",
            "ungranted_needs",
            Some(ManifestVariant::UngrantedRead),
            Some(true),
            "selection",
            "declared permissions not granted",
        ),
        (
            "external_probe_fixture",
            "incompatible",
            Some(ManifestVariant::FutureOnly),
            Some(true),
            "selection",
            "adapter protocol incompatible",
        ),
        (
            "external_probe_fixture",
            "ambiguous_provider",
            Some(ManifestVariant::AmbiguousProviders),
            Some(true),
            "selection",
            "host provider ambiguous",
        ),
    ];
    for (id, label, variant, trust, stage, reason) in cases {
        let temp = TempDir::new().unwrap();
        let registry = registry(&temp);
        let fixture = fixture(&temp, label, variant.unwrap_or(ManifestVariant::Normal));
        if variant.is_some() {
            register(&registry, fixture.manifest.clone());
        }
        if trust == Some(true) {
            explicitly_trust(&registry, "external_probe_fixture");
        }
        let before = fs::read(&fixture.registry_file).ok();
        let mut dispatcher = DiagnosticDispatcher::new(registry_view(&temp), contract().clone());
        let result = dispatcher.probe_registered(id, selection(), &Cancellation::default());
        let failure = result.err().expect("refusal before child start");
        assert_eq!(failure.stage, stage, "{label}");
        assert_eq!(failure.reason, reason, "{label}");
        assert!(!failure.execution_attempted, "{label}");
        assert!(operation_log(&fixture.log).is_empty(), "{label}");
        assert!(start_log(&fixture.log).is_empty(), "{label}");
        assert_eq!(fs::read(&fixture.registry_file).ok(), before, "{label}");
    }
}

#[test]
fn handshake_protocol_failures_use_one_child_and_never_start_probe() {
    for (mode, reason) in [
        ("wrong_handshake_id", "response correlation refused"),
        ("wrong_handshake_version", "response correlation refused"),
        ("handshake_error", "handshake refused"),
    ] {
        let temp = TempDir::new().unwrap();
        let registry = registry(&temp);
        let fixture = fixture(&temp, mode, ManifestVariant::Normal);
        register(&registry, fixture.manifest.clone());
        explicitly_trust(&registry, "external_probe_fixture");
        let before = fs::read(&fixture.registry_file).unwrap();

        let mut dispatcher = DiagnosticDispatcher::new(registry_view(&temp), contract().clone());
        let failure = dispatcher
            .probe_registered(
                "external_probe_fixture",
                selection(),
                &Cancellation::default(),
            )
            .err()
            .expect("handshake protocol failure is refused");

        assert_eq!(failure.stage, "protocol");
        assert_eq!(failure.reason, reason);
        assert!(failure.execution_attempted);
        assert_eq!(operation_log(&fixture.log), ["handshake"]);
        assert_eq!(start_log(&fixture.log), ["start"]);
        assert_eq!(fs::read(&fixture.registry_file).unwrap(), before);
    }
}

#[test]
fn code_drift_refuses_before_spawn_and_retains_the_explicit_grant() {
    let temp = TempDir::new().unwrap();
    let registry = registry(&temp);
    let fixture = fixture(&temp, "normal", ManifestVariant::Normal);
    register(&registry, fixture.manifest.clone());
    explicitly_trust(&registry, "external_probe_fixture");
    let before = fs::read(&fixture.registry_file).unwrap();
    let grant = registry.read().unwrap().document().unwrap().adapters()["external_probe_fixture"]
        .trust()
        .unwrap()
        .confirmation_digest()
        .to_owned();
    fs::write(
        &fixture.script,
        format!("{DRIVER}\n# changed after confirmation\n"),
    )
    .unwrap();

    let mut dispatcher = DiagnosticDispatcher::new(registry_view(&temp), contract().clone());
    let failure = dispatcher
        .probe_registered(
            "external_probe_fixture",
            selection(),
            &Cancellation::default(),
        )
        .err()
        .expect("changed trusted code is refused");

    assert_eq!(failure.stage, "trust");
    assert!(!failure.execution_attempted);
    assert!(operation_log(&fixture.log).is_empty());
    assert!(start_log(&fixture.log).is_empty());
    assert_eq!(fs::read(&fixture.registry_file).unwrap(), before);
    assert_eq!(
        registry.read().unwrap().document().unwrap().adapters()["external_probe_fixture"]
            .trust()
            .unwrap()
            .confirmation_digest(),
        grant
    );
}

#[test]
fn malformed_or_excessive_output_never_reaches_probe_or_leaks_driver_diagnostics() {
    for (mode, stage, reason) in [
        ("invalid_json", "protocol", "response correlation refused"),
        (
            "double_document",
            "protocol",
            "response correlation refused",
        ),
        ("stdout_flood", "execution", "response limit exceeded"),
        ("stderr_flood", "execution", "stderr limit exceeded"),
        ("nonzero_exit", "execution", "adapter exited unsuccessfully"),
    ] {
        let temp = TempDir::new().unwrap();
        let registry = registry(&temp);
        let fixture = fixture(&temp, mode, ManifestVariant::Normal);
        register(&registry, fixture.manifest.clone());
        explicitly_trust(&registry, "external_probe_fixture");
        let before = fs::read(&fixture.registry_file).unwrap();
        let mut dispatcher = DiagnosticDispatcher::new(registry_view(&temp), contract().clone());
        let failure = dispatcher
            .probe_registered(
                "external_probe_fixture",
                selection(),
                &Cancellation::default(),
            )
            .err()
            .expect("invalid response must fail");
        assert_eq!(failure.stage, stage, "{mode}");
        assert_eq!(failure.reason, reason, "{mode}");
        assert!(failure.execution_attempted);
        assert_eq!(operation_log(&fixture.log), ["handshake"]);
        assert_eq!(start_log(&fixture.log), ["start"]);
        assert_eq!(fs::read(&fixture.registry_file).unwrap(), before);
        assert!(!failure.to_string().contains("PRIVATE_DRIVER_ERROR_CANARY"));
        assert!(!format!("{failure:?}").contains(temp.path().to_str().unwrap()));
    }
}

#[test]
fn code_changes_during_handshake_invalidate_candidate_without_revoking_recorded_grant() {
    let temp = TempDir::new().unwrap();
    let registry = registry(&temp);
    let fixture = fixture(&temp, "mutate_script", ManifestVariant::Normal);
    register(&registry, fixture.manifest.clone());
    explicitly_trust(&registry, "external_probe_fixture");
    let before = fs::read(&fixture.registry_file).unwrap();
    let mut dispatcher = DiagnosticDispatcher::new(registry_view(&temp), contract().clone());
    let failure = dispatcher
        .probe_registered(
            "external_probe_fixture",
            selection(),
            &Cancellation::default(),
        )
        .err()
        .expect("post-handshake drift must fail");
    assert_eq!(failure.stage, "trust");
    assert_eq!(failure.reason, "current code verification failed");
    assert!(failure.execution_attempted);
    assert_eq!(operation_log(&fixture.log), ["handshake"]);
    assert_eq!(start_log(&fixture.log), ["start"]);
    assert_eq!(fs::read(&fixture.registry_file).unwrap(), before);
    assert!(
        registry.read().unwrap().document().unwrap().adapters()["external_probe_fixture"]
            .trust()
            .is_some()
    );
    assert!(fs::read_to_string(&fixture.script)
        .unwrap()
        .ends_with("# fixture code changed after handshake\n"));
}

#[test]
fn trust_revocation_or_removal_during_handshake_prevents_the_next_child() {
    for (mode, stage, reason) in [
        ("revoke_trust", "trust", "explicit code trust required"),
        ("unregister_self", "selection", "unknown adapter"),
        ("reregister_self", "trust", "registration or trust changed"),
    ] {
        let temp = TempDir::new().unwrap();
        let registry = registry(&temp);
        let fixture = fixture(&temp, mode, ManifestVariant::Normal);
        register(&registry, fixture.manifest.clone());
        explicitly_trust(&registry, "external_probe_fixture");
        let mut dispatcher = DiagnosticDispatcher::new(registry_view(&temp), contract().clone());
        let failure = dispatcher
            .probe_registered(
                "external_probe_fixture",
                selection(),
                &Cancellation::default(),
            )
            .err()
            .expect("concurrent revocation must refuse");
        assert!(failure.execution_attempted);
        assert_eq!(failure.stage, stage, "{mode}");
        assert_eq!(failure.reason, reason, "{mode}");
        assert_eq!(operation_log(&fixture.log), ["handshake"]);
        assert_eq!(start_log(&fixture.log), ["start"]);
        let current = registry.read().unwrap();
        let record = current
            .document()
            .unwrap()
            .adapters()
            .get("external_probe_fixture");
        if mode == "revoke_trust" {
            assert!(record.unwrap().trust().is_none());
        } else {
            if mode == "unregister_self" {
                assert!(record.is_none());
            } else {
                assert!(record.unwrap().trust().is_some());
            }
        }
    }
}

#[test]
fn post_probe_code_or_registry_drift_refuses_candidate() {
    for (mode, stage, reason) in [
        (
            "mutate_script_probe",
            "trust",
            "current code verification failed",
        ),
        (
            "revoke_trust_probe",
            "trust",
            "explicit code trust required",
        ),
        ("unregister_self_probe", "selection", "unknown adapter"),
        (
            "reregister_self_probe",
            "trust",
            "registration or trust changed",
        ),
    ] {
        let temp = TempDir::new().unwrap();
        let registry = registry(&temp);
        let fixture = fixture(&temp, mode, ManifestVariant::Normal);
        register(&registry, fixture.manifest.clone());
        explicitly_trust(&registry, "external_probe_fixture");
        let mut dispatcher = DiagnosticDispatcher::new(registry_view(&temp), contract().clone());
        let failure = dispatcher
            .probe_registered(
                "external_probe_fixture",
                selection(),
                &Cancellation::default(),
            )
            .err()
            .expect("post-probe drift refuses candidate");
        assert_eq!(failure.stage, stage, "{mode}");
        assert_eq!(failure.reason, reason, "{mode}");
        assert!(failure.execution_attempted, "{mode}");
        assert_eq!(
            operation_log(&fixture.log),
            ["handshake", "probe"],
            "{mode}"
        );
        assert_eq!(start_log(&fixture.log), ["start", "start"], "{mode}");
    }
}

#[test]
fn user_working_directory_replacement_during_handshake_refuses_before_probe() {
    let temp = TempDir::new().unwrap();
    let registry = registry(&temp);
    let fixture = fixture(&temp, "replace_user_cwd", ManifestVariant::Normal);
    register(&registry, fixture.manifest.clone());
    explicitly_trust(&registry, "external_probe_fixture");
    let mut dispatcher = DiagnosticDispatcher::new(registry_view(&temp), contract().clone());
    let failure = dispatcher
        .probe_registered(
            "external_probe_fixture",
            selection(),
            &Cancellation::default(),
        )
        .err()
        .expect("replaced user work directory refuses");
    assert_eq!(failure.stage, "context");
    assert_eq!(failure.reason, "working directory changed");
    assert!(failure.execution_attempted);
    assert_eq!(operation_log(&fixture.log), ["handshake"]);
    assert_eq!(start_log(&fixture.log), ["start"]);
}

#[test]
fn candidate_identity_provider_and_context_mismatches_are_refused() {
    for mode in [
        "candidate_adapter_mismatch",
        "candidate_provider_mismatch",
        "candidate_context_mismatch",
    ] {
        let temp = TempDir::new().unwrap();
        let registry = registry(&temp);
        let fixture = fixture(&temp, mode, ManifestVariant::Normal);
        register(&registry, fixture.manifest.clone());
        explicitly_trust(&registry, "external_probe_fixture");
        let mut dispatcher = DiagnosticDispatcher::new(registry_view(&temp), contract().clone());
        let failure = dispatcher
            .probe_registered(
                "external_probe_fixture",
                selection(),
                &Cancellation::default(),
            )
            .err()
            .expect("candidate identity/context mismatch refuses");
        assert_eq!(failure.stage, "protocol", "{mode}");
        assert_eq!(
            failure.reason, "candidate identity or context refused",
            "{mode}"
        );
        assert!(failure.execution_attempted, "{mode}");
        assert_eq!(
            operation_log(&fixture.log),
            ["handshake", "probe"],
            "{mode}"
        );
        assert_eq!(start_log(&fixture.log), ["start", "start"], "{mode}");
    }
}

#[test]
fn unrelated_registry_revision_change_does_not_invalidate_matching_selected_facts() {
    let temp = TempDir::new().unwrap();
    let registry = registry(&temp);
    let fixture = fixture(&temp, "unrelated_update", ManifestVariant::Normal);
    register(&registry, fixture.manifest.clone());
    explicitly_trust(&registry, "external_probe_fixture");
    let before = registry.read().unwrap();
    let selected = before.document().unwrap().adapters()["external_probe_fixture"].clone();
    let revision = before.document().unwrap().revision();
    let mut dispatcher = DiagnosticDispatcher::new(registry_view(&temp), contract().clone());
    let candidate = dispatcher
        .probe_registered(
            "external_probe_fixture",
            selection(),
            &Cancellation::default(),
        )
        .unwrap();
    assert_eq!(candidate.adapter_id(), "external_probe_fixture");
    assert_eq!(operation_log(&fixture.log), ["handshake", "probe"]);
    assert_eq!(start_log(&fixture.log), ["start", "start"]);
    let after = registry.read().unwrap();
    let document = after.document().unwrap();
    assert_eq!(document.revision(), revision + 1);
    assert!(document.adapters().contains_key("unrelated_metadata"));
    let current = &document.adapters()["external_probe_fixture"];
    assert_eq!(current.manifest().raw(), selected.manifest().raw());
    assert_eq!(
        current.registration_revision(),
        selected.registration_revision()
    );
    assert_eq!(current.trust_revision(), selected.trust_revision());
    assert_eq!(
        current.trust().unwrap().confirmation_digest(),
        selected.trust().unwrap().confirmation_digest()
    );
}
