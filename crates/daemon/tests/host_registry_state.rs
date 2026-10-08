use std::fs;
use std::os::unix::fs::{symlink, FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Barrier, OnceLock};

use libra_governor_daemon::host_runtime::contract::{HostContract, ValidatedManifest};
use libra_governor_daemon::host_runtime::state::{
    delete_legacy_state, reservation_target, AdapterRegistry,
};
use libra_governor_daemon::host_runtime::{RegistryEffect, RegistryFailure};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const BASE: &str =
    include_str!("../../protocol/contracts/host-adapter/v1/fixtures/valid-manifest-synthetic.json");

fn contract() -> &'static HostContract {
    static CONTRACT: OnceLock<HostContract> = OnceLock::new();
    CONTRACT.get_or_init(|| HostContract::load().expect("pinned contract resources load"))
}

fn registry(temp: &TempDir) -> AdapterRegistry {
    AdapterRegistry::new(temp.path().join("state"), contract().clone())
}

fn manifest(id: &str) -> ValidatedManifest {
    let mut value: Value = serde_json::from_str(BASE).unwrap();
    value["adapter_id"] = json!(id);
    // Registration validates declared metadata but deliberately does not measure or execute it.
    contract()
        .validate_manifest(&serde_json::to_vec(&value).unwrap())
        .unwrap()
}

fn unavailable_manifest(temp: &TempDir, id: &str) -> ValidatedManifest {
    let missing = temp.path().join("not-created-adapter");
    let mut value: Value = serde_json::from_str(BASE).unwrap();
    value["adapter_id"] = json!(id);
    value["launch"] = json!({"executable":missing,"argv":["--stdio-json"]});
    value["runtime_files"] = json!([{
        "path":missing,
        "kind":"entrypoint",
        "digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    }]);
    contract()
        .validate_manifest(&serde_json::to_vec(&value).unwrap())
        .unwrap()
}

fn measured_manifest(id: &str, executable: &Path, bytes: &[u8]) -> ValidatedManifest {
    let mut value: Value = serde_json::from_str(BASE).unwrap();
    value["adapter_id"] = json!(id);
    value["launch"] = json!({"executable":executable,"argv":["literal-argument"]});
    let digest = Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    value["runtime_files"] = json!([{
        "path":executable,
        "kind":"entrypoint",
        "digest":format!("sha256:{digest}")
    }]);
    contract()
        .validate_manifest(&serde_json::to_vec(&value).unwrap())
        .unwrap()
}

fn registry_file(temp: &TempDir) -> PathBuf {
    temp.path().join("state/host-adapters/registry.json")
}

fn register(registry: &AdapterRegistry, adapter: ValidatedManifest) -> RegistryEffect {
    let snapshot = registry.read().unwrap();
    registry.register(adapter, snapshot.stamp()).unwrap()
}

#[test]
fn absent_passive_read_does_not_initialize_registry_or_reservation() {
    let temp = TempDir::new().unwrap();
    let state_root = temp.path().join("legacy-state");
    fs::create_dir(&state_root).unwrap();
    fs::set_permissions(&state_root, fs::Permissions::from_mode(0o755)).unwrap();
    let before = fs::read_dir(temp.path()).unwrap().count();
    let registry = AdapterRegistry::new(state_root.clone(), contract().clone());

    let snapshot = registry.read().unwrap();

    assert!(snapshot.document().is_none());
    assert!(!state_root.join("host-adapters").exists());
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), before);
    assert_eq!(
        fs::metadata(&state_root).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[test]
fn registration_is_passive_metadata_and_last_removal_keeps_generation() {
    let temp = TempDir::new().unwrap();
    let registry = registry(&temp);
    let adapter = unavailable_manifest(&temp, "metadata_only");
    assert!(!adapter
        .value()
        .pointer("/launch/executable")
        .and_then(Value::as_str)
        .map(Path::new)
        .unwrap()
        .exists());

    assert_eq!(
        register(&registry, adapter.clone()),
        RegistryEffect::AppliedVerified
    );
    let first = registry.read().unwrap();
    let document = first.document().unwrap();
    let generation = document.registry_id().to_owned();
    assert_eq!(document.revision(), 1);
    let record = document.adapters().get("metadata_only").unwrap();
    assert_eq!(record.manifest().raw(), adapter.raw());
    assert_eq!(record.manifest().digest(), adapter.digest());
    assert!(record.trust().is_none());
    assert_eq!(record.trust_revision(), 0);

    assert!(registry.register(adapter.clone(), first.stamp()).is_err());
    assert!(registry
        .register(manifest("claude_code"), first.stamp())
        .is_err());
    assert!(registry
        .unregister("not_registered", first.stamp())
        .is_err());
    assert!(registry.unregister("codex", first.stamp()).is_err());

    let restarted = AdapterRegistry::new(temp.path().join("state"), contract().clone());
    let restarted_snapshot = restarted.read().unwrap();
    assert_eq!(
        restarted_snapshot
            .document()
            .unwrap()
            .adapters()
            .get("metadata_only")
            .unwrap()
            .manifest()
            .raw(),
        adapter.raw()
    );
    assert_eq!(
        restarted
            .unregister("metadata_only", restarted_snapshot.stamp())
            .unwrap(),
        RegistryEffect::AppliedVerified
    );
    let empty = restarted.read().unwrap();
    let document = empty.document().unwrap();
    assert_eq!(document.registry_id(), generation);
    assert_eq!(document.revision(), 2);
    assert!(document.adapters().is_empty());
}

#[test]
fn stale_stamp_cannot_overwrite_a_newer_registration() {
    let temp = TempDir::new().unwrap();
    let registry = registry(&temp);
    let absent = registry.read().unwrap();
    register(&registry, manifest("first_adapter"));
    let current = registry.read().unwrap();
    let raw_before = fs::read(registry_file(&temp)).unwrap();

    assert!(registry
        .register(manifest("stale_adapter"), absent.stamp())
        .is_err());
    assert_eq!(fs::read(registry_file(&temp)).unwrap(), raw_before);
    assert_eq!(current.document().unwrap().revision(), 1);
    assert!(registry
        .read()
        .unwrap()
        .document()
        .unwrap()
        .adapters()
        .contains_key("first_adapter"));
}

#[test]
fn explicit_trust_review_confirmation_revocation_and_reregistration_are_generation_bound() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("state");
    let registry = AdapterRegistry::new(root.clone(), contract().clone());
    let executable = temp.path().join("inert-binary-fixture");
    let executable_bytes = b"not executed by registry tests";
    fs::write(&executable, executable_bytes).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();

    register(
        &registry,
        measured_manifest("reviewed_adapter", &executable, executable_bytes),
    );
    let snapshot = registry.read().unwrap();
    assert!(snapshot
        .document()
        .unwrap()
        .adapters()
        .get("reviewed_adapter")
        .unwrap()
        .trust()
        .is_none());

    let review = registry.review_trust("reviewed_adapter").unwrap();
    assert_eq!(review.identity_scope(), "declared_launch_and_runtime_files");
    assert!(!review.recorded_match());
    assert!(review.manifest_digest().starts_with("sha256:"));
    assert!(review.implementation_digest().starts_with("sha256:"));
    assert!(review.confirmation_digest().starts_with("sha256:"));
    assert_eq!(
        registry
            .confirm_trust(
                "reviewed_adapter",
                review.manifest_digest(),
                "sha256:0000000000000000000000000000000000000000000000000000000000000000",
                snapshot.stamp(),
            )
            .unwrap_err()
            .effect,
        RegistryEffect::NoChange
    );

    assert_eq!(
        registry
            .confirm_trust(
                "reviewed_adapter",
                review.manifest_digest(),
                review.confirmation_digest(),
                snapshot.stamp(),
            )
            .unwrap(),
        RegistryEffect::AppliedVerified
    );
    let trusted = registry.read().unwrap();
    let trust = trusted
        .document()
        .unwrap()
        .adapters()
        .get("reviewed_adapter")
        .unwrap()
        .trust()
        .unwrap();
    assert_eq!(
        trust.implementation_digest(),
        review.implementation_digest()
    );
    assert!(registry
        .review_trust("reviewed_adapter")
        .unwrap()
        .recorded_match());
    assert_eq!(
        registry
            .confirm_trust(
                "reviewed_adapter",
                review.manifest_digest(),
                review.confirmation_digest(),
                trusted.stamp(),
            )
            .unwrap(),
        RegistryEffect::NoChange
    );
    let still_trusted = registry.read().unwrap();
    assert_eq!(
        registry
            .clear_trust("reviewed_adapter", still_trusted.stamp())
            .unwrap(),
        RegistryEffect::AppliedVerified
    );
    let cleared = registry.read().unwrap();
    assert!(cleared
        .document()
        .unwrap()
        .adapters()
        .get("reviewed_adapter")
        .unwrap()
        .trust()
        .is_none());

    assert_eq!(
        registry
            .unregister("reviewed_adapter", cleared.stamp())
            .unwrap(),
        RegistryEffect::AppliedVerified
    );
    let empty = registry.read().unwrap();
    assert_eq!(empty.document().unwrap().revision(), 4);
    register(
        &registry,
        measured_manifest("reviewed_adapter", &executable, executable_bytes),
    );
    let reregistered = registry.read().unwrap();
    assert_eq!(reregistered.document().unwrap().revision(), 5);
    assert!(registry
        .confirm_trust(
            "reviewed_adapter",
            review.manifest_digest(),
            review.confirmation_digest(),
            reregistered.stamp(),
        )
        .is_err());
    assert!(reregistered
        .document()
        .unwrap()
        .adapters()
        .get("reviewed_adapter")
        .unwrap()
        .trust()
        .is_none());
}

#[test]
fn changed_code_cannot_be_confirmed_with_a_prechange_review() {
    let temp = TempDir::new().unwrap();
    let registry = registry(&temp);
    let executable = temp.path().join("inert-entrypoint");
    fs::write(&executable, b"first version").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    register(
        &registry,
        measured_manifest("drifted_adapter", &executable, b"first version"),
    );
    let registered = registry.read().unwrap();
    let review = registry.review_trust("drifted_adapter").unwrap();

    fs::write(&executable, b"changed version").unwrap();

    let failure = registry
        .confirm_trust(
            "drifted_adapter",
            review.manifest_digest(),
            review.confirmation_digest(),
            registered.stamp(),
        )
        .unwrap_err();
    assert_eq!(failure.stage, "identity");
    assert_eq!(failure.reason, "identity_digest_mismatch");
    assert_eq!(failure.effect, RegistryEffect::NoChange);
    let after = registry.read().unwrap();
    assert!(after
        .document()
        .unwrap()
        .adapters()
        .get("drifted_adapter")
        .unwrap()
        .trust()
        .is_none());
}

#[test]
fn unsafe_registry_file_kinds_fail_closed_without_changing_registry_bytes() {
    let temp = TempDir::new().unwrap();
    let registry = registry(&temp);
    register(&registry, manifest("safe_baseline"));
    let path = registry_file(&temp);
    let original = fs::read(&path).unwrap();

    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(registry.read().is_err());
    assert_eq!(fs::read(&path).unwrap(), original);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

    let hardlink = temp.path().join("registry-hardlink");
    fs::hard_link(&path, &hardlink).unwrap();
    assert!(registry.read().is_err());
    assert_eq!(fs::read(&path).unwrap(), original);
    fs::remove_file(hardlink).unwrap();

    let moved = temp.path().join("registry-moved");
    fs::rename(&path, &moved).unwrap();
    symlink(&moved, &path).unwrap();
    assert!(registry.read().is_err());
    assert_eq!(fs::read(&moved).unwrap(), original);
    fs::remove_file(&path).unwrap();
    fs::rename(&moved, &path).unwrap();

    let raw = fs::read(&path).unwrap();
    fs::remove_file(&path).unwrap();
    // This utility only creates an inert FIFO test fixture; no adapter is invoked.
    let status = Command::new("mkfifo")
        .arg("-m")
        .arg("600")
        .arg(&path)
        .status()
        .expect("mkfifo test utility is available");
    assert!(status.success());
    let read = registry.read();
    assert!(
        read.is_err(),
        "a FIFO registry must be rejected without waiting for a writer"
    );
    assert!(fs::symlink_metadata(&path).unwrap().file_type().is_fifo());
    fs::remove_file(&path).unwrap();
    fs::write(&path, raw).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(registry
        .read()
        .unwrap()
        .document()
        .unwrap()
        .adapters()
        .contains_key("safe_baseline"));
}

#[test]
fn registry_corruption_never_becomes_an_empty_successful_snapshot() {
    let temp = TempDir::new().unwrap();
    let registry = registry(&temp);
    register(&registry, manifest("corruption_baseline"));
    let path = registry_file(&temp);
    let original = fs::read(&path).unwrap();
    fs::write(&path, br#"{"schema_version":1,"adapters":{}}"#).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

    let error: RegistryFailure = registry.read().unwrap_err();

    assert_eq!(error.effect, RegistryEffect::NoChange);
    assert!(registry_file(&temp).exists());
    fs::write(&path, original).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(registry.read().unwrap().document().is_some());
}

#[test]
fn missing_parent_reads_refusals_and_legacy_delete_noop_create_nothing() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    fs::set_permissions(&home, fs::Permissions::from_mode(0o755)).unwrap();
    let root = home.join(".local/state/libra");
    let registry = AdapterRegistry::new(root.clone(), contract().clone());
    let before_entries = fs::read_dir(&home).unwrap().count();
    let absent = registry.read().unwrap();
    assert!(absent.document().is_none());

    assert!(registry
        .unregister("unknown_adapter", absent.stamp())
        .is_err());
    assert!(registry.review_trust("unknown_adapter").is_err());
    assert!(registry
        .confirm_trust(
            "unknown_adapter",
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            absent.stamp(),
        )
        .is_err());
    assert!(registry
        .clear_trust("unknown_adapter", absent.stamp())
        .is_err());
    delete_legacy_state(&root).unwrap();

    assert_eq!(fs::read_dir(&home).unwrap().count(), before_entries);
    assert!(!home.join(".local").exists());
    assert!(!root.exists());
    assert!(reservation_target(&root).is_err());
}

#[test]
fn first_registration_prepares_private_parent_chain_without_chmodding_legacy_anchor() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    fs::set_permissions(&home, fs::Permissions::from_mode(0o755)).unwrap();
    let root = home.join(".local/state/libra");
    let registry = AdapterRegistry::new(root.clone(), contract().clone());
    let adapter = manifest("first_parent_registration");
    let absent = registry.read().unwrap();
    assert!(absent.document().is_none());

    assert_eq!(
        registry.register(adapter.clone(), absent.stamp()).unwrap(),
        RegistryEffect::AppliedVerified
    );

    assert_eq!(
        fs::metadata(&home).unwrap().permissions().mode() & 0o777,
        0o755
    );
    for directory in [home.join(".local"), home.join(".local/state"), root.clone()] {
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700,
            "new owned state directory must be private: {}",
            directory.display()
        );
    }
    assert_eq!(
        fs::metadata(root.join("host-adapters"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let fresh = AdapterRegistry::new(root, contract().clone())
        .read()
        .unwrap();
    assert_eq!(
        fresh
            .document()
            .unwrap()
            .adapters()
            .get("first_parent_registration")
            .unwrap()
            .manifest()
            .raw(),
        adapter.raw()
    );
}

#[test]
fn concurrent_first_registration_with_a_shared_absent_stamp_keeps_one_record() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("missing/home/.local/state/libra");
    let schemas = contract().clone();
    let first_registry = AdapterRegistry::new(root.clone(), schemas.clone());
    let second_registry = AdapterRegistry::new(root.clone(), schemas);
    let absent = first_registry.read().unwrap();
    assert!(absent.document().is_none());
    let expected = absent.stamp().clone();
    let first_manifest = manifest("concurrent_first");
    let second_manifest = manifest("concurrent_second");
    let barrier = Arc::new(Barrier::new(2));

    let first_barrier = barrier.clone();
    let first_stamp = expected.clone();
    let first = std::thread::spawn(move || {
        first_barrier.wait();
        first_registry.register(first_manifest, &first_stamp)
    });
    let second_barrier = barrier.clone();
    let second = std::thread::spawn(move || {
        second_barrier.wait();
        second_registry.register(second_manifest, &expected)
    });
    let results = [first.join().unwrap(), second.join().unwrap()];

    assert_eq!(
        results
            .iter()
            .filter(|result| result.as_ref().ok() == Some(&RegistryEffect::AppliedVerified))
            .count(),
        1,
        "exactly one first registrant can commit from the shared absent generation"
    );
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
    let stale = results
        .iter()
        .find_map(|result| result.as_ref().err())
        .unwrap();
    assert_eq!(stale.stage, "registry");
    assert!(
        matches!(
            stale.reason,
            "registry changed" | "state_parent_changed" | "state namespace changed"
        ),
        "unexpected first-registration refusal: {}",
        stale.reason
    );
    assert_eq!(stale.effect, RegistryEffect::NoChange);
    let final_registry = AdapterRegistry::new(root.clone(), contract().clone());
    let final_snapshot = final_registry.read().unwrap();
    let document = final_snapshot.document().unwrap();
    assert_eq!(document.revision(), 1);
    assert_eq!(document.adapters().len(), 1);
    assert!(
        document.adapters().contains_key("concurrent_first")
            ^ document.adapters().contains_key("concurrent_second")
    );

    let target = reservation_target(&root).unwrap();
    let lock_path = libra_governor_daemon::write_lock::derive_lock_path(&target).unwrap();
    assert!(lock_path.is_file());
    assert_eq!(
        fs::read_dir(lock_path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path() == lock_path)
            .count(),
        1,
        "both first writers use the same outside-root reservation"
    );
}

#[test]
fn retained_passive_snapshots_do_not_retain_directory_descriptors() {
    let temp = TempDir::new().unwrap();
    let registry = registry(&temp);
    let snapshots: Vec<_> = (0..128)
        .map(|_| {
            registry
                .read()
                .expect("passive snapshot releases observation handles")
        })
        .collect();
    assert!(snapshots
        .iter()
        .all(|snapshot| snapshot.document().is_none()));
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
}

#[test]
fn confirmation_preview_at_counter_limit_distinguishes_new_grant_from_true_noop() {
    let temp = TempDir::new().unwrap();
    let registry = registry(&temp);
    let program = temp.path().join("inert-payload");
    let bytes = b"never-executed-payload";
    fs::write(&program, bytes).unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    let adapter = measured_manifest("measured", &program, bytes);
    register(&registry, adapter.clone());
    let review = registry.review_trust("measured").unwrap();
    let path = registry_file(&temp);
    let initial = fs::read(&path).unwrap();
    let mut value: Value = serde_json::from_slice(&initial).unwrap();
    value["revision"] = json!(9_007_199_254_740_991_u64);
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    let snapshot = registry.read().unwrap();
    let exhausted = fs::read(&path).unwrap();
    assert!(registry
        .check_trust_confirmation(&snapshot, &review)
        .is_err());
    assert!(registry
        .confirm_trust(
            "measured",
            adapter.digest(),
            review.confirmation_digest(),
            snapshot.stamp()
        )
        .is_err());
    assert_eq!(fs::read(&path).unwrap(), exhausted);

    fs::write(&path, initial).unwrap();
    registry
        .confirm_trust(
            "measured",
            adapter.digest(),
            review.confirmation_digest(),
            registry.read().unwrap().stamp(),
        )
        .unwrap();
    let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value["revision"] = json!(9_007_199_254_740_991_u64);
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    let snapshot = registry.read().unwrap();
    let granted = fs::read(&path).unwrap();
    let review = registry.review_trust("measured").unwrap();
    registry
        .check_trust_confirmation(&snapshot, &review)
        .unwrap();
    assert_eq!(
        registry
            .confirm_trust(
                "measured",
                adapter.digest(),
                review.confirmation_digest(),
                snapshot.stamp()
            )
            .unwrap(),
        RegistryEffect::NoChange
    );
    assert_eq!(fs::read(&path).unwrap(), granted);
}
