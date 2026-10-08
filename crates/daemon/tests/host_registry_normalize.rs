use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use libra_governor_daemon::host_binding::{BindingCandidate, HostBindingOutcome, RecordOnlyReason};
use libra_governor_daemon::host_runtime::contract::{HostContract, ValidatedManifest};
use libra_governor_daemon::host_runtime::dispatch::{
    Cancellation, DiagnosticDispatcher, DiagnosticScope, DiagnosticSelection,
    NativeNormalizationInput,
};
use libra_governor_daemon::host_runtime::state::AdapterRegistry;
use libra_governor_daemon::host_runtime::RegistryEffect;
use libra_governor_protocol::host_event::{HostEventSource, HostEventSourceKind};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

const BASE_MANIFEST: &str =
    include_str!("../../protocol/contracts/host-adapter/v1/fixtures/valid-manifest-synthetic.json");
const SNAPSHOT: &[u8] = include_bytes!(
    "../../protocol/contracts/host-adapter/v1/fixtures/valid-snapshot-codex-shape-only-unknown.json"
);
const DRIVER: &str = include_str!("fixtures/external_probe_driver.py");
const PYTHON: &str = "/usr/bin/python3";
const ADAPTER_ID: &str = "external_probe_fixture";
const OBSERVED_AT: &str = "2026-10-06T00:00:00Z";
const HOST_ID: &str = "synthetic-c2-host";

struct Fixture {
    _temp: TempDir,
    contract: HostContract,
    registry: AdapterRegistry,
    log: PathBuf,
    registry_file: PathBuf,
}

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

fn lines(path: &Path) -> Vec<String> {
    if !path.exists() {
        return Vec::new();
    }
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn state_inventory(root: &Path) -> BTreeMap<PathBuf, (u32, u64, Option<String>)> {
    fn visit(
        root: &Path,
        path: &Path,
        inventory: &mut BTreeMap<PathBuf, (u32, u64, Option<String>)>,
    ) {
        let metadata = fs::symlink_metadata(path).unwrap();
        assert!(!metadata.file_type().is_symlink());
        assert!(metadata.is_dir() || metadata.is_file());
        let content = metadata.is_file().then(|| digest(&fs::read(path).unwrap()));
        inventory.insert(
            path.strip_prefix(root).unwrap().to_owned(),
            (metadata.mode(), metadata.ino(), content),
        );
        if metadata.is_dir() {
            for entry in fs::read_dir(path).unwrap() {
                visit(root, &entry.unwrap().path(), inventory);
            }
        }
    }
    let mut inventory = BTreeMap::new();
    visit(root, root, &mut inventory);
    inventory
}

fn make_fixture(mode: &str, max_bytes: Option<usize>, trusted: bool, compatible: bool) -> Fixture {
    let temp = TempDir::new().unwrap();
    let contract = HostContract::load().unwrap();
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
    manifest["adapter_id"] = json!(ADAPTER_ID);
    if let Some(max_bytes) = max_bytes {
        manifest["input_limits"]["max_bytes"] = json!(max_bytes);
    }
    if !compatible {
        manifest["protocol_versions"] = json!([2]);
        manifest["contract_version_range"] = json!({"minimum":2,"maximum":3});
    }
    manifest["launch"] = json!({
        "executable": PYTHON,
        "argv": [path_string(&driver), path_string(&registry_file), path_string(&log), path_string(&snapshot), mode]
    });
    manifest["runtime_files"] = json!([
        {"path": PYTHON, "kind":"entrypoint", "digest":digest(&python_bytes)},
        {"path":path_string(&driver), "kind":"entrypoint", "digest":digest(&driver_bytes)},
        {"path":path_string(&snapshot), "kind":"dependency", "digest":digest(&snapshot_bytes)}
    ]);
    let manifest: ValidatedManifest = contract
        .validate_manifest(&serde_json::to_vec(&manifest).unwrap())
        .unwrap();

    let registry = AdapterRegistry::new(temp.path().join("state"), contract.clone());
    let state = registry.read().unwrap();
    assert_eq!(
        registry.register(manifest, state.stamp()).unwrap(),
        RegistryEffect::AppliedVerified
    );
    if trusted {
        let review = registry.review_trust(ADAPTER_ID).unwrap();
        let state = registry.read().unwrap();
        assert_eq!(
            registry
                .confirm_trust(
                    ADAPTER_ID,
                    review.manifest_digest(),
                    review.confirmation_digest(),
                    state.stamp(),
                )
                .unwrap(),
            RegistryEffect::AppliedVerified
        );
    }

    Fixture {
        _temp: temp,
        contract,
        registry,
        log,
        registry_file,
    }
}

fn source(with_optional_fields: bool) -> HostEventSource {
    HostEventSource {
        kind: HostEventSourceKind::Hook,
        native_event_name: "unrelated_provider.tool.completed".into(),
        native_schema_ref: with_optional_fields.then(|| "urn:synthetic:native:v1".into()),
        native_event_id: with_optional_fields.then(|| "native-source-event-id".into()),
        replay_key: with_optional_fields.then(|| "native-replay-key".into()),
    }
}

fn input() -> NativeNormalizationInput {
    NativeNormalizationInput {
        host_id: HOST_ID.into(),
        observed_at: OBSERVED_AT.into(),
        source: source(true),
        native_payload: br#"{"session_id":"payload-only-session","agent_id":"payload-only-agent","prompt":"synthetic only","cwd":"/not-read","model":"not-an-authority"}"#.to_vec(),
    }
}

fn selection(scope: DiagnosticScope) -> DiagnosticSelection {
    DiagnosticSelection {
        scope,
        configuration: b"{}".to_vec(),
    }
}

fn start_lines(log: &Path) -> Vec<String> {
    lines(&PathBuf::from(format!("{}.starts", log.display())))
}

impl Fixture {
    fn normalize(
        &self,
        id: &str,
        input: NativeNormalizationInput,
        scope: DiagnosticScope,
    ) -> Result<
        libra_governor_daemon::host_runtime::dispatch::CandidateNormalization,
        libra_governor_daemon::host_runtime::dispatch::DiagnosticFailure,
    > {
        let registry = AdapterRegistry::new(self._temp.path().join("state"), self.contract.clone());
        let mut dispatcher = DiagnosticDispatcher::new(registry, self.contract.clone());
        dispatcher.normalize_registered(id, selection(scope), input, &Cancellation::default())
    }

    fn grant(&self) -> String {
        self.registry.read().unwrap().document().unwrap().adapters()[ADAPTER_ID]
            .trust()
            .unwrap()
            .confirmation_digest()
            .to_owned()
    }
}

type NormalizeResult = Result<
    libra_governor_daemon::host_runtime::dispatch::CandidateNormalization,
    libra_governor_daemon::host_runtime::dispatch::DiagnosticFailure,
>;

fn expect_failure(
    result: NormalizeResult,
) -> libra_governor_daemon::host_runtime::dispatch::DiagnosticFailure {
    match result {
        Ok(_) => panic!("expected normalization refusal"),
        Err(failure) => failure,
    }
}

#[test]
fn registered_trusted_unknown_adapter_normalizes_and_binds_only_an_unverified_candidate() {
    let fixture = make_fixture("normal", None, true, true);
    let state_before = state_inventory(&fixture._temp.path().join("state"));
    let before = fs::read(&fixture.registry_file).unwrap();
    let grant_before = fixture.grant();
    let expected_source = serde_json::to_value(source(true)).unwrap();
    let result = fixture
        .normalize(ADAPTER_ID, input(), DiagnosticScope::User)
        .unwrap();

    assert_eq!(result.adapter_id(), ADAPTER_ID);
    assert_eq!(result.verification_state(), "unverified");
    assert_eq!(result.events().len(), 1);
    assert_eq!(result.bindings().len(), 1);
    let event = &result.events()[0];
    assert_eq!(event.event_id(), "adapter-event-claim");
    assert_eq!(event.adapter_id(), ADAPTER_ID);
    assert_eq!(event.adapter_version(), "1.2.0");
    assert_eq!(event.host_version(), Some("0.160.0"));
    assert_eq!(event.capability_snapshot_id(), "synthetic-codex-a");
    assert_eq!(event.observed_at(), OBSERVED_AT);
    assert_eq!(event.identity().host_id(), HOST_ID);
    assert_eq!(event.identity().tool_provider(), "synthetic_host");
    assert_eq!(
        event.identity().provider_session_id(),
        Some("adapter-session-claim")
    );
    assert_eq!(event.identity().agent_id(), Some("adapter-agent-claim"));
    assert_eq!(event.identity().turn_id(), Some("adapter-turn-claim"));
    assert_ne!(
        event.identity().provider_session_id(),
        Some("payload-only-session")
    );
    assert_ne!(event.identity().agent_id(), Some("payload-only-agent"));
    assert_eq!(
        serde_json::to_value(event.source()).unwrap(),
        expected_source
    );
    assert_eq!(event.scope(), libra_governor_protocol::HostEventScope::Host);
    assert!(matches!(
        &result.bindings()[0],
        HostBindingOutcome::Candidate(BindingCandidate::CompletedTool { .. })
    ));

    assert_eq!(lines(&fixture.log), ["handshake", "probe", "normalize"]);
    assert_eq!(start_lines(&fixture.log), ["start", "start", "start"]);
    assert_eq!(
        fs::read_to_string(format!("{}.normalization-input", fixture.log.display())).unwrap(),
        OBSERVED_AT
    );
    assert_eq!(fs::read(&fixture.registry_file).unwrap(), before);
    assert_eq!(fixture.grant(), grant_before);
    assert_eq!(
        state_inventory(&fixture._temp.path().join("state")),
        state_before
    );
}

#[test]
fn identity_gaps_lifecycle_without_native_context_and_usage_remain_record_only() {
    for (mode, reason) in [
        (
            "identity_unknown",
            RecordOnlyReason::MissingAttributionIdentity,
        ),
        (
            "identity_missing",
            RecordOnlyReason::MissingAttributionIdentity,
        ),
        (
            "identity_child",
            RecordOnlyReason::MissingAttributionIdentity,
        ),
        (
            "lifecycle_turn_start",
            RecordOnlyReason::MissingNativeContext,
        ),
        ("lifecycle_turn_end", RecordOnlyReason::MissingNativeContext),
        ("usage", RecordOnlyReason::UnsupportedUsage),
    ] {
        let fixture = make_fixture(mode, None, true, true);
        let before = fs::read(&fixture.registry_file).unwrap();
        let result = fixture
            .normalize(ADAPTER_ID, input(), DiagnosticScope::User)
            .unwrap();
        assert_eq!(result.events().len(), 1, "{mode}");
        assert!(
            matches!(
                &result.bindings()[0],
                HostBindingOutcome::RecordOnly { reason: actual, .. } if *actual == reason
            ),
            "{mode}"
        );
        assert_eq!(
            lines(&fixture.log),
            ["handshake", "probe", "normalize"],
            "{mode}"
        );
        assert_eq!(
            start_lines(&fixture.log),
            ["start", "start", "start"],
            "{mode}"
        );
        assert_eq!(fs::read(&fixture.registry_file).unwrap(), before, "{mode}");
    }
}

#[test]
fn equivalent_fractional_utc_observation_and_absent_source_fields_are_preserved() {
    let fixture = make_fixture("equivalent_timestamp", None, true, true);
    let mut input = input();
    input.observed_at = "2026-10-06T00:00:00Z".into();
    input.source = source(false);
    let expected_source = serde_json::to_value(&input.source).unwrap();
    let result = fixture
        .normalize(ADAPTER_ID, input, DiagnosticScope::User)
        .unwrap();
    let supplied = OffsetDateTime::parse("2026-10-06T00:00:00Z", &Rfc3339).unwrap();
    assert_eq!(result.events()[0].identity().observed_at(), supplied);
    assert_eq!(result.events()[0].observed_at(), "2026-10-06T00:00:00.000Z");
    assert_eq!(
        serde_json::to_value(result.events()[0].source()).unwrap(),
        expected_source
    );
    assert_eq!(lines(&fixture.log), ["handshake", "probe", "normalize"]);
    assert_eq!(
        fs::read_to_string(format!("{}.normalization-input", fixture.log.display())).unwrap(),
        "2026-10-06T00:00:00Z"
    );
}

#[test]
fn an_event_cannot_add_a_source_field_that_was_absent_from_the_request() {
    let fixture = make_fixture("event_source_field_added", None, true, true);
    let mut projection = input();
    projection.source = source(false);
    let failure = expect_failure(fixture.normalize(ADAPTER_ID, projection, DiagnosticScope::User));
    assert_eq!(failure.stage, "protocol");
    assert_eq!(failure.reason, "normalized event correlation refused");
    assert!(failure.execution_attempted);
    assert_eq!(lines(&fixture.log), ["handshake", "probe", "normalize"]);
    assert_eq!(start_lines(&fixture.log), ["start", "start", "start"]);
}

#[test]
fn absent_candidate_host_version_remains_absent_on_the_correlated_event() {
    let fixture = make_fixture("snapshot_host_version_absent", None, true, true);
    let result = fixture
        .normalize(ADAPTER_ID, input(), DiagnosticScope::User)
        .unwrap();
    assert_eq!(result.events().len(), 1);
    assert_eq!(result.events()[0].host_version(), None);
    assert_eq!(
        result.events()[0].capability_snapshot_id(),
        "synthetic-codex-a"
    );
    assert_eq!(lines(&fixture.log), ["handshake", "probe", "normalize"]);
    assert_eq!(start_lines(&fixture.log), ["start", "start", "start"]);
}

#[test]
fn caller_projection_refusals_and_known_request_lower_bound_start_no_child() {
    let mut invalid_host = input();
    invalid_host.host_id.clear();
    let mut invalid_time = input();
    invalid_time.observed_at = "tomorrow".into();
    let mut invalid_source = input();
    invalid_source.source.native_event_name.clear();
    let mut duplicate_payload = input();
    duplicate_payload.native_payload = br#"{"duplicate":1,"duplicate":2}"#.to_vec();
    let mut nested_duplicate_payload = input();
    nested_duplicate_payload.native_payload =
        br#"{"nested":{"duplicate":1,"duplicate":1}}"#.to_vec();
    let mut malformed_payload = input();
    malformed_payload.native_payload = b"{".to_vec();
    let mut array_payload = input();
    array_payload.native_payload = b"[]".to_vec();
    let mut scalar_payload = input();
    scalar_payload.native_payload = b"17".to_vec();
    let mut excessive_payload = input();
    excessive_payload.native_payload = serde_json::to_vec(&json!({
        "filler": "x".repeat(1_048_500)
    }))
    .unwrap();

    for (label, projection) in [
        ("invalid_host", invalid_host),
        ("invalid_time", invalid_time),
        ("invalid_source", invalid_source),
        ("duplicate_payload", duplicate_payload),
        ("nested_duplicate_payload", nested_duplicate_payload),
        ("malformed_payload", malformed_payload),
        ("array_payload", array_payload),
        ("scalar_payload", scalar_payload),
        ("oversized_total_projection", excessive_payload),
    ] {
        let fixture = make_fixture("normal", None, true, true);
        let before = fs::read(&fixture.registry_file).unwrap();
        let state_before = state_inventory(&fixture._temp.path().join("state"));
        let failure =
            expect_failure(fixture.normalize(ADAPTER_ID, projection, DiagnosticScope::User));
        assert_eq!(failure.stage, "input", "{label}");
        assert_eq!(failure.reason, "normalization input refused", "{label}");
        assert!(!failure.execution_attempted, "{label}");
        assert!(lines(&fixture.log).is_empty(), "{label}");
        assert!(start_lines(&fixture.log).is_empty(), "{label}");
        assert_eq!(fs::read(&fixture.registry_file).unwrap(), before, "{label}");
        assert_eq!(
            state_inventory(&fixture._temp.path().join("state")),
            state_before,
            "{label}"
        );
    }

    let fixture = make_fixture("normal", Some(1024), true, true);
    let mut lower_bound = input();
    lower_bound.native_payload = serde_json::to_vec(&json!({"filler":"x".repeat(900)})).unwrap();
    let failure = expect_failure(fixture.normalize(ADAPTER_ID, lower_bound, DiagnosticScope::User));
    assert_eq!(failure.stage, "input");
    assert_eq!(failure.reason, "normalization request refused");
    assert!(!failure.execution_attempted);
    assert!(lines(&fixture.log).is_empty());
    assert!(start_lines(&fixture.log).is_empty());
}

#[test]
fn unregistered_untrusted_and_incompatible_adapters_refuse_before_children() {
    let cases = [
        (
            "unknown_id",
            true,
            true,
            "absent_adapter",
            "selection",
            "unknown adapter",
        ),
        (
            "untrusted",
            false,
            true,
            ADAPTER_ID,
            "trust",
            "explicit code trust required",
        ),
        (
            "incompatible",
            true,
            false,
            ADAPTER_ID,
            "selection",
            "adapter protocol incompatible",
        ),
    ];
    for (mode, trusted, compatible, id, stage, reason) in cases {
        let fixture = make_fixture(mode, None, trusted, compatible);
        let before = fs::read(&fixture.registry_file).unwrap();
        let failure = expect_failure(fixture.normalize(id, input(), DiagnosticScope::User));
        assert_eq!(failure.stage, stage, "{mode}");
        assert_eq!(failure.reason, reason, "{mode}");
        assert!(!failure.execution_attempted, "{mode}");
        assert!(lines(&fixture.log).is_empty(), "{mode}");
        assert!(start_lines(&fixture.log).is_empty(), "{mode}");
        assert_eq!(fs::read(&fixture.registry_file).unwrap(), before, "{mode}");
    }
}

#[test]
fn handshake_probe_and_actual_normalize_request_limits_have_truthful_child_counts() {
    let fixture = make_fixture("handshake_error", None, true, true);
    let failure = expect_failure(fixture.normalize(ADAPTER_ID, input(), DiagnosticScope::User));
    assert_eq!(failure.stage, "protocol");
    assert_eq!(failure.reason, "handshake refused");
    assert!(failure.execution_attempted);
    assert_eq!(lines(&fixture.log), ["handshake"]);
    assert_eq!(start_lines(&fixture.log), ["start"]);

    let fixture = make_fixture("candidate_adapter_mismatch", None, true, true);
    let failure = expect_failure(fixture.normalize(ADAPTER_ID, input(), DiagnosticScope::User));
    assert_eq!(failure.stage, "protocol");
    assert_eq!(failure.reason, "candidate identity or context refused");
    assert!(failure.execution_attempted);
    assert_eq!(lines(&fixture.log), ["handshake", "probe"]);
    assert_eq!(start_lines(&fixture.log), ["start", "start"]);

    let fixture = make_fixture("normal", Some(1024), true, true);
    let before = fs::read(&fixture.registry_file).unwrap();
    let grant = fixture.grant();
    let failure = expect_failure(fixture.normalize(ADAPTER_ID, input(), DiagnosticScope::User));
    assert_eq!(failure.stage, "input");
    assert_eq!(failure.reason, "normalization request refused");
    assert!(failure.execution_attempted);
    assert_eq!(lines(&fixture.log), ["handshake", "probe"]);
    assert_eq!(start_lines(&fixture.log), ["start", "start"]);
    assert_eq!(fs::read(&fixture.registry_file).unwrap(), before);
    assert_eq!(fixture.grant(), grant);
}

#[test]
fn returned_event_correlation_is_all_or_nothing_across_identity_and_snapshot_fields() {
    let cases = [
        "event_host_id_mismatch",
        "event_provider_mismatch",
        "event_adapter_mismatch",
        "event_version_mismatch",
        "event_snapshot_mismatch",
        "event_host_version_mismatch",
        "event_host_version_absent",
        "event_source_name_mismatch",
        "event_source_kind_mismatch",
        "event_source_field_added",
        "event_source_id_mismatch",
        "event_replay_key_mismatch",
        "event_scope_mismatch",
        "event_scope_unknown",
        "different_timestamp",
        "mixed_batch_mismatch",
    ];
    for mode in cases {
        let fixture = make_fixture(mode, None, true, true);
        let state_before = state_inventory(&fixture._temp.path().join("state"));
        let failure = expect_failure(fixture.normalize(ADAPTER_ID, input(), DiagnosticScope::User));
        assert_eq!(
            failure.stage,
            "protocol",
            "{mode}: {}; starts={}; operations={}",
            failure.reason,
            start_lines(&fixture.log).len(),
            lines(&fixture.log).len()
        );
        assert_eq!(
            failure.reason, "normalized event correlation refused",
            "{mode}"
        );
        assert!(failure.execution_attempted, "{mode}");
        assert_eq!(
            lines(&fixture.log),
            ["handshake", "probe", "normalize"],
            "{mode}"
        );
        assert_eq!(
            start_lines(&fixture.log),
            ["start", "start", "start"],
            "{mode}"
        );
        assert_eq!(
            state_inventory(&fixture._temp.path().join("state")),
            state_before,
            "{mode}"
        );
    }
}

#[test]
fn malformed_normalize_response_and_wrong_protocol_correlation_are_refused() {
    for (mode, reason) in [
        ("normalize_wrong_request_id", "response correlation refused"),
        ("normalize_wrong_version", "response correlation refused"),
        ("normalize_error", "normalization refused"),
        ("normalize_empty_events", "response correlation refused"),
        (
            "normalize_wrong_result_class",
            "response correlation refused",
        ),
        ("invalid_event_schema", "response correlation refused"),
        ("invalid_event_id", "response correlation refused"),
        ("invalid_event_identity", "response correlation refused"),
    ] {
        let fixture = make_fixture(mode, None, true, true);
        let failure = expect_failure(fixture.normalize(ADAPTER_ID, input(), DiagnosticScope::User));
        assert_eq!(failure.stage, "protocol", "{mode}");
        assert_eq!(failure.reason, reason, "{mode}");
        assert!(failure.execution_attempted, "{mode}");
        assert_eq!(
            lines(&fixture.log),
            ["handshake", "probe", "normalize"],
            "{mode}"
        );
        assert_eq!(
            start_lines(&fixture.log),
            ["start", "start", "start"],
            "{mode}"
        );
        assert!(!failure
            .to_string()
            .contains("PRIVATE_NORMALIZE_ERROR_CANARY"));
    }
}

#[test]
fn code_trust_registration_and_user_cwd_drift_discard_batches_before_or_after_normalize() {
    let cases = [
        (
            "mutate_script_probe",
            2,
            "trust",
            "current code verification failed",
            vec!["handshake", "probe"],
        ),
        (
            "revoke_trust_probe",
            2,
            "trust",
            "explicit code trust required",
            vec!["handshake", "probe"],
        ),
        (
            "reregister_self_probe",
            2,
            "trust",
            "registration or trust changed",
            vec!["handshake", "probe"],
        ),
        (
            "replace_user_cwd_probe",
            2,
            "context",
            "working directory changed",
            vec!["handshake", "probe"],
        ),
        (
            "mutate_script_normalize",
            3,
            "trust",
            "current code verification failed",
            vec!["handshake", "probe", "normalize"],
        ),
        (
            "revoke_trust_normalize",
            3,
            "trust",
            "explicit code trust required",
            vec!["handshake", "probe", "normalize"],
        ),
        (
            "reregister_self_normalize",
            3,
            "trust",
            "registration or trust changed",
            vec!["handshake", "probe", "normalize"],
        ),
        (
            "replace_user_cwd_normalize",
            3,
            "context",
            "working directory changed",
            vec!["handshake", "probe", "normalize"],
        ),
    ];
    for (mode, count, stage, reason, operations) in cases {
        let fixture = make_fixture(mode, None, true, true);
        let before = fs::read(&fixture.registry_file).unwrap();
        let failure = expect_failure(fixture.normalize(ADAPTER_ID, input(), DiagnosticScope::User));
        assert_eq!(
            failure.stage,
            stage,
            "{mode}: {}; starts={}; operations={}",
            failure.reason,
            start_lines(&fixture.log).len(),
            lines(&fixture.log).len()
        );
        assert_eq!(failure.reason, reason, "{mode}");
        assert!(failure.execution_attempted, "{mode}");
        assert_eq!(lines(&fixture.log), operations, "{mode}");
        assert_eq!(start_lines(&fixture.log).len(), count, "{mode}");
        if mode.starts_with("mutate_script") || mode.starts_with("replace_user_cwd") {
            assert_eq!(fs::read(&fixture.registry_file).unwrap(), before, "{mode}");
        }
    }
}
