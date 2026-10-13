//! Launch-tuple digest computation and pre-invocation re-measurement.
//!
//! HORO-1853 design-v2 requires that approval binds a SHA256 digest over
//! a canonical launch tuple (absolute executable path + digest, ordered
//! argv, sorted fixed environment map `{LANG,LC_ALL}` only, ordered
//! declared runtime-file paths+digests) plus a manifest digest, and that
//! every invocation re-measures this identity from disk before running
//! and refuses on any mismatch — never silently re-approves.
//!
//! This mirrors `crates/daemon/src/host_runtime/identity.rs`'s
//! re-measure-every-invocation *pattern* in spirit, deliberately without
//! importing it: that module is a different crate's trust boundary, and
//! per this codebase's ADR-0005 precedent a narrow duplicate is
//! preferred here over a cross-cutting shared dependency. One
//! consequence of not replicating its fd-walk-based TOCTOU defenses:
//! this module reads files with ordinary `std::fs::read`, which leaves
//! a residual TOCTOU window between measuring a file and the runner
//! later executing it (an attacker who can race a file replace between
//! those two reads is not defended against here). That window is judged
//! acceptable for this slice because the subject is a *local operator's
//! own registered extension*, not an untrusted adapter catalog entry;
//! closing it fully would mean reproducing `host_runtime::identity`'s
//! held-fd/open-then-stat machinery, which this slice's scope does not
//! call for — that machinery exists to defend a different, higher-risk
//! trust boundary (an externally registered adapter catalog), and
//! bringing the whole apparatus over here is a judgment call, not
//! something the ticket itself mandated either way.

use std::path::Path;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::registration::Registration;

/// Caps how much of any one file this module will hash, so a malicious
/// or misconfigured registration cannot force an unbounded read.
const MAX_DIGESTED_FILE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    #[error("registration is not enabled")]
    Disabled,
    #[error("registration.approval.approved is false")]
    NotApproved,
    #[error("failed to read {0:?}: {1}")]
    Io(std::path::PathBuf, String),
    #[error("{0:?} exceeds the {MAX_DIGESTED_FILE_BYTES}-byte digest limit")]
    FileTooLarge(std::path::PathBuf),
    #[error("executable digest mismatch: approval expects {expected}, measured {measured}")]
    ExecutableDigestMismatch { expected: String, measured: String },
    #[error("manifest digest mismatch: approval expects {expected}, measured {measured}")]
    ManifestDigestMismatch { expected: String, measured: String },
    #[error("launch tuple digest mismatch: approval expects {expected}, measured {measured}")]
    LaunchTupleDigestMismatch { expected: String, measured: String },
    #[error("runtime file {path:?} digest mismatch: declared {declared}, measured {measured}")]
    RuntimeFileDigestMismatch {
        path: String,
        declared: String,
        measured: String,
    },
}

fn digest_string(bytes: &[u8]) -> String {
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256:{hex}")
}

pub(crate) fn digest_file(path: &Path) -> Result<String, IdentityError> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| IdentityError::Io(path.to_owned(), error.to_string()))?;
    if metadata.len() > MAX_DIGESTED_FILE_BYTES {
        return Err(IdentityError::FileTooLarge(path.to_owned()));
    }
    let bytes = std::fs::read(path)
        .map_err(|error| IdentityError::Io(path.to_owned(), error.to_string()))?;
    Ok(digest_string(&Sha256::digest(&bytes)))
}

/// Canonicalizes a JSON value (recursively sorted object keys) and
/// returns `"sha256:<hex>"` over its serialized bytes. `serde_json`'s
/// default `Map` is a `BTreeMap`, already key-sorted, unless some crate
/// in the final build enables the `preserve_order` feature — Cargo
/// feature unification means that is a workspace-wide decision this
/// crate does not control, so this call is not optional defensiveness:
/// it is what keeps this digest identical regardless of what any other
/// workspace member turns on, matching `host_runtime::identity`'s own
/// pattern.
fn canonical_digest(mut value: Value) -> String {
    value.sort_all_objects();
    let bytes = serde_json::to_vec(&value).expect("Value::to_vec never fails for a Value");
    digest_string(&Sha256::digest(&bytes))
}

/// The manifest digest: canonical JSON of the registration with the
/// `approval` block removed (the approval record cannot bind over
/// itself) — so editing the registration after approval, including the
/// approval metadata's own sibling fields, invalidates trust.
pub(crate) fn manifest_digest(registration: &Registration) -> String {
    let mut value = serde_json::to_value(registration).expect("Registration is always Serialize");
    if let Some(object) = value.as_object_mut() {
        object.remove("approval");
    }
    canonical_digest(value)
}

/// The launch tuple digest: absolute executable path + its measured
/// digest, ordered argv, the sorted fixed environment map (`LANG`,
/// `LC_ALL` only — no other key is part of identity), and the declared
/// runtime files in their declared order with their measured digests.
pub(crate) fn launch_tuple_digest(
    registration: &Registration,
    executable_digest: &str,
    runtime_file_digests: &[(String, String)],
) -> String {
    let value = serde_json::json!({
        "executable": registration.transport.executable,
        "executable_digest": executable_digest,
        "argv": registration.transport.args,
        "environment": {
            "LANG": registration.transport.environment.lang,
            "LC_ALL": registration.transport.environment.lc_all,
        },
        "runtime_files": runtime_file_digests
            .iter()
            .map(|(path, digest)| serde_json::json!({"path": path, "digest": digest}))
            .collect::<Vec<_>>(),
    });
    canonical_digest(value)
}

/// Re-measures every digest this registration's approval binds, from
/// disk, right now — never from a cached value — and refuses on any
/// mismatch. Callers (see [`crate::runner`]) must call this
/// unconditionally before every invocation; there is no code path that
/// skips re-measurement for an already-trusted registration, by
/// construction (there is no "trusted" state cached anywhere in this
/// crate).
pub fn verify_identity(registration: &Registration) -> Result<(), IdentityError> {
    if !registration.enabled {
        return Err(IdentityError::Disabled);
    }
    if !registration.approval.approved {
        return Err(IdentityError::NotApproved);
    }

    let executable_path = Path::new(&registration.transport.executable);
    let executable_digest = digest_file(executable_path)?;
    if executable_digest != registration.approval.executable_sha256 {
        return Err(IdentityError::ExecutableDigestMismatch {
            expected: registration.approval.executable_sha256.clone(),
            measured: executable_digest,
        });
    }

    let manifest_measured = manifest_digest(registration);
    if manifest_measured != registration.approval.manifest_sha256 {
        return Err(IdentityError::ManifestDigestMismatch {
            expected: registration.approval.manifest_sha256.clone(),
            measured: manifest_measured,
        });
    }

    let mut runtime_file_digests = Vec::with_capacity(registration.approval.runtime_files.len());
    for declared in &registration.approval.runtime_files {
        let measured = digest_file(Path::new(&declared.path))?;
        if measured != declared.sha256 {
            return Err(IdentityError::RuntimeFileDigestMismatch {
                path: declared.path.clone(),
                declared: declared.sha256.clone(),
                measured,
            });
        }
        runtime_file_digests.push((declared.path.clone(), measured));
    }

    let launch_tuple_measured =
        launch_tuple_digest(registration, &executable_digest, &runtime_file_digests);
    if launch_tuple_measured != registration.approval.launch_tuple_sha256 {
        return Err(IdentityError::LaunchTupleDigestMismatch {
            expected: registration.approval.launch_tuple_sha256.clone(),
            measured: launch_tuple_measured,
        });
    }

    Ok(())
}

/// The digests an approval binds, freshly re-measured from disk right
/// now — the same values [`verify_identity`] re-derives on every
/// invocation, surfaced here so an operator-facing `approve` command can
/// measure them once, show them to the operator, and bind them into a
/// [`Registration`]'s [`crate::registration::Approval`] via [`approve`].
/// `executable_sha256`/`manifest_sha256`/`launch_tuple_sha256` mirror
/// `Approval`'s own fields of the same name. `runtime_file_digests` is
/// `(path, sha256)` pairs in exactly the declared order of the
/// registration's `approval.runtime_files` at measurement time —
/// [`approve`] uses both the order and the path to line each measured
/// digest back up with the declared entry it belongs to, rather than
/// trusting positional order alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeasuredIdentity {
    pub executable_sha256: String,
    pub manifest_sha256: String,
    pub launch_tuple_sha256: String,
    pub runtime_file_digests: Vec<(String, String)>,
}

/// Measures `registration`'s executable digest, manifest digest,
/// per-runtime-file digests, and launch-tuple digest fresh from disk, in
/// the same order [`verify_identity`] checks them, reusing its exact
/// private canonicalization ([`digest_file`], [`manifest_digest`],
/// [`launch_tuple_digest`]) rather than reimplementing it. Unlike
/// [`verify_identity`], this does **not** compare the result against
/// `registration.approval`'s existing (possibly blank) digest fields —
/// it has no notion of "mismatch", only "what is on disk right now".
/// That comparison is `verify_identity`'s job at invocation time; this
/// function's job is to produce the values an `approve` operation binds
/// in the first place.
pub fn measure_identity(registration: &Registration) -> Result<MeasuredIdentity, IdentityError> {
    let executable_digest = digest_file(Path::new(&registration.transport.executable))?;

    let mut runtime_file_digests = Vec::with_capacity(registration.approval.runtime_files.len());
    for declared in &registration.approval.runtime_files {
        let measured = digest_file(Path::new(&declared.path))?;
        runtime_file_digests.push((declared.path.clone(), measured));
    }

    let manifest_measured = manifest_digest(registration);
    let launch_tuple_measured =
        launch_tuple_digest(registration, &executable_digest, &runtime_file_digests);

    Ok(MeasuredIdentity {
        executable_sha256: executable_digest,
        manifest_sha256: manifest_measured,
        launch_tuple_sha256: launch_tuple_measured,
        runtime_file_digests,
    })
}

/// Failure modes of an operator-facing approval.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApprovalError {
    /// `approve` itself never touches disk — a caller measures first via
    /// [`measure_identity`] and passes the result in — so this variant
    /// exists to let that caller propagate a measurement failure through
    /// the same `Result` chain as the approval step itself, via `?`,
    /// rather than inventing a second error type for what is really one
    /// operator-facing operation.
    #[error("failed to measure identity before approval: {0}")]
    Measurement(#[from] IdentityError),
    /// `measured.runtime_file_digests` does not correspond, path-for-path
    /// and in order, to `registration.approval.runtime_files` — e.g. a
    /// [`MeasuredIdentity`] measured against a different registration, or
    /// a registration whose declared runtime files changed between
    /// measuring and approving. Caught here rather than silently
    /// approving against the wrong files.
    #[error(
        "measured runtime file digests do not match the registration's declared runtime files"
    )]
    RuntimeFilesMismatch,
}

/// Binds `measured`'s digests into `registration.approval`, setting
/// `approval.approved = true` and each declared runtime file's
/// `sha256`. Touches nothing else on `registration` — not `enabled`,
/// not any `expected[]` entry, not
/// `approval.revision`/`endpoint_owner`/`consent`. Callers are
/// responsible for measuring `measured` against this same registration
/// (see [`measure_identity`]); this function checks only that
/// `measured.runtime_file_digests` lines up path-for-path with
/// `registration.approval.runtime_files`, returning
/// [`ApprovalError::RuntimeFilesMismatch`] if not — it does not
/// re-measure anything itself.
pub fn approve(
    mut registration: Registration,
    measured: MeasuredIdentity,
) -> Result<Registration, ApprovalError> {
    if measured.runtime_file_digests.len() != registration.approval.runtime_files.len() {
        return Err(ApprovalError::RuntimeFilesMismatch);
    }
    for (declared, (path, digest)) in registration
        .approval
        .runtime_files
        .iter_mut()
        .zip(measured.runtime_file_digests.into_iter())
    {
        if declared.path != path {
            return Err(ApprovalError::RuntimeFilesMismatch);
        }
        declared.sha256 = digest;
    }

    registration.approval.approved = true;
    registration.approval.executable_sha256 = measured.executable_sha256;
    registration.approval.manifest_sha256 = measured.manifest_sha256;
    registration.approval.launch_tuple_sha256 = measured.launch_tuple_sha256;
    Ok(registration)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registration::{
        Approval, Consent, ExpectedConstraint, FixedEnvironment, ObservationConfidence,
        RegisteredContext, Registration, RuntimeFileDigest, Transport, TransportKind, TrustMode,
    };
    use crate::wire::{Metric, Scope, Unit};
    use std::fs;

    fn write_executable(dir: &Path, name: &str, contents: &[u8]) -> std::path::PathBuf {
        let path = dir.join(name);
        fs::write(&path, contents).unwrap();
        path
    }

    fn sample_expected() -> ExpectedConstraint {
        ExpectedConstraint {
            id: "c1".into(),
            scope: Scope::Account {
                handle: "acct".into(),
            },
            metric: Metric::TotalTokens,
            unit: Unit::Tokens,
            window_uuid: "win-1".into(),
            provider: "anthropic".into(),
            account_fingerprint: "fp".into(),
            pool_alias: "alias".into(),
            max_staleness_secs: 60,
            window_length_secs: 3600,
            capacity_ceiling: None,
        }
    }

    fn build(executable: &Path, runtime_files: Vec<RuntimeFileDigest>) -> Registration {
        Registration {
            registration_id: "reg-a".into(),
            extension_id: "ext.example".into(),
            extension_version: "1.0.0".into(),
            enabled: true,
            transport: Transport {
                kind: TransportKind::OneshotStdio,
                executable: executable.to_str().unwrap().to_owned(),
                args: vec!["--describe".into()],
                environment: FixedEnvironment {
                    lang: "C.UTF-8".into(),
                    lc_all: "C.UTF-8".into(),
                },
            },
            context: RegisteredContext {
                selector: "sel".into(),
                handle: "handle".into(),
                principal: "principal".into(),
                authority: "authority".into(),
            },
            expected: vec![sample_expected()],
            binding_revision: "1".into(),
            binding_verified_at: "2026-10-12T00:00:00Z".into(),
            scope_configured_at: "2026-10-12T00:00:00Z".into(),
            timeout_ms: 5_000,
            trust_mode: TrustMode::ObservationOnly,
            observation_confidence: ObservationConfidence::Low,
            approval: Approval {
                approved: true,
                revision: "1".into(),
                executable_sha256: String::new(),
                manifest_sha256: String::new(),
                launch_tuple_sha256: String::new(),
                runtime_files,
                endpoint_owner: "owner".into(),
                consent: Consent {
                    consented_by: "operator".into(),
                    recorded_at_reference: "ref-1".into(),
                },
            },
        }
    }

    /// Fills in all three bound digests from the registration's current
    /// on-disk state, the way an approval workflow would at approval
    /// time.
    fn approve(mut registration: Registration) -> Registration {
        let executable_digest = digest_file(Path::new(&registration.transport.executable)).unwrap();
        registration.approval.executable_sha256 = executable_digest.clone();
        let mut runtime_file_digests = Vec::new();
        for declared in &mut registration.approval.runtime_files {
            let digest = digest_file(Path::new(&declared.path)).unwrap();
            declared.sha256 = digest.clone();
            runtime_file_digests.push((declared.path.clone(), digest));
        }
        registration.approval.manifest_sha256 = manifest_digest(&registration);
        registration.approval.launch_tuple_sha256 =
            launch_tuple_digest(&registration, &executable_digest, &runtime_file_digests);
        registration
    }

    #[test]
    fn freshly_approved_registration_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let executable = write_executable(dir.path(), "ext", b"binary-one");
        let registration = approve(build(&executable, vec![]));
        assert!(verify_identity(&registration).is_ok());
    }

    #[test]
    fn disabled_or_unapproved_registration_is_refused_before_any_measurement() {
        let dir = tempfile::tempdir().unwrap();
        let executable = write_executable(dir.path(), "ext", b"binary-one");
        let mut registration = approve(build(&executable, vec![]));
        registration.enabled = false;
        assert_eq!(
            verify_identity(&registration).unwrap_err(),
            IdentityError::Disabled
        );

        let mut registration = approve(build(&executable, vec![]));
        registration.approval.approved = false;
        assert_eq!(
            verify_identity(&registration).unwrap_err(),
            IdentityError::NotApproved
        );
    }

    #[test]
    fn executable_digest_mismatch_after_file_edit_invalidates_trust() {
        let dir = tempfile::tempdir().unwrap();
        let executable = write_executable(dir.path(), "ext", b"binary-one");
        let registration = approve(build(&executable, vec![]));
        assert!(verify_identity(&registration).is_ok());

        fs::write(&executable, b"binary-two-different-bytes").unwrap();
        assert!(matches!(
            verify_identity(&registration).unwrap_err(),
            IdentityError::ExecutableDigestMismatch { .. }
        ));
    }

    #[test]
    fn changed_argv_invalidates_trust_without_touching_any_file() {
        let dir = tempfile::tempdir().unwrap();
        let executable = write_executable(dir.path(), "ext", b"binary-one");
        let registration = approve(build(&executable, vec![]));
        assert!(verify_identity(&registration).is_ok());

        // Tightened (HORO-1854 adversarial review, test-quality fix):
        // `transport.args` is part of both the manifest (the whole
        // registration minus `approval`) and the launch tuple, and
        // `verify_identity` checks the manifest digest first — so this
        // mismatch is always caught there, never reaching the launch
        // tuple check at all. The previous version of this test accepted
        // either variant, which meant it could not actually prove argv
        // is covered by the launch tuple specifically (as opposed to
        // only the manifest); see
        // `changed_runtime_files_order_invalidates_trust_via_the_launch_tuple_alone`
        // below for a test that isolates the launch-tuple-only case.
        let mut mutated = registration.clone();
        mutated.transport.args.push("--extra-flag".into());
        assert!(matches!(
            verify_identity(&mutated).unwrap_err(),
            IdentityError::ManifestDigestMismatch { .. }
        ));
    }

    // Regression/coverage test (HORO-1854 adversarial review,
    // test-quality fix): `approval.runtime_files` lives inside the
    // `approval` block, which `manifest_digest` deliberately excludes
    // (the approval record cannot bind over itself) — so the manifest
    // digest is *blind* to `runtime_files` entirely. The launch tuple
    // digest is the only thing that covers it, and `launch_tuple_digest`
    // folds the declared files in through in their *declared order*
    // (see its own doc comment), not a sorted/order-insensitive set — so
    // reordering the same set of files (same membership, same digests)
    // without touching any file on disk must still invalidate trust,
    // and must do so specifically via `LaunchTupleDigestMismatch`, since
    // the manifest digest cannot see this change at all.
    #[test]
    fn changed_runtime_files_order_invalidates_trust_via_the_launch_tuple_alone() {
        let dir = tempfile::tempdir().unwrap();
        let executable = write_executable(dir.path(), "ext", b"binary-one");
        let lib_a = write_executable(dir.path(), "a.so", b"lib-a");
        let lib_b = write_executable(dir.path(), "b.so", b"lib-b");
        let runtime_files = vec![
            RuntimeFileDigest {
                path: lib_a.to_str().unwrap().to_owned(),
                sha256: String::new(),
            },
            RuntimeFileDigest {
                path: lib_b.to_str().unwrap().to_owned(),
                sha256: String::new(),
            },
        ];
        let registration = approve(build(&executable, runtime_files));
        assert!(verify_identity(&registration).is_ok());

        let mut reordered = registration.clone();
        reordered.approval.runtime_files.swap(0, 1);
        assert!(matches!(
            verify_identity(&reordered).unwrap_err(),
            IdentityError::LaunchTupleDigestMismatch { .. }
        ));
    }

    /// Companion to the order test above: dropping one declared runtime
    /// file (same remaining files, same digests, nothing on disk
    /// touched) is a *membership* change, not just an order change, and
    /// must be caught the same way — via the launch tuple, since the
    /// manifest digest still cannot see into `approval.runtime_files` at
    /// all.
    #[test]
    fn dropped_runtime_file_invalidates_trust_via_the_launch_tuple_alone() {
        let dir = tempfile::tempdir().unwrap();
        let executable = write_executable(dir.path(), "ext", b"binary-one");
        let lib_a = write_executable(dir.path(), "a.so", b"lib-a");
        let lib_b = write_executable(dir.path(), "b.so", b"lib-b");
        let runtime_files = vec![
            RuntimeFileDigest {
                path: lib_a.to_str().unwrap().to_owned(),
                sha256: String::new(),
            },
            RuntimeFileDigest {
                path: lib_b.to_str().unwrap().to_owned(),
                sha256: String::new(),
            },
        ];
        let registration = approve(build(&executable, runtime_files));
        assert!(verify_identity(&registration).is_ok());

        let mut dropped = registration.clone();
        dropped.approval.runtime_files.pop();
        assert!(matches!(
            verify_identity(&dropped).unwrap_err(),
            IdentityError::LaunchTupleDigestMismatch { .. }
        ));
    }

    #[test]
    fn changed_declared_runtime_file_invalidates_trust() {
        let dir = tempfile::tempdir().unwrap();
        let executable = write_executable(dir.path(), "ext", b"binary-one");
        let runtime_file = write_executable(dir.path(), "lib.so", b"lib-contents");
        let runtime_files = vec![RuntimeFileDigest {
            path: runtime_file.to_str().unwrap().to_owned(),
            sha256: String::new(),
        }];
        let registration = approve(build(&executable, runtime_files));
        assert!(verify_identity(&registration).is_ok());

        fs::write(&runtime_file, b"a-completely-different-library").unwrap();
        assert!(matches!(
            verify_identity(&registration).unwrap_err(),
            IdentityError::RuntimeFileDigestMismatch { .. }
        ));
    }

    #[test]
    fn edited_manifest_field_invalidates_trust_even_though_approval_block_is_excluded() {
        let dir = tempfile::tempdir().unwrap();
        let executable = write_executable(dir.path(), "ext", b"binary-one");
        let registration = approve(build(&executable, vec![]));
        assert!(verify_identity(&registration).is_ok());

        let mut mutated = registration.clone();
        mutated.context.handle = "a-different-handle".into();
        assert!(matches!(
            verify_identity(&mutated).unwrap_err(),
            IdentityError::ManifestDigestMismatch { .. }
        ));
    }

    /// Proves `enabled` is itself covered by the manifest digest — the
    /// property `tests/shared_pool_anti_vacuity.rs`'s `attack4` cannot
    /// prove through the crate's public API (see that test's own
    /// comment): `approve()` here is this module's *test-only* helper,
    /// which can compute digests for a registration with
    /// `enabled == false`, unlike the integration suite's black-box
    /// `tests/common::approve()` (which refuses to run against anything
    /// not already enabled). Build disabled, approve while disabled
    /// (digests bound to `enabled == false`), then flip `enabled` to
    /// `true` in memory without re-approving: if `enabled` were excluded
    /// from the manifest digest's computation, this would verify
    /// successfully; because it's included, flipping it invalidates the
    /// manifest digest specifically.
    #[test]
    fn flipping_enabled_without_reapproval_trips_the_manifest_digest() {
        let dir = tempfile::tempdir().unwrap();
        let executable = write_executable(dir.path(), "ext", b"binary-one");
        let mut disabled = build(&executable, vec![]);
        disabled.enabled = false;
        let disabled = approve(disabled);
        // `verify_identity` checks `Disabled` before it ever measures a
        // digest (see its own doc comment), so there is no "verifies ok
        // while disabled" state to assert here — that's the exact gap
        // that makes this property untestable through the public API
        // alone, and the reason this test lives in-crate.
        assert_eq!(
            verify_identity(&disabled).unwrap_err(),
            IdentityError::Disabled
        );

        let mut flipped = disabled.clone();
        flipped.enabled = true;
        assert!(
            matches!(
                verify_identity(&flipped).unwrap_err(),
                IdentityError::ManifestDigestMismatch { .. }
            ),
            "enabled must be covered by the manifest digest"
        );

        // Control: re-approving after the flip (recomputing all three
        // digests against the now-enabled state) must succeed — the
        // failure above comes only from the stale approval, not from
        // some permanent inability to approve an enabled registration.
        let reapproved = approve(flipped);
        assert!(verify_identity(&reapproved).is_ok());
    }

    #[test]
    fn measure_identity_then_approve_produces_a_registration_that_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let executable = write_executable(dir.path(), "ext", b"binary-one");
        let lib_a = write_executable(dir.path(), "a.so", b"lib-a");
        let runtime_files = vec![RuntimeFileDigest {
            path: lib_a.to_str().unwrap().to_owned(),
            sha256: String::new(),
        }];
        let unapproved = build(&executable, runtime_files);

        let measured = super::measure_identity(&unapproved).unwrap();
        let approved = super::approve(unapproved, measured).unwrap();

        assert!(approved.approval.approved);
        assert!(verify_identity(&approved).is_ok());
    }

    #[test]
    fn approve_sets_only_approved_flag_and_the_three_digests() {
        let dir = tempfile::tempdir().unwrap();
        let executable = write_executable(dir.path(), "ext", b"binary-one");
        let mut unapproved = build(&executable, vec![]);
        unapproved.approval.approved = false;
        let before = unapproved.clone();

        let measured = super::measure_identity(&unapproved).unwrap();
        let approved = super::approve(unapproved, measured.clone()).unwrap();

        assert!(approved.approval.approved);
        assert_eq!(
            approved.approval.executable_sha256,
            measured.executable_sha256
        );
        assert_eq!(approved.approval.manifest_sha256, measured.manifest_sha256);
        assert_eq!(
            approved.approval.launch_tuple_sha256,
            measured.launch_tuple_sha256
        );

        // Nothing else moved.
        let mut expected_approval = before.approval.clone();
        expected_approval.approved = true;
        expected_approval.executable_sha256 = measured.executable_sha256;
        expected_approval.manifest_sha256 = measured.manifest_sha256;
        expected_approval.launch_tuple_sha256 = measured.launch_tuple_sha256;
        assert_eq!(approved.approval, expected_approval);
        assert_eq!(approved.context, before.context);
        assert_eq!(approved.transport, before.transport);
        assert_eq!(approved.enabled, before.enabled);
    }

    #[test]
    fn measure_identity_surfaces_io_error_for_a_missing_executable() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let registration = build(&missing, vec![]);
        assert!(matches!(
            super::measure_identity(&registration).unwrap_err(),
            IdentityError::Io(_, _)
        ));
    }
}
