//! Ownership-aware metadata storage. Passive reads never initialize state.

use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::catalog::RegistryDocument;
use super::contract::HostContract;
use super::{RegistryEffect, RegistryFailure};

const MAX_REGISTRY_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryStamp(Option<(String, u64, String)>);

#[derive(Clone, Debug)]
pub struct RegistrySnapshot {
    document: Option<RegistryDocument>,
    stamp: RegistryStamp,
}

// Live observations belong to a bounded read or mutation, never a caller-held snapshot.
struct ObservedRegistry {
    document: Option<RegistryDocument>,
    stamp: RegistryStamp,
    file_identity: Option<Metadata>,
    namespace: Namespace,
}

impl RegistrySnapshot {
    pub fn document(&self) -> Option<&RegistryDocument> {
        self.document.as_ref()
    }
    pub fn stamp(&self) -> &RegistryStamp {
        &self.stamp
    }
}

pub struct AdapterRegistry {
    root: Result<PathBuf, RegistryFailure>,
    contract: HostContract,
}

/// Lifecycle commits reuse the existing outside-root reservation. It remains
/// held across intent, target mutation, readback and metadata completion.
pub(super) struct LifecycleTransaction<'a> {
    registry: &'a AdapterRegistry,
    guard: crate::write_lock::WriteLockGuard,
    observed: ObservedRegistry,
    changed: bool,
    created_legacy_root: Option<super::parents::PreparedParent>,
}

impl LifecycleTransaction<'_> {
    pub(super) fn document(&self) -> Option<&RegistryDocument> {
        self.observed.document.as_ref()
    }

    pub(super) fn root(&self) -> Result<&Path, RegistryFailure> {
        self.registry.root()
    }

    pub(super) fn verify(&self) -> Result<(), RegistryFailure> {
        if let Some(proof) = &self.created_legacy_root {
            proof.check().map_err(|failure| self.qualify(failure))?;
        }
        self.registry
            .verify_no_change(
                &self.observed.stamp,
                &self.guard,
                self.observed.file_identity.as_ref(),
                &self.observed.namespace,
            )
            .map_err(|failure| self.qualify(failure))
    }

    pub(super) fn qualify(&self, mut failure: RegistryFailure) -> RegistryFailure {
        if self.changed {
            failure.effect = RegistryEffect::EffectUnconfirmed;
        }
        failure
    }

    pub(super) fn note_configuration_change(&mut self) {
        self.changed = true;
    }

    pub(super) fn prepare_legacy_marker_root(&mut self) -> Result<(), RegistryFailure> {
        self.verify()?;
        let root = self.registry.root()?.to_owned();
        let entry = self
            .observed
            .namespace
            .entries
            .iter()
            .position(|(path, _, _)| path == &root)
            .ok_or_else(|| refusal("registry", "state namespace changed"))?;
        if self.observed.namespace.entries[entry].1.is_some() {
            return Ok(());
        }
        storage_boundary(CommitBoundary::BeforeLegacyRootCreation)?;
        let (proof, held) = self.observed.namespace.parent.create_child_exclusive(
            root.file_name()
                .ok_or_else(|| refusal("registry", "invalid state root"))?,
        )?;
        self.changed = true;
        let owned = held
            .metadata()
            .map_err(|_| self.qualify(refusal("registry", "state unavailable")))?;
        self.observed.namespace.entries[entry].1 = Some(owned);
        self.created_legacy_root = Some(proof);
        storage_boundary(CommitBoundary::AfterLegacyRootCreation)
            .map_err(|failure| self.qualify(failure))?;
        self.verify()
    }

    pub(super) fn commit(&mut self, document: &RegistryDocument) -> Result<(), RegistryFailure> {
        let expected = RegistryStamp(Some((
            document.registry_id.clone(),
            document.revision,
            digest(&document.encode(&self.registry.contract)?),
        )));
        let observed = self
            .registry
            .commit_observed(
                document,
                &self.observed.stamp,
                &self.guard,
                self.observed.file_identity.as_ref(),
                &self.observed.namespace,
            )
            .map_err(|failure| self.qualify(failure))?;
        self.changed = true;
        storage_boundary(CommitBoundary::AfterLifecycleCommit)
            .map_err(|failure| self.qualify(failure))?;
        observed
            .namespace
            .check()
            .map_err(|failure| self.qualify(failure))?;
        verify_file_revision(
            &self.registry.root()?.join("host-adapters/registry.json"),
            observed.file_identity.as_ref(),
        )
        .map_err(|failure| self.qualify(failure))?;
        if observed.stamp != expected {
            return Err(RegistryFailure::unconfirmed(
                "registry",
                "committed registry changed",
            ));
        }
        self.observed = observed;
        self.verify()
    }
}

impl AdapterRegistry {
    pub(super) fn begin_lifecycle(
        &self,
        expected: &RegistryStamp,
        prepare_missing: bool,
    ) -> Result<LifecycleTransaction<'_>, RegistryFailure> {
        let (guard, observed) = self.begin(expected, prepare_missing)?;
        Ok(LifecycleTransaction {
            registry: self,
            guard,
            observed,
            changed: false,
            created_legacy_root: None,
        })
    }

    pub(super) fn begin_admission(
        &self,
        expected: &RegistryStamp,
    ) -> Result<LifecycleTransaction<'_>, RegistryFailure> {
        let (guard, observed) = self.begin_with_creation(expected, false, false, false)?;
        Ok(LifecycleTransaction {
            registry: self,
            guard,
            observed,
            changed: false,
            created_legacy_root: None,
        })
    }

    pub(super) fn begin_legacy(
        &self,
        expected: &RegistryStamp,
        prepare_parent: bool,
    ) -> Result<LifecycleTransaction<'_>, RegistryFailure> {
        let (guard, observed) = self.begin_with_creation(expected, prepare_parent, true, true)?;
        Ok(LifecycleTransaction {
            registry: self,
            guard,
            observed,
            changed: false,
            created_legacy_root: None,
        })
    }
    /// Construction neither creates directories nor reads host configuration.
    pub fn new(state_root: PathBuf, contract: HostContract) -> Self {
        Self {
            root: resolve_root_locator(&state_root),
            contract,
        }
    }

    pub(super) fn root(&self) -> Result<&Path, RegistryFailure> {
        self.root.as_deref().map_err(Clone::clone)
    }

    pub fn read(&self) -> Result<RegistrySnapshot, RegistryFailure> {
        let observed = self.read_observed()?;
        Ok(RegistrySnapshot {
            document: observed.document,
            stamp: observed.stamp,
        })
    }

    fn read_observed(&self) -> Result<ObservedRegistry, RegistryFailure> {
        let namespace = Namespace::capture(self.root()?)?;
        let path = self.root()?.join("host-adapters/registry.json");
        let raw = read_private(&path)?;
        namespace.check()?;
        match raw {
            None => Ok(ObservedRegistry {
                document: None,
                stamp: RegistryStamp(None),
                file_identity: None,
                namespace,
            }),
            Some((raw, file_identity)) => {
                let document = RegistryDocument::parse(&raw, &self.contract)?;
                namespace.check()?;
                verify_file_revision(&path, Some(&file_identity))?;
                let stamp = RegistryStamp(Some((
                    document.registry_id.clone(),
                    document.revision,
                    digest(&raw),
                )));
                Ok(ObservedRegistry {
                    document: Some(document),
                    stamp,
                    file_identity: Some(file_identity),
                    namespace,
                })
            }
        }
    }

    /// Pure preview against an immutable snapshot; apply still rechecks under lock.
    pub fn check_registration(
        &self,
        snapshot: &RegistrySnapshot,
        manifest: &super::contract::ValidatedManifest,
    ) -> Result<(), RegistryFailure> {
        if matches!(manifest.adapter_id(), "claude_code" | "codex") {
            return Err(refusal("registry", "reserved adapter id"));
        }
        if let Some(document) = &snapshot.document {
            registration_candidate(Some(document.clone()), manifest.clone())?
                .encode(&self.contract)?;
        }
        // One bounded manifest record and the fixed-size generation envelope fit
        // the catalog bounds. Allocate the real generation only during apply.
        Ok(())
    }

    /// Pure preview using the same candidate encoder as guarded removal.
    pub fn check_unregistration(
        &self,
        snapshot: &RegistrySnapshot,
        id: &str,
    ) -> Result<(), RegistryFailure> {
        unregistration_candidate(snapshot.document.clone(), id)?.encode(&self.contract)?;
        Ok(())
    }

    /// Preview a measured review without granting trust or executing code.
    /// Apply independently remeasures identity under its reservation.
    pub fn check_trust_confirmation(
        &self,
        snapshot: &RegistrySnapshot,
        review: &TrustReview,
    ) -> Result<(), RegistryFailure> {
        let document = snapshot
            .document
            .clone()
            .ok_or_else(|| refusal("registry", "unknown adapter"))?;
        if let Some(candidate) = trust_candidate(
            document,
            review.adapter_id(),
            review.manifest_digest(),
            review.implementation_digest(),
            review.confirmation_digest(),
        )? {
            candidate.encode(&self.contract)?;
        }
        Ok(())
    }

    pub fn register(
        &self,
        manifest: super::contract::ValidatedManifest,
        expected: &RegistryStamp,
    ) -> Result<RegistryEffect, RegistryFailure> {
        if matches!(manifest.adapter_id(), "claude_code" | "codex") {
            return Err(refusal("registry", "reserved adapter id"));
        }
        let (guard, snapshot) = self.begin(expected, true)?;
        let original_file = snapshot.file_identity.clone();
        let original_namespace = snapshot.namespace.clone();
        let document = registration_candidate(snapshot.document, manifest)?;
        self.commit(
            &document,
            expected,
            &guard,
            original_file.as_ref(),
            &original_namespace,
        )
    }

    pub fn unregister(
        &self,
        id: &str,
        expected: &RegistryStamp,
    ) -> Result<RegistryEffect, RegistryFailure> {
        if matches!(id, "claude_code" | "codex") {
            return Err(refusal(
                "registry",
                "builtin adapter cannot be unregistered",
            ));
        }
        let (guard, snapshot) = self.begin(expected, false)?;
        let original_file = snapshot.file_identity.clone();
        let original_namespace = snapshot.namespace.clone();
        let document = unregistration_candidate(snapshot.document, id)?;
        self.commit(
            &document,
            expected,
            &guard,
            original_file.as_ref(),
            &original_namespace,
        )
    }

    pub fn review_trust(&self, id: &str) -> Result<TrustReview, RegistryFailure> {
        let before = self.read()?;
        let document = before
            .document
            .as_ref()
            .ok_or_else(|| refusal("registry", "unknown adapter"))?;
        let record = document
            .adapters
            .get(id)
            .ok_or_else(|| refusal("registry", "unknown adapter"))?;
        let implementation_digest = super::identity::measure_identity(&record.manifest)?;
        let after = self.read()?;
        let current = after
            .document
            .as_ref()
            .ok_or_else(|| refusal("registry", "registration changed"))?;
        let latest = current
            .adapters
            .get(id)
            .ok_or_else(|| refusal("registry", "registration changed"))?;
        if current.registry_id != document.registry_id
            || latest.registration_revision != record.registration_revision
            || latest.manifest.raw() != record.manifest.raw()
        {
            return Err(refusal("registry", "registration changed"));
        }
        let confirmation_digest = super::catalog::confirmation_digest(
            &current.registry_id,
            latest.registration_revision,
            latest.manifest.digest(),
            &implementation_digest,
        );
        Ok(TrustReview {
            adapter_id: id.to_owned(),
            manifest_digest: latest.manifest.digest().to_owned(),
            recorded_match: latest
                .trust
                .as_ref()
                .is_some_and(|t| t.implementation_digest == implementation_digest),
            implementation_digest,
            confirmation_digest,
        })
    }

    pub fn confirm_trust(
        &self,
        id: &str,
        expected_manifest_digest: &str,
        confirmation: &str,
        expected: &RegistryStamp,
    ) -> Result<RegistryEffect, RegistryFailure> {
        let (guard, snapshot) = self.begin(expected, false)?;
        let original_file = snapshot.file_identity.clone();
        let original_namespace = snapshot.namespace.clone();
        let document = snapshot
            .document
            .ok_or_else(|| refusal("registry", "unknown adapter"))?;
        let record = document
            .adapters
            .get(id)
            .ok_or_else(|| refusal("registry", "unknown adapter"))?;
        if record.manifest.digest() != expected_manifest_digest {
            return Err(refusal("registry", "manifest changed"));
        }
        let measured = super::identity::measure_identity(&record.manifest)?;
        let Some(document) = trust_candidate(
            document,
            id,
            expected_manifest_digest,
            &measured,
            confirmation,
        )?
        else {
            self.verify_no_change(
                expected,
                &guard,
                original_file.as_ref(),
                &original_namespace,
            )?;
            return Ok(RegistryEffect::NoChange);
        };
        self.commit(
            &document,
            expected,
            &guard,
            original_file.as_ref(),
            &original_namespace,
        )
    }

    pub fn clear_trust(
        &self,
        id: &str,
        expected: &RegistryStamp,
    ) -> Result<RegistryEffect, RegistryFailure> {
        let (guard, snapshot) = self.begin(expected, false)?;
        let original_file = snapshot.file_identity.clone();
        let original_namespace = snapshot.namespace.clone();
        let mut document = snapshot
            .document
            .ok_or_else(|| refusal("registry", "unknown adapter"))?;
        let record = document
            .adapters
            .get(id)
            .ok_or_else(|| refusal("registry", "unknown adapter"))?;
        if record.trust.is_none() {
            self.verify_no_change(
                expected,
                &guard,
                original_file.as_ref(),
                &original_namespace,
            )?;
            return Ok(RegistryEffect::NoChange);
        }
        let revision = next_revision(document.revision)?;
        let record = document.adapters.get_mut(id).expect("validated record");
        record.trust = None;
        record.trust_revision = revision;
        document.revision = revision;
        self.commit(
            &document,
            expected,
            &guard,
            original_file.as_ref(),
            &original_namespace,
        )
    }

    fn begin(
        &self,
        expected: &RegistryStamp,
        prepare_missing: bool,
    ) -> Result<(crate::write_lock::WriteLockGuard, ObservedRegistry), RegistryFailure> {
        self.begin_with_creation(expected, prepare_missing, true, false)
    }

    fn begin_with_creation(
        &self,
        expected: &RegistryStamp,
        prepare_missing: bool,
        create_reservation: bool,
        allow_absent_catalog: bool,
    ) -> Result<(crate::write_lock::WriteLockGuard, ObservedRegistry), RegistryFailure> {
        let initial = self.read_observed()?;
        if &initial.stamp != expected {
            return Err(refusal("registry", "registry changed"));
        }
        if initial.document.is_none() && !prepare_missing && !allow_absent_catalog {
            return Err(refusal("registry", "unknown adapter"));
        }
        storage_boundary(CommitBoundary::BeforePreparation)?;
        let parent = &initial.namespace.parent;
        let prepared = if parent.needs_preparation() {
            if !prepare_missing {
                return Err(refusal("registry", "unknown adapter"));
            }
            Some(parent.prepare()?)
        } else {
            None
        };
        if let Some(observation) = &prepared {
            observation.check()?;
        } else {
            parent.check()?;
        }
        let target = reservation_target(self.root()?)?;
        let observed_parent = prepared
            .as_ref()
            .map(|p| p.canonical_path())
            .unwrap_or_else(|| parent.canonical_path());
        if target.parent() != Some(observed_parent) {
            return Err(refusal("registry", "state namespace changed"));
        }
        let acquisition = if create_reservation {
            crate::write_lock::acquire_private(&target)
        } else {
            crate::write_lock::acquire_existing_private(&target)
        };
        let guard = acquisition.map_err(|_| refusal("write_lock", "registry lock unavailable"))?;
        guard
            .verify()
            .map_err(|_| refusal("registry", "state reservation changed"))?;
        if let Some(observation) = &prepared {
            observation.check()?;
        } else {
            parent.check()?;
        }
        if reservation_target(self.root()?)? != target {
            return Err(refusal("registry", "state namespace changed"));
        }
        let snapshot = self.read_observed()?;
        guard
            .verify()
            .map_err(|_| refusal("registry", "state reservation changed"))?;
        if let Some(observation) = &prepared {
            observation.check()?;
        } else {
            parent.check()?;
        }
        if &snapshot.stamp != expected {
            return Err(refusal("registry", "registry changed"));
        }
        Ok((guard, snapshot))
    }

    fn verify_no_change(
        &self,
        expected: &RegistryStamp,
        guard: &crate::write_lock::WriteLockGuard,
        original_file: Option<&Metadata>,
        original_namespace: &Namespace,
    ) -> Result<(), RegistryFailure> {
        storage_boundary(CommitBoundary::BeforeNoChange)?;
        self.verify_reservation(guard)?;
        original_namespace.check()?;
        verify_file_revision(
            &self.root()?.join("host-adapters/registry.json"),
            original_file,
        )?;
        if &self.read()?.stamp != expected {
            return Err(refusal("registry", "registry changed"));
        }
        Ok(())
    }

    fn verify_reservation(
        &self,
        guard: &crate::write_lock::WriteLockGuard,
    ) -> Result<(), RegistryFailure> {
        guard
            .verify()
            .map_err(|_| refusal("registry", "state reservation changed"))?;
        let actual = crate::write_lock::derive_lock_path(&reservation_target(self.root()?)?)
            .map_err(|_| refusal("registry", "state reservation changed"))?;
        if actual != guard.lock_path {
            return Err(refusal("registry", "state reservation changed"));
        }
        Ok(())
    }

    fn commit(
        &self,
        document: &RegistryDocument,
        expected: &RegistryStamp,
        guard: &crate::write_lock::WriteLockGuard,
        original_file: Option<&Metadata>,
        original_namespace: &Namespace,
    ) -> Result<RegistryEffect, RegistryFailure> {
        self.commit_observed(document, expected, guard, original_file, original_namespace)?;
        Ok(RegistryEffect::AppliedVerified)
    }

    fn commit_observed(
        &self,
        document: &RegistryDocument,
        expected: &RegistryStamp,
        guard: &crate::write_lock::WriteLockGuard,
        original_file: Option<&Metadata>,
        original_namespace: &Namespace,
    ) -> Result<ObservedRegistry, RegistryFailure> {
        self.verify_reservation(guard)?;
        original_namespace.check()?;
        verify_file_revision(
            &self.root()?.join("host-adapters/registry.json"),
            original_file,
        )?;
        let raw = document.encode(&self.contract)?;
        // Only missing owned components are created; an existing legacy root is never chmodded.
        ensure_owned_directory(self.root()?, false)?;
        let directory = self.root()?.join("host-adapters");
        ensure_owned_directory(&directory, true)?;
        let namespace = Namespace::capture(self.root()?)?;
        original_namespace.check_existing()?;
        if &self.read()?.stamp != expected {
            return Err(refusal("registry", "registry changed"));
        }
        let path = directory.join("registry.json");
        let temporary_path =
            directory.join(format!(".registry-{}.tmp", uuid::Uuid::new_v4().simple()));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .custom_flags(no_follow_flags())
            .open(&temporary_path)
            .map_err(|_| refusal("registry", "temporary file unavailable"))?;
        let initial = file
            .metadata()
            .map_err(|_| refusal("registry", "temporary file unavailable"))?;
        let mut temporary = OwnedTemporary {
            path: temporary_path,
            metadata: initial,
            landed: false,
        };
        if !safe_file(&temporary.metadata) {
            return Err(refusal("registry", "unsafe temporary file"));
        }
        file.write_all(&raw)
            .and_then(|_| file.flush())
            .and_then(|_| file.sync_all())
            .map_err(|_| refusal("registry", "temporary write failed"))?;
        namespace.check()?;
        if &self.read()?.stamp != expected {
            return Err(refusal("registry", "registry changed"));
        }
        temporary.check()?;
        self.verify_reservation(guard)?;
        storage_boundary(CommitBoundary::BeforeReplace)?;
        original_namespace.check_existing()?;
        verify_file_revision(&path, original_file)?;
        fs::rename(&temporary.path, &path)
            .map_err(|_| refusal("registry", "registry replacement failed"))?;
        temporary.landed = true;
        // A possibly committed write is retained on any subsequent failure.
        let verified = (|| {
            storage_boundary(CommitBoundary::AfterReplace)?;
            original_namespace.check_existing()?;
            self.verify_reservation(guard)?;
            let directory_file = open_no_follow(&directory)?;
            directory_file
                .sync_all()
                .map_err(|_| refusal("registry", "directory synchronization failed"))?;
            namespace.check()?;
            storage_boundary(CommitBoundary::Readback)?;
            let (observed, identity) =
                read_private(&path)?.ok_or_else(|| refusal("registry", "readback unavailable"))?;
            if observed != raw {
                return Err(refusal("registry", "readback changed"));
            }
            let parsed = RegistryDocument::parse(&observed, &self.contract)?;
            let held = file
                .metadata()
                .map_err(|_| refusal("registry", "readback unavailable"))?;
            if !safe_file(&held) || !unchanged(&held, &identity) {
                return Err(refusal("registry", "readback file changed"));
            }
            namespace.check()?;
            self.verify_reservation(guard)?;
            verify_file_revision(&path, Some(&identity))?;
            Ok(ObservedRegistry {
                stamp: RegistryStamp(Some((
                    parsed.registry_id.clone(),
                    parsed.revision,
                    digest(&observed),
                ))),
                document: Some(parsed),
                file_identity: Some(identity),
                namespace,
            })
        })();
        verified.map_err(|_: RegistryFailure| {
            RegistryFailure::unconfirmed("registry", "registry effect unconfirmed")
        })
    }
}

/// Passive measured code identity. This is never host trust or execution admission.
#[derive(Clone, Debug)]
pub struct TrustReview {
    adapter_id: String,
    manifest_digest: String,
    implementation_digest: String,
    confirmation_digest: String,
    recorded_match: bool,
}
impl TrustReview {
    pub fn adapter_id(&self) -> &str {
        &self.adapter_id
    }
    pub fn manifest_digest(&self) -> &str {
        &self.manifest_digest
    }
    pub fn implementation_digest(&self) -> &str {
        &self.implementation_digest
    }
    pub fn confirmation_digest(&self) -> &str {
        &self.confirmation_digest
    }
    pub fn recorded_match(&self) -> bool {
        self.recorded_match
    }
    pub fn identity_scope(&self) -> &'static str {
        "declared_launch_and_runtime_files"
    }
}
fn registration_candidate(
    document: Option<RegistryDocument>,
    manifest: super::contract::ValidatedManifest,
) -> Result<RegistryDocument, RegistryFailure> {
    let mut document = document.unwrap_or_else(|| RegistryDocument {
        schema_version: 1,
        registry_id: uuid::Uuid::new_v4().simple().to_string(),
        revision: 0,
        adapters: std::collections::BTreeMap::new(),
        installations: std::collections::BTreeMap::new(),
    });
    if document.adapters.contains_key(manifest.adapter_id()) {
        return Err(refusal("registry", "adapter already registered"));
    }
    let revision = next_revision(document.revision)?;
    document.adapters.insert(
        manifest.adapter_id().to_owned(),
        super::catalog::RegistryRecord {
            manifest,
            registration_revision: revision,
            trust_revision: 0,
            trust: None,
        },
    );
    document.revision = revision;
    Ok(document)
}

fn unregistration_candidate(
    document: Option<RegistryDocument>,
    id: &str,
) -> Result<RegistryDocument, RegistryFailure> {
    if matches!(id, "claude_code" | "codex") {
        return Err(refusal(
            "registry",
            "builtin adapter cannot be unregistered",
        ));
    }
    let mut document = document.ok_or_else(|| refusal("registry", "unknown adapter"))?;
    if document.adapters.remove(id).is_none() {
        return Err(refusal("registry", "unknown adapter"));
    }
    document.revision = next_revision(document.revision)?;
    Ok(document)
}

fn trust_candidate(
    mut document: RegistryDocument,
    id: &str,
    manifest_digest: &str,
    measured: &str,
    confirmation: &str,
) -> Result<Option<RegistryDocument>, RegistryFailure> {
    let record = document
        .adapters
        .get(id)
        .ok_or_else(|| refusal("registry", "unknown adapter"))?;
    if record.manifest.digest() != manifest_digest {
        return Err(refusal("registry", "manifest changed"));
    }
    let actual_confirmation = super::catalog::confirmation_digest(
        &document.registry_id,
        record.registration_revision,
        manifest_digest,
        measured,
    );
    if confirmation != actual_confirmation {
        return Err(refusal(
            "code_trust",
            "confirmation does not match current registration",
        ));
    }
    if record.trust.as_ref().is_some_and(|grant| {
        grant.implementation_digest == measured && grant.confirmation_digest == actual_confirmation
    }) {
        return Ok(None);
    }
    let revision = next_revision(document.revision)?;
    let record = document.adapters.get_mut(id).expect("validated record");
    record.trust = Some(super::catalog::CodeTrust {
        manifest_digest: manifest_digest.to_owned(),
        implementation_digest: measured.to_owned(),
        confirmation_digest: actual_confirmation,
        confirmed_at: time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|_| refusal("code_trust", "confirmation timestamp unavailable"))?,
    });
    record.trust_revision = revision;
    document.revision = revision;
    Ok(Some(document))
}

fn next_revision(current: u64) -> Result<u64, RegistryFailure> {
    current
        .checked_add(1)
        .filter(|v| *v <= 9_007_199_254_740_991)
        .ok_or_else(|| refusal("registry", "registry revision exhausted"))
}

/// Legacy deletion is serialized with registry writers and refuses any adapter entry.
/// The caller obtains user confirmation before entering this bounded operation.
pub fn delete_legacy_state(root: &Path) -> Result<(), RegistryFailure> {
    let locator = resolve_root_locator(root)?;
    let root = locator.as_path();
    let passive = Namespace::capture(root)?;
    if metadata_or_absent(root)?.is_none() {
        passive.check()?;
        return Ok(());
    }
    storage_boundary(CommitBoundary::BeforeLegacyLock)?;
    let target = reservation_target(root)?;
    let _guard = crate::write_lock::acquire_private(&target)
        .map_err(|_| refusal("write_lock", "registry lock unavailable"))?;
    passive.check()?;
    if reservation_target(root)? != target {
        return Err(refusal("registry", "state namespace changed"));
    }
    if metadata_or_absent(&root.join("host-adapters"))?.is_some() {
        return Err(refusal(
            "registry",
            "adapter state requires explicit unregister and ownership review",
        ));
    }
    let namespace = Namespace::capture(root)?;
    if metadata_or_absent(root)?.is_none() {
        return Ok(());
    }
    storage_boundary(CommitBoundary::BeforeDelete)?;
    passive.check()?;
    namespace.check()?;
    _guard
        .verify()
        .map_err(|_| refusal("registry", "state reservation changed"))?;
    if reservation_target(root)? != target
        || metadata_or_absent(&root.join("host-adapters"))?.is_some()
    {
        return Err(refusal("registry", "state namespace changed"));
    }
    passive.check()?;
    namespace.check()?;
    fs::remove_dir_all(root).map_err(|_| refusal("registry", "legacy state deletion failed"))
}

// Preserve `..` for the descriptor-aware resolver: lexical collapse across a
// symlink would select a different parent. Capture cwd only at operation setup.
fn resolve_root_locator(root: &Path) -> Result<PathBuf, RegistryFailure> {
    std::path::absolute(root)
        .map(|absolute| absolute.components().collect())
        .map_err(|_| refusal("registry", "state locator unavailable"))
}

/// Stable reservation outside the state root also coordinates legacy deletion.
pub fn reservation_target(root: &Path) -> Result<PathBuf, RegistryFailure> {
    let name = root
        .file_name()
        .ok_or_else(|| refusal("registry", "invalid state root"))?;
    let parent = root
        .parent()
        .ok_or_else(|| refusal("registry", "invalid state root"))?;
    let canonical =
        fs::canonicalize(parent).map_err(|_| refusal("registry", "state parent unavailable"))?;
    let mut target = std::ffi::OsString::from(".");
    target.push(name);
    target.push(".host-adapter-registry");
    Ok(canonical.join(target))
}

fn refusal(stage: &'static str, reason: &'static str) -> RegistryFailure {
    RegistryFailure::new(stage, reason)
}
fn digest(raw: &[u8]) -> String {
    format!(
        "sha256:{}",
        Sha256::digest(raw)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}
fn no_follow_flags() -> i32 {
    (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32
}
fn open_no_follow(path: &Path) -> Result<File, RegistryFailure> {
    OpenOptions::new()
        .read(true)
        .custom_flags(no_follow_flags())
        .open(path)
        .map_err(|_| refusal("registry", "state unavailable"))
}
fn same_inode(a: &Metadata, b: &Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}
fn unchanged(a: &Metadata, b: &Metadata) -> bool {
    same_inode(a, b)
        && a.mode() == b.mode()
        && a.uid() == b.uid()
        && a.nlink() == b.nlink()
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}
fn safe_directory(m: &Metadata, private: bool) -> bool {
    m.is_dir()
        && m.uid() == rustix::process::getuid().as_raw()
        && m.mode() & if private { 0o077 } else { 0o022 } == 0
}
fn safe_file(m: &Metadata) -> bool {
    m.is_file()
        && m.uid() == rustix::process::getuid().as_raw()
        && m.mode() & 0o077 == 0
        && m.nlink() == 1
}
fn metadata_or_absent(path: &Path) -> Result<Option<Metadata>, RegistryFailure> {
    match fs::symlink_metadata(path) {
        Ok(m) => Ok(Some(m)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(refusal("registry", "state unavailable")),
    }
}

#[derive(Clone, Debug)]
struct Namespace {
    entries: Vec<(PathBuf, Option<Metadata>, bool)>,
    parent: super::parents::ParentObservation,
}
impl Namespace {
    fn capture(root: &Path) -> Result<Self, RegistryFailure> {
        if !root.is_absolute()
            || root
                .components()
                .any(|c| matches!(c, std::path::Component::CurDir))
        {
            return Err(refusal("registry", "invalid state root"));
        }
        let parent = super::parents::ParentObservation::capture(
            root.parent()
                .ok_or_else(|| refusal("registry", "invalid state root"))?,
        )?;
        let canonical_parent = parent.canonical_path().to_owned();
        let paths = [
            (canonical_parent, false),
            (root.to_owned(), false),
            (root.join("host-adapters"), true),
        ];
        let mut entries = Vec::new();
        for (path, private) in paths {
            let metadata = metadata_or_absent(&path)?;
            if let Some(m) = &metadata {
                if !safe_directory(m, private) {
                    return Err(refusal("registry", "unsafe state namespace"));
                }
                let file = open_no_follow(&path)?;
                let opened = file
                    .metadata()
                    .map_err(|_| refusal("registry", "state unavailable"))?;
                if !safe_directory(&opened, private) || !same_inode(m, &opened) {
                    return Err(refusal("registry", "state namespace changed"));
                }
            }
            entries.push((path, metadata, private));
        }
        Ok(Self { entries, parent })
    }
    fn check_existing(&self) -> Result<(), RegistryFailure> {
        let existing = Self {
            entries: self
                .entries
                .iter()
                .filter(|(_, m, _)| m.is_some())
                .cloned()
                .collect(),
            parent: self.parent.clone(),
        };
        existing.check()
    }
    fn check(&self) -> Result<(), RegistryFailure> {
        self.parent.check()?;
        for (path, before, private) in &self.entries {
            match (before, metadata_or_absent(path)?) {
                (None, None) => {}
                (Some(a), Some(b)) if safe_directory(&b, *private) && same_inode(a, &b) => {}
                _ => return Err(refusal("registry", "state namespace changed")),
            }
        }
        Ok(())
    }
}
fn read_private(path: &Path) -> Result<Option<(Vec<u8>, Metadata)>, RegistryFailure> {
    let Some(observed) = metadata_or_absent(path)? else {
        return Ok(None);
    };
    if !safe_file(&observed) || observed.len() > MAX_REGISTRY_BYTES {
        return Err(refusal("registry", "unsafe registry file"));
    }
    let mut file = open_no_follow(path)?;
    let before = file
        .metadata()
        .map_err(|_| refusal("registry", "state unavailable"))?;
    if !safe_file(&before) || !unchanged(&before, &observed) {
        return Err(refusal("registry", "registry file changed"));
    }
    let mut raw = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_REGISTRY_BYTES + 1)
        .read_to_end(&mut raw)
        .map_err(|_| refusal("registry", "registry read failed"))?;
    let after = file
        .metadata()
        .map_err(|_| refusal("registry", "state unavailable"))?;
    let final_path =
        fs::symlink_metadata(path).map_err(|_| refusal("registry", "registry file changed"))?;
    if raw.len() as u64 > MAX_REGISTRY_BYTES
        || !unchanged(&before, &after)
        || !unchanged(&after, &final_path)
    {
        return Err(refusal("registry", "registry file changed"));
    }
    Ok(Some((raw, after)))
}
fn verify_file_revision(path: &Path, expected: Option<&Metadata>) -> Result<(), RegistryFailure> {
    match (expected, metadata_or_absent(path)?) {
        (None, None) => Ok(()),
        (Some(before), Some(after)) if safe_file(&after) && unchanged(before, &after) => Ok(()),
        _ => Err(refusal("registry", "registry file changed")),
    }
}

fn ensure_owned_directory(path: &Path, private: bool) -> Result<(), RegistryFailure> {
    if metadata_or_absent(path)?.is_none() {
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder
            .create(path)
            .map_err(|_| refusal("registry", "state directory creation failed"))?;
    }
    let m = fs::symlink_metadata(path).map_err(|_| refusal("registry", "state unavailable"))?;
    if !safe_directory(&m, private) {
        return Err(refusal("registry", "unsafe state namespace"));
    }
    Ok(())
}
struct OwnedTemporary {
    path: PathBuf,
    metadata: Metadata,
    landed: bool,
}
impl OwnedTemporary {
    fn check(&self) -> Result<(), RegistryFailure> {
        let observed = fs::symlink_metadata(&self.path)
            .map_err(|_| refusal("registry", "temporary file changed"))?;
        if !safe_file(&observed) || !same_inode(&self.metadata, &observed) {
            return Err(refusal("registry", "temporary file changed"));
        }
        Ok(())
    }
}
impl Drop for OwnedTemporary {
    fn drop(&mut self) {
        if !self.landed && self.check().is_ok() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CommitBoundary {
    BeforeLegacyRootCreation,
    AfterLegacyRootCreation,
    AfterLifecycleCommit,
    BeforeLegacyLock,
    BeforePreparation,
    BeforeNoChange,
    BeforeDelete,
    BeforeReplace,
    AfterReplace,
    Readback,
}
fn storage_boundary(point: CommitBoundary) -> Result<(), RegistryFailure> {
    #[cfg(test)]
    {
        tests::boundary(point)
    }
    #[cfg(not(test))]
    {
        let _ = point;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lifecycle_refresh_refuses_identical_byte_directory_and_file_replacement() {
        for initially_absent in [false, true] {
            for component in ["root", "adapters", "catalog"] {
                let home = tempfile::tempdir().unwrap();
                let root = home.path().join("state");
                let contract = HostContract::load().unwrap();
                let registry = AdapterRegistry::new(root.clone(), contract.clone());
                if !initially_absent {
                    registry
                        .register(
                            fixture(&contract, "first"),
                            registry.read().unwrap().stamp(),
                        )
                        .unwrap();
                }
                let snapshot = registry.read().unwrap();
                let candidate = registration_candidate(
                    snapshot.document().cloned(),
                    fixture(&contract, "next"),
                )
                .unwrap();
                let expected = candidate.encode(&contract).unwrap();
                let changed_root = root.clone();
                let moved = home.path().join("retained-original");
                let retained = moved.clone();
                ACTION.with(|pending| {
                    *pending.borrow_mut() = Some((
                        CommitBoundary::AfterLifecycleCommit,
                        Box::new(move || {
                            let catalog = changed_root.join("host-adapters/registry.json");
                            let raw = fs::read(&catalog).unwrap();
                            let source = match component {
                                "root" => changed_root.clone(),
                                "adapters" => changed_root.join("host-adapters"),
                                _ => catalog.clone(),
                            };
                            fs::rename(&source, &moved).unwrap();
                            if component == "root" {
                                fs::create_dir(&changed_root).unwrap();
                                fs::set_permissions(
                                    &changed_root,
                                    fs::Permissions::from_mode(0o700),
                                )
                                .unwrap();
                            }
                            if component != "catalog" {
                                fs::create_dir(changed_root.join("host-adapters")).unwrap();
                                fs::set_permissions(
                                    changed_root.join("host-adapters"),
                                    fs::Permissions::from_mode(0o700),
                                )
                                .unwrap();
                            }
                            fs::write(&catalog, raw).unwrap();
                            fs::set_permissions(&catalog, fs::Permissions::from_mode(0o600))
                                .unwrap();
                            fs::write(changed_root.join("foreign"), b"retain replacement").unwrap();
                        }),
                    ))
                });
                let mut transaction = registry.begin_lifecycle(snapshot.stamp(), true).unwrap();
                assert_eq!(
                    transaction.commit(&candidate).unwrap_err().effect,
                    RegistryEffect::EffectUnconfirmed
                );
                assert_eq!(
                    fs::read(root.join("host-adapters/registry.json")).unwrap(),
                    expected
                );
                assert_eq!(
                    fs::read(root.join("foreign")).unwrap(),
                    b"retain replacement"
                );
                assert!(retained.exists());
                assert!(transaction.commit(&candidate).is_err());
                assert_eq!(
                    fs::read(root.join("host-adapters/registry.json")).unwrap(),
                    expected
                );
            }
        }
    }

    #[test]
    fn legacy_wrapper_qualifies_target_drift_after_owned_root_creation() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("state");
        let target = home.path().join("settings.json");
        fs::write(&target, b"{}").unwrap();
        let changed = target.clone();
        ACTION.with(|pending| {
            *pending.borrow_mut() = Some((
                CommitBoundary::AfterLegacyRootCreation,
                Box::new(move || fs::write(changed, b"{\"foreign\":true}").unwrap()),
            ))
        });
        let lifecycle = super::super::config_lifecycle::ConfigLifecycle::new(
            root.clone(),
            HostContract::load().unwrap(),
        );
        let called = std::cell::Cell::new(false);
        let failure = lifecycle
            .run_legacy_claude(&target, true, |_| {
                called.set(true);
            })
            .unwrap_err();
        assert_eq!(failure.effect, RegistryEffect::EffectUnconfirmed);
        assert!(!called.get());
        assert!(root.is_dir());
        assert_eq!(fs::read(&target).unwrap(), b"{\"foreign\":true}");
        assert!(!root.join("install.json").exists());
        assert!(!root.join("host-adapters").exists());
    }

    #[test]
    fn legacy_root_preparation_advances_only_its_owned_leaf_and_retains_creation_proof() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("state");
        let registry = AdapterRegistry::new(root.clone(), HostContract::load().unwrap());
        let snapshot = registry.read().unwrap();
        let mut transaction = registry.begin_legacy(snapshot.stamp(), true).unwrap();
        transaction.prepare_legacy_marker_root().unwrap();
        assert_eq!(fs::metadata(&root).unwrap().mode() & 0o777, 0o700);
        fs::write(root.join("install.json"), b"owned legacy marker").unwrap();
        transaction.verify().unwrap();
        assert!(!root.join("host-adapters").exists());
        assert_eq!(*registry.read().unwrap().stamp(), *snapshot.stamp());
        let moved = home.path().join("moved");
        fs::rename(&root, &moved).unwrap();
        fs::create_dir(&root).unwrap();
        fs::write(root.join("foreign"), b"keep").unwrap();
        assert!(transaction.verify().is_err());
        assert_eq!(fs::read(root.join("foreign")).unwrap(), b"keep");
        assert_eq!(
            fs::read(moved.join("install.json")).unwrap(),
            b"owned legacy marker"
        );
    }

    #[test]
    fn raced_legacy_root_appearance_is_not_adopted_or_deleted() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("state");
        let registry = AdapterRegistry::new(root.clone(), HostContract::load().unwrap());
        let snapshot = registry.read().unwrap();
        let mut transaction = registry.begin_legacy(snapshot.stamp(), true).unwrap();
        let appeared = root.clone();
        ACTION.with(|pending| {
            *pending.borrow_mut() = Some((
                CommitBoundary::BeforeLegacyRootCreation,
                Box::new(move || {
                    fs::create_dir(&appeared).unwrap();
                    fs::write(appeared.join("foreign"), b"keep").unwrap();
                }),
            ))
        });
        assert!(transaction.prepare_legacy_marker_root().is_err());
        assert_eq!(fs::read(root.join("foreign")).unwrap(), b"keep");
        assert!(!root.join("install.json").exists());
        assert!(!root.join("host-adapters").exists());
        assert!(transaction.verify().is_err());
    }

    #[test]
    fn legacy_root_preparation_preserves_existing_mode_and_refuses_later_catalog_appearance() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("state");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        let registry = AdapterRegistry::new(root.clone(), HostContract::load().unwrap());
        let snapshot = registry.read().unwrap();
        let mut transaction = registry.begin_legacy(snapshot.stamp(), true).unwrap();
        transaction.prepare_legacy_marker_root().unwrap();
        assert_eq!(fs::metadata(&root).unwrap().mode() & 0o777, 0o755);
        fs::create_dir(root.join("host-adapters")).unwrap();
        fs::set_permissions(
            root.join("host-adapters"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        fs::write(root.join("host-adapters/foreign"), b"keep").unwrap();
        assert!(transaction.verify().is_err());
        assert_eq!(
            fs::read(root.join("host-adapters/foreign")).unwrap(),
            b"keep"
        );
    }

    #[test]
    fn lifecycle_refresh_refuses_valid_replacement_instead_of_adopting_it() {
        for increment_revision in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().join("state");
            let contract = HostContract::load().unwrap();
            let registry = AdapterRegistry::new(root.clone(), contract.clone());
            registry
                .register(
                    fixture(&contract, "first"),
                    registry.read().unwrap().stamp(),
                )
                .unwrap();
            let snapshot = registry.read().unwrap();
            let mut candidate = snapshot.document().unwrap().clone();
            candidate.revision += 1;
            let mut replaced = candidate.clone();
            if increment_revision {
                replaced.revision += 1;
            }
            replaced.adapters.insert(
                "foreign".into(),
                super::super::catalog::RegistryRecord {
                    manifest: fixture(&contract, "foreign"),
                    registration_revision: replaced.revision,
                    trust_revision: 0,
                    trust: None,
                },
            );
            let replacement = replaced.encode(&contract).unwrap();
            let path = root.join("host-adapters/registry.json");
            let later_path = path.clone();
            let later_bytes = replacement.clone();
            ACTION.with(|pending| {
                *pending.borrow_mut() = Some((
                    CommitBoundary::AfterLifecycleCommit,
                    Box::new(move || fs::write(later_path, later_bytes).unwrap()),
                ))
            });
            let mut transaction = registry.begin_lifecycle(snapshot.stamp(), false).unwrap();
            assert_eq!(
                transaction.commit(&candidate).unwrap_err().effect,
                RegistryEffect::EffectUnconfirmed
            );
            assert_eq!(fs::read(&path).unwrap(), replacement);
            assert!(transaction.commit(&candidate).is_err());
            assert_eq!(fs::read(&path).unwrap(), replacement);
        }
    }
    use std::cell::{Cell, RefCell};
    use std::os::unix::fs::PermissionsExt;
    thread_local! { static FAILURE: Cell<Option<CommitBoundary>> = const { Cell::new(None) }; }

    type BoundaryAction = (CommitBoundary, Box<dyn FnOnce()>);
    thread_local! { static ACTION: RefCell<Option<BoundaryAction>> = const { RefCell::new(None) }; }

    pub(super) fn boundary(point: CommitBoundary) -> Result<(), RegistryFailure> {
        let action = ACTION.with(|pending| {
            let mut pending = pending.borrow_mut();
            if pending.as_ref().is_some_and(|(at, _)| *at == point) {
                pending.take()
            } else {
                None
            }
        });
        if let Some((_, action)) = action {
            action();
        }
        if FAILURE.get() == Some(point) {
            FAILURE.set(None);
            Err(refusal("registry", "injected storage failure"))
        } else {
            Ok(())
        }
    }
    fn fixture(contract: &HostContract, id: &str) -> super::super::contract::ValidatedManifest {
        let mut value: serde_json::Value = serde_json::from_str(include_str!(
            "../../../protocol/contracts/host-adapter/v1/fixtures/valid-manifest-synthetic.json"
        ))
        .unwrap();
        value["adapter_id"] = serde_json::json!(id);
        contract
            .validate_manifest(&serde_json::to_vec(&value).unwrap())
            .unwrap()
    }
    #[test]
    fn pre_replace_failure_preserves_old_bytes_and_post_replace_failure_retains_new_document() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let contract = HostContract::load().unwrap();
        let registry = AdapterRegistry::new(root.clone(), contract.clone());
        registry
            .register(
                fixture(&contract, "first"),
                registry.read().unwrap().stamp(),
            )
            .unwrap();
        let path = root.join("host-adapters/registry.json");
        let before = fs::read(&path).unwrap();
        let inode = fs::metadata(&path).unwrap().ino();
        FAILURE.set(Some(CommitBoundary::BeforeReplace));
        let error = registry
            .register(
                fixture(&contract, "second"),
                registry.read().unwrap().stamp(),
            )
            .unwrap_err();
        assert_eq!(error.effect, RegistryEffect::NoChange);
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        FAILURE.set(Some(CommitBoundary::AfterReplace));
        let error = registry
            .register(
                fixture(&contract, "second"),
                registry.read().unwrap().stamp(),
            )
            .unwrap_err();
        assert_eq!(error.effect, RegistryEffect::EffectUnconfirmed);
        let after = registry.read().unwrap();
        assert_eq!(after.document().unwrap().revision(), 2);
        assert!(after.document().unwrap().adapters().contains_key("second"));
        assert_ne!(fs::read(&path).unwrap(), before);
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }
    #[test]
    fn readback_failure_retains_committed_state_without_stale_restore() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let contract = HostContract::load().unwrap();
        let registry = AdapterRegistry::new(root, contract.clone());
        FAILURE.set(Some(CommitBoundary::Readback));
        let error = registry
            .register(
                fixture(&contract, "first"),
                registry.read().unwrap().stamp(),
            )
            .unwrap_err();
        assert_eq!(error.effect, RegistryEffect::EffectUnconfirmed);
        assert!(registry
            .read()
            .unwrap()
            .document()
            .unwrap()
            .adapters()
            .contains_key("first"));
    }
    #[test]
    fn legacy_deletion_refuses_any_adapter_entry_and_preserves_outside_reservation() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(root.join("ledger.sqlite3"), b"old-ledger").unwrap();
        let target = reservation_target(&root).unwrap();
        drop(crate::write_lock::acquire_private(&target).unwrap());
        let lock = crate::write_lock::derive_lock_path(&target).unwrap();
        let inode = fs::metadata(&lock).unwrap().ino();
        fs::write(root.join("host-adapters"), b"foreign-or-corrupt-entry").unwrap();
        assert!(delete_legacy_state(&root).is_err());
        assert_eq!(
            fs::read(root.join("ledger.sqlite3")).unwrap(),
            b"old-ledger"
        );
        assert_eq!(fs::metadata(&lock).unwrap().ino(), inode);
        fs::remove_file(root.join("host-adapters")).unwrap();
        delete_legacy_state(&root).unwrap();
        assert!(!root.exists());
        assert_eq!(fs::metadata(&lock).unwrap().ino(), inode);
        delete_legacy_state(&root).unwrap();
    }
    #[test]
    fn deletion_boundary_refuses_new_adapter_entry_or_replaced_reservation() {
        for replace_reservation in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().join("state");
            fs::create_dir(&root).unwrap();
            fs::write(root.join("ledger.sqlite3"), b"preserved-ledger").unwrap();
            let lock =
                crate::write_lock::derive_lock_path(&reservation_target(&root).unwrap()).unwrap();
            let changed_root = root.clone();
            let changed_lock = lock.clone();
            ACTION.with(|pending| {
                *pending.borrow_mut() = Some((
                    CommitBoundary::BeforeDelete,
                    Box::new(move || {
                        if replace_reservation {
                            fs::rename(&changed_lock, changed_lock.with_extension("old-lock"))
                                .unwrap();
                            fs::write(&changed_lock, b"replacement-lock").unwrap();
                            fs::set_permissions(&changed_lock, fs::Permissions::from_mode(0o600))
                                .unwrap();
                        } else {
                            fs::create_dir(changed_root.join("host-adapters")).unwrap();
                            fs::set_permissions(
                                changed_root.join("host-adapters"),
                                fs::Permissions::from_mode(0o700),
                            )
                            .unwrap();
                        }
                    }),
                ))
            });
            assert!(delete_legacy_state(&root).is_err());
            assert_eq!(
                fs::read(root.join("ledger.sqlite3")).unwrap(),
                b"preserved-ledger"
            );
            assert!(root.exists());
        }
    }

    #[test]
    fn confirmed_trust_noop_rechecks_intervening_revocation_without_restoring_it() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let contract = HostContract::load().unwrap();
        let registry = AdapterRegistry::new(root.clone(), contract.clone());
        let program = directory.path().join("inert-native");
        fs::write(&program, b"measured-but-never-executed").unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        let mut value = fixture(&contract, "first").value().clone();
        value["launch"] = serde_json::json!({"executable":program,"argv":[]});
        value["runtime_files"] = serde_json::json!([{"path":program,"kind":"entrypoint","digest":digest(b"measured-but-never-executed")}]);
        let manifest = contract
            .validate_manifest(&serde_json::to_vec(&value).unwrap())
            .unwrap();
        registry
            .register(manifest.clone(), registry.read().unwrap().stamp())
            .unwrap();
        let review = registry.review_trust("first").unwrap();
        registry
            .confirm_trust(
                "first",
                manifest.digest(),
                review.confirmation_digest(),
                registry.read().unwrap().stamp(),
            )
            .unwrap();
        let path = root.join("host-adapters/registry.json");
        let granted = fs::read(&path).unwrap();
        let stamp = registry.read().unwrap().stamp().clone();
        assert_eq!(
            registry
                .confirm_trust(
                    "first",
                    manifest.digest(),
                    review.confirmation_digest(),
                    &stamp
                )
                .unwrap(),
            RegistryEffect::NoChange
        );
        assert_eq!(fs::read(&path).unwrap(), granted);
        let mut revoked: serde_json::Value = serde_json::from_slice(&granted).unwrap();
        revoked["revision"] = serde_json::json!(3);
        revoked["adapters"]["first"]["trust"] = serde_json::Value::Null;
        revoked["adapters"]["first"]["trust_revision"] = serde_json::json!(3);
        let revoked = serde_json::to_vec(&revoked).unwrap();
        let external_bytes = revoked.clone();
        let changed_path = path.clone();
        ACTION.with(|pending| {
            *pending.borrow_mut() = Some((
                CommitBoundary::BeforeNoChange,
                Box::new(move || fs::write(changed_path, external_bytes).unwrap()),
            ))
        });
        let error = registry
            .confirm_trust(
                "first",
                manifest.digest(),
                review.confirmation_digest(),
                &stamp,
            )
            .unwrap_err();
        assert_eq!(error.effect, RegistryEffect::NoChange);
        assert_eq!(fs::read(&path).unwrap(), revoked);
        assert!(
            registry.read().unwrap().document().unwrap().adapters()["first"]
                .trust()
                .is_none()
        );
    }
    #[test]
    fn concurrent_prepared_first_registrants_share_one_guard_and_stale_cas() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("missing/parents/state");
        let contract = HostContract::load().unwrap();
        let expected = AdapterRegistry::new(root.clone(), contract.clone())
            .read()
            .unwrap()
            .stamp()
            .clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let mut resumes = Vec::new();
        let mut workers = Vec::new();
        for id in ["first", "second"] {
            let registry = AdapterRegistry::new(root.clone(), contract.clone());
            let manifest = fixture(&contract, id);
            let stamp = expected.clone();
            let ready = ready_tx.clone();
            let (resume_tx, resume_rx) = std::sync::mpsc::channel();
            resumes.push(resume_tx);
            workers.push(std::thread::spawn(move || {
                ACTION.with(|pending| {
                    *pending.borrow_mut() = Some((
                        CommitBoundary::BeforePreparation,
                        Box::new(move || {
                            ready.send(()).unwrap();
                            resume_rx
                                .recv_timeout(std::time::Duration::from_secs(5))
                                .unwrap();
                        }),
                    ))
                });
                registry.register(manifest, &stamp)
            }));
        }
        for _ in 0..2 {
            ready_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
        }
        for resume in resumes {
            resume.send(()).unwrap();
        }
        let results: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(
            results
                .iter()
                .filter(|result| result.as_ref().ok() == Some(&RegistryEffect::AppliedVerified))
                .count(),
            1
        );
        let loser = results
            .iter()
            .find_map(|result| result.as_ref().err())
            .unwrap();
        assert_eq!(loser.reason, "registry changed");
        assert_eq!(loser.effect, RegistryEffect::NoChange);
        let snapshot = AdapterRegistry::new(root, contract).read().unwrap();
        assert_eq!(snapshot.document().unwrap().revision(), 1);
        assert_eq!(snapshot.document().unwrap().adapters().len(), 1);
    }
    #[test]
    fn legacy_deletion_does_not_adopt_replacement_root_after_preflight() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("ledger.sqlite3"), b"original-ledger").unwrap();
        let changed_root = root.clone();
        let old_root = directory.path().join("previous-state");
        let retained_root = old_root.clone();
        ACTION.with(|pending| {
            *pending.borrow_mut() = Some((
                CommitBoundary::BeforeLegacyLock,
                Box::new(move || {
                    fs::rename(&changed_root, &old_root).unwrap();
                    fs::create_dir(&changed_root).unwrap();
                    fs::write(changed_root.join("ledger.sqlite3"), b"replacement-ledger").unwrap();
                }),
            ))
        });
        assert!(delete_legacy_state(&root).is_err());
        assert_eq!(
            fs::read(root.join("ledger.sqlite3")).unwrap(),
            b"replacement-ledger"
        );
        assert_eq!(
            fs::read(retained_root.join("ledger.sqlite3")).unwrap(),
            b"original-ledger"
        );
    }
    #[test]
    fn relative_locator_is_captured_once_even_when_cwd_later_changes() {
        const CHILD: &str = "LIBRA_REGISTRY_CWD_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "host_runtime::state::tests::relative_locator_is_captured_once_even_when_cwd_later_changes", "--nocapture"])
                .env(CHILD, "1").output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        // Isolate process-global cwd changes from the normal parallel test process.
        let original_cwd = std::env::current_dir().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let first_cwd = directory.path().join("first");
        let later_cwd = directory.path().join("other/second");
        fs::create_dir(&first_cwd).unwrap();
        fs::create_dir_all(&later_cwd).unwrap();
        std::env::set_current_dir(&first_cwd).unwrap();
        let contract = HostContract::load().unwrap();
        let registry = AdapterRegistry::new(PathBuf::from("../state"), contract.clone());
        std::env::set_current_dir(&later_cwd).unwrap();
        registry
            .register(
                fixture(&contract, "captured"),
                registry.read().unwrap().stamp(),
            )
            .unwrap();
        assert!(directory
            .path()
            .join("state/host-adapters/registry.json")
            .is_file());
        assert!(!directory.path().join("other/state").exists());
        let disappearing_cwd = directory.path().join("disappearing");
        fs::create_dir(&disappearing_cwd).unwrap();
        std::env::set_current_dir(&disappearing_cwd).unwrap();
        fs::remove_dir(&disappearing_cwd).unwrap();
        let unavailable = AdapterRegistry::new(PathBuf::from("new-state"), contract);
        std::env::set_current_dir(&original_cwd).unwrap();
        assert!(unavailable.read().is_err());
        assert!(!directory.path().join("new-state").exists());
    }
    #[test]
    fn legacy_delete_waiting_for_registration_refuses_without_removing_ledger_or_catalog() {
        use std::time::Duration;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("ledger.sqlite3"), b"retained-ledger").unwrap();
        let contract = HostContract::load().unwrap();
        let registry = AdapterRegistry::new(root.clone(), contract.clone());
        let manifest = fixture(&contract, "first");
        let expected = registry.read().unwrap().stamp().clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            ACTION.with(|pending| {
                *pending.borrow_mut() = Some((
                    CommitBoundary::BeforeReplace,
                    Box::new(move || {
                        ready_tx.send(()).unwrap();
                        resume_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    }),
                ))
            });
            registry.register(manifest, &expected)
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let deletion_root = root.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let deletion =
            std::thread::spawn(move || done_tx.send(delete_legacy_state(&deletion_root)).unwrap());
        assert!(matches!(
            done_rx.recv_timeout(Duration::from_millis(30)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        resume_tx.send(()).unwrap();
        assert_eq!(
            writer.join().unwrap().unwrap(),
            RegistryEffect::AppliedVerified
        );
        assert!(done_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .is_err());
        deletion.join().unwrap();
        assert_eq!(
            fs::read(root.join("ledger.sqlite3")).unwrap(),
            b"retained-ledger"
        );
        assert!(AdapterRegistry::new(root, contract)
            .read()
            .unwrap()
            .document()
            .unwrap()
            .adapters()
            .contains_key("first"));
    }

    #[test]
    fn maximum_escaped_initial_manifest_preview_is_passive_and_real_record_fits_catalog() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let contract = HostContract::load().unwrap();
        let registry = AdapterRegistry::new(root.clone(), contract.clone());
        let mut raw = fixture(&contract, "maximum").raw().to_vec();
        raw.resize(65_536, b'\t');
        let manifest = contract.validate_manifest(&raw).unwrap();
        let absent = registry.read().unwrap();
        registry.check_registration(&absent, &manifest).unwrap();
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
        assert_eq!(
            registry.register(manifest, absent.stamp()).unwrap(),
            RegistryEffect::AppliedVerified
        );
        let snapshot = registry.read().unwrap();
        assert_eq!(
            snapshot.document().unwrap().adapters()["maximum"]
                .manifest()
                .raw(),
            raw
        );
        assert!(
            fs::read(root.join("host-adapters/registry.json"))
                .unwrap()
                .len()
                < 8 * 1024 * 1024
        );
    }
    #[test]
    fn prospective_registration_refuses_encoded_byte_overflow_of_a_valid_existing_catalog() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let contract = HostContract::load().unwrap();
        let registry = AdapterRegistry::new(root.clone(), contract.clone());
        let mut document = RegistryDocument {
            schema_version: 1,
            registry_id: "a".repeat(32),
            revision: 64,
            adapters: std::collections::BTreeMap::new(),
            installations: std::collections::BTreeMap::new(),
        };
        for index in 0..64 {
            let id = format!("adapter_{index:04}");
            let mut raw = fixture(&contract, &id).raw().to_vec();
            raw.resize(65_536, b'\t');
            let manifest = contract.validate_manifest(&raw).unwrap();
            document.adapters.insert(
                id,
                super::super::catalog::RegistryRecord {
                    manifest,
                    registration_revision: index + 1,
                    trust_revision: 0,
                    trust: None,
                },
            );
        }
        let raw = document.encode(&contract).unwrap();
        assert!(raw.len() <= 8 * 1024 * 1024);
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let child = root.join("host-adapters");
        fs::create_dir(&child).unwrap();
        fs::set_permissions(&child, fs::Permissions::from_mode(0o700)).unwrap();
        let path = child.join("registry.json");
        fs::write(&path, &raw).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let snapshot = registry.read().unwrap();
        assert_eq!(snapshot.document().unwrap().adapters().len(), 64);
        let mut proposed = fixture(&contract, "additional").raw().to_vec();
        proposed.resize(65_536, b'\t');
        let manifest = contract.validate_manifest(&proposed).unwrap();
        let error = registry
            .check_registration(&snapshot, &manifest)
            .unwrap_err();
        assert_eq!((error.stage, error.reason), ("registry", "input too large"));
        assert_eq!(fs::read(&path).unwrap(), raw);
        let error = registry.register(manifest, snapshot.stamp()).unwrap_err();
        assert_eq!((error.stage, error.reason), ("registry", "input too large"));
        assert_eq!(error.effect, RegistryEffect::NoChange);
        assert_eq!(fs::read(&path).unwrap(), raw);
    }
}
