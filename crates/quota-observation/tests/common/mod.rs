//! Shared black-box conformance harness for `crates/quota-observation`
//! (HORO-1855, Phase A — tests that pass against the crate as it
//! exists today, per the accepted HORO-1853 design-v2 contract).
//!
//! `#![allow(dead_code)]`: every test binary in this directory compiles
//! this whole module, but each binary only uses a subset of its
//! helpers — `cargo clippy --all-targets -D warnings` would otherwise
//! fail a binary for not calling a helper some *other* binary needs.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use time::macros::datetime;
use time::OffsetDateTime;

use libra_governor_quota_observation::freshness::{self, BoundSnapshotResult, FreshnessError};
use libra_governor_quota_observation::identity::{self, IdentityError};
use libra_governor_quota_observation::registration::{
    Approval, CapacityCeiling, Consent, ExpectedConstraint, FixedEnvironment,
    ObservationConfidence, RegisteredContext, Registration, RuntimeFileDigest, Transport,
    TransportKind, TrustMode,
};
use libra_governor_quota_observation::runner::{self, FrameError, InvokeError};
use libra_governor_quota_observation::wire::{
    ErrorCode, Quantity, Scope, SnapshotOperation, SnapshotRequest, SnapshotResponse,
    WireValidationError,
};

/// Rust's default test harness runs every `#[test]` in one binary in
/// parallel by default, and every test in this directory spawns a real
/// child process (and some wait out real wall-clock deadlines, e.g.
/// `frame_then_hold`). Each test function that invokes the fictional
/// extension — directly or via [`observe`] — must hold this lock for
/// its duration, exactly as `crates/quota-observation/src/runner.rs`'s
/// own `SUBPROCESS_SERIAL` does (see commit `ee20154`). A test with
/// several subprocess-spawning threads of its own (the shared-pool
/// concurrency attack) takes the lock once, in the test function
/// itself, around all of its threads — never inside this module's
/// helpers, since `std::sync::Mutex` is not reentrant and `observe`
/// calling back into a lock its own caller is already holding would
/// deadlock.
pub static SUBPROCESS_SERIAL: Mutex<()> = Mutex::new(());

/// The fixed clock every test evaluates freshness against, matching
/// the design-v2 example payloads' own `observed_at`/`valid_until`
/// timestamps (`2027-01-18T12:00:0Xs`) so freshness results never
/// depend on wall-clock time.
pub fn fixed_now() -> OffsetDateTime {
    datetime!(2027-01-18 12:00:30 UTC)
}

static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A unique request id built only from `[A-Za-z0-9_.-]` — the
/// fictional extension's `sed` extraction (see
/// `fixtures/fictional-ext/quota-ext.sh`) depends on that charset.
pub fn unique_request_id(tag: &str) -> String {
    let counter = REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("r-{tag}-{counter}-{nanos}")
}

/// Approves `registration` by repeatedly calling the crate's own
/// public [`identity::verify_identity`] and copying the `measured`
/// digest out of whichever mismatch it reports back into the matching
/// `approval.*_sha256` field, until it returns `Ok(())` or four
/// rounds pass without converging. This is the only way an integration
/// test can approve a registration: `manifest_digest`/
/// `launch_tuple_digest`/`digest_file` are `pub(crate)` in
/// `src/identity.rs`, not visible outside the crate, by design (so
/// that recomputing a digest independently here could never
/// accidentally diverge from the crate's own canonicalization). The
/// loop order mirrors `verify_identity`'s own check order: executable
/// digest, then manifest digest, then launch-tuple digest.
///
/// Requires `registration.enabled == true` and
/// `registration.approval.approved == true` going in —
/// `verify_identity` checks `Disabled`/`NotApproved` before it
/// measures anything, so this loop cannot discover a digest mismatch
/// while either of those is the actual failure.
pub fn approve(mut registration: Registration) -> Registration {
    for _ in 0..4 {
        match identity::verify_identity(&registration) {
            Ok(()) => return registration,
            Err(IdentityError::ExecutableDigestMismatch { measured, .. }) => {
                registration.approval.executable_sha256 = measured;
            }
            Err(IdentityError::ManifestDigestMismatch { measured, .. }) => {
                registration.approval.manifest_sha256 = measured;
            }
            Err(IdentityError::LaunchTupleDigestMismatch { measured, .. }) => {
                registration.approval.launch_tuple_sha256 = measured;
            }
            Err(other) => panic!(
                "approve(): registration must be enabled+approved before approving, got {other:?}"
            ),
        }
    }
    panic!("approve(): did not converge after 4 rounds");
}

/// Builds a minimal, valid, approved [`Registration`] whose transport
/// is `fixtures/fictional-ext/quota-ext.sh` invoked as
/// `<script> <mode> <template_path> [<capture_path>]`, with exactly
/// one `expected[]` entry. Callers mutate the returned value (e.g. to
/// add more `expected[]` entries, change `account_fingerprint`, or
/// shorten `timeout_ms` for a `frame_then_hold` test) and must call
/// [`approve`] again after any change that affects the manifest or
/// launch tuple — which is every field except the three
/// `approval.*_sha256` values themselves.
pub fn sample_registration(
    script: &Path,
    mode: &str,
    template: &Path,
    capture: Option<&Path>,
    expected_id: &str,
    account_fingerprint: &str,
    pool_alias: &str,
) -> Registration {
    let mut args = vec![mode.to_string(), template.to_str().unwrap().to_string()];
    if let Some(capture) = capture {
        args.push(capture.to_str().unwrap().to_string());
    }
    let registration = Registration {
        registration_id: format!("reg-{expected_id}"),
        extension_id: "examplecorp.quota".into(),
        extension_version: "0.1.0".into(),
        enabled: true,
        transport: Transport {
            kind: TransportKind::OneshotStdio,
            executable: script.to_str().unwrap().to_string(),
            args,
            environment: FixedEnvironment {
                lang: "C.UTF-8".into(),
                lc_all: "C.UTF-8".into(),
            },
        },
        context: RegisteredContext {
            selector: "profile_a".into(),
            handle: "context_a".into(),
            principal: "principal_a".into(),
            authority: "authority_a".into(),
        },
        expected: vec![sample_expected(
            expected_id,
            account_fingerprint,
            pool_alias,
        )],
        binding_revision: "1".into(),
        binding_verified_at: "2027-01-18T11:59:00Z".into(),
        scope_configured_at: "2027-01-18T11:58:00Z".into(),
        timeout_ms: 10_000,
        trust_mode: TrustMode::ObservationOnly,
        observation_confidence: ObservationConfidence::Low,
        approval: Approval {
            approved: true,
            revision: "1".into(),
            executable_sha256: String::new(),
            manifest_sha256: String::new(),
            launch_tuple_sha256: String::new(),
            runtime_files: Vec::<RuntimeFileDigest>::new(),
            endpoint_owner: "ops-team@examplecorp.invalid".into(),
            consent: Consent {
                consented_by: "ops-team@examplecorp.invalid".into(),
                recorded_at_reference: "ticket:EXAMPLE-1".into(),
            },
        },
    };
    approve(registration)
}

pub fn sample_expected(
    id: &str,
    account_fingerprint: &str,
    pool_alias: &str,
) -> ExpectedConstraint {
    ExpectedConstraint {
        id: id.into(),
        scope: Scope::Principal {
            handle: "principal_a".into(),
        },
        metric: libra_governor_quota_observation::wire::Metric::TotalTokens,
        unit: libra_governor_quota_observation::wire::Unit::Tokens,
        window_uuid: "20000000-0000-4000-8000-000000000001".into(),
        provider: "examplecorp".into(),
        account_fingerprint: account_fingerprint.into(),
        pool_alias: pool_alias.into(),
        max_staleness_secs: 300,
        window_length_secs: 2_592_000,
        capacity_ceiling: Some(CapacityCeiling {
            value: Quantity::parse("5000000").unwrap(),
            unit: libra_governor_quota_observation::wire::Unit::Tokens,
        }),
    }
}

/// Copies `script` into `dir` (mode 0755) and returns its path. Every
/// test uses its own tempdir's copy rather than sharing one path
/// across tests, matching the design's "each test copies the script
/// into its own tempdir" instruction.
pub fn copy_script(dir: &Path) -> std::path::PathBuf {
    let source =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fictional-ext/quota-ext.sh");
    let dest = dir.join("quota-ext.sh");
    std::fs::copy(&source, &dest).expect("copy fictional extension script");
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&dest).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&dest, perms).unwrap();
    dest
}

/// Writes `contents` (a single JSON line) as a response template at
/// `dir/name` and returns its path.
pub fn write_template(dir: &Path, name: &str, contents: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, contents).unwrap();
    path
}

pub fn fixture_path(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/scenarios")
        .join(name)
}

/// Every distinct failure this harness can observe, preserving the
/// crate's own real error variants rather than inventing new
/// semantics. A variant appearing here only ever wraps a value the
/// crate itself produced.
#[derive(Debug)]
pub enum ObserveError {
    Invoke(InvokeError),
    Frame(FrameError),
    WireValidation(WireValidationError),
    Identifiers(FreshnessError),
    ExtensionError(ErrorCode),
    Binding(FreshnessError),
    Freshness(FreshnessError),
}

#[derive(Debug)]
pub struct ObserveOk {
    pub bound: BoundSnapshotResult,
    /// Per matched (bound, non-missing) constraint id -> `is_fresh`'s
    /// result, using that id's own registered `max_staleness_secs`.
    pub freshness: HashMap<String, bool>,
}

/// The full round trip: the [`SnapshotRequest`] this call sent, the
/// raw response frame bytes the extension returned (empty on an
/// `invoke` failure), and the typed outcome.
#[derive(Debug)]
pub struct Observed {
    pub request: SnapshotRequest,
    pub raw_response: Vec<u8>,
    pub result: Result<ObserveOk, ObserveError>,
}

/// Reference wiring for one `describe`-less snapshot round trip,
/// using *only* `crates/quota-observation`'s own public functions, in
/// the order the design mandates: build request -> validate ->
/// serialize -> invoke -> parse_single_frame -> validate(requested) ->
/// verify_response_identifiers -> (error short-circuit) ->
/// apply_account_binding -> is_fresh per bound constraint. This
/// function adds no comparison of its own beyond calling those
/// functions in that order and reporting exactly what they say — if
/// the crate has no check for something, that absence is what a
/// caller observes here, never silently treated as success.
pub fn observe(registration: &Registration, now: OffsetDateTime) -> Observed {
    let constraint_ids: Vec<String> = registration
        .expected
        .iter()
        .map(|entry| entry.id.clone())
        .collect();
    let max_age_seconds = freshness::compute_max_age_seconds(registration, &constraint_ids)
        .expect("test registrations always have a matching expected[] entry for every id");
    let request = SnapshotRequest {
        protocol_version: "libra.quota-observation/0.1".into(),
        request_id: unique_request_id("obs"),
        operation: SnapshotOperation::Snapshot,
        context_selector: registration.context.selector.clone(),
        constraint_ids,
        max_age_seconds,
        budget_ms: registration.timeout_ms,
    };
    request
        .validate()
        .expect("test requests always satisfy SnapshotRequest::validate");

    let body = serde_json::to_vec(&request).expect("SnapshotRequest is always Serialize");
    let mut frame = body;
    frame.push(b'\n');

    let cancel = AtomicBool::new(false);
    let raw = match runner::invoke(registration, &frame, &cancel) {
        Ok(raw) => raw,
        Err(error) => {
            return Observed {
                request,
                raw_response: Vec::new(),
                result: Err(ObserveError::Invoke(error)),
            };
        }
    };

    let response: SnapshotResponse = match runner::parse_single_frame(&raw) {
        Ok(response) => response,
        Err(error) => {
            return Observed {
                request,
                raw_response: raw,
                result: Err(ObserveError::Frame(error)),
            };
        }
    };

    if let Err(error) = response.validate(&request.constraint_ids) {
        return Observed {
            request,
            raw_response: raw,
            result: Err(ObserveError::WireValidation(error)),
        };
    }

    if let Err(error) = freshness::verify_response_identifiers(&request, &response, registration) {
        return Observed {
            request,
            raw_response: raw,
            result: Err(ObserveError::Identifiers(error)),
        };
    }

    if let Some(wire_error) = &response.error {
        return Observed {
            request,
            raw_response: raw,
            result: Err(ObserveError::ExtensionError(wire_error.code)),
        };
    }

    let result = response
        .result
        .as_ref()
        .expect("validate() guarantees exactly one of result/error is set");

    let bound =
        match freshness::apply_account_binding(result, &request.constraint_ids, registration) {
            Ok(bound) => bound,
            Err(error) => {
                return Observed {
                    request,
                    raw_response: raw,
                    result: Err(ObserveError::Binding(error)),
                };
            }
        };

    let max_staleness_by_id: HashMap<&str, u64> = registration
        .expected
        .iter()
        .map(|entry| (entry.id.as_str(), entry.max_staleness_secs))
        .collect();

    let mut freshness_by_id = HashMap::new();
    for constraint in &bound.constraints {
        let max_staleness = *max_staleness_by_id
            .get(constraint.id.as_str())
            .expect("apply_account_binding only returns constraints present in expected[]");
        match freshness::is_fresh(
            constraint,
            max_staleness,
            bound.context_valid_until.as_deref(),
            now,
            now,
        ) {
            Ok(fresh) => {
                freshness_by_id.insert(constraint.id.clone(), fresh);
            }
            Err(error) => {
                return Observed {
                    request,
                    raw_response: raw,
                    result: Err(ObserveError::Freshness(error)),
                };
            }
        }
    }

    Observed {
        request,
        raw_response: raw,
        result: Ok(ObserveOk {
            bound,
            freshness: freshness_by_id,
        }),
    }
}
