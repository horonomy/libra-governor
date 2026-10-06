//! The concrete installed Claude advisory consumer. Local lifecycle state is
//! never a host acknowledgement or canonical execution/economic attribution.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::catalog::RegistryDocument;
use super::config_io::{FileObservation, LockedTarget};
use super::config_profile::{
    self, ConfigIntent, ConfigSlot, DesiredSlot, ProfileRequest, SlotAction, SlotObserved,
    SlotRequest,
};
use super::config_record::{
    uuid_valid, ConnectionEvidence, FileDelta, FileFingerprint, InstallationContext,
    InstallationRecord, IntentPhase, PendingIntent, ProductBinary, TargetDelta,
};
use super::config_settings::{self, CallbackPresence, OwnedCallback};
use super::contract::HostContract;
use super::dispatch::{
    Cancellation, DiagnosticDispatcher, DiagnosticFailure, DiagnosticScope, DiagnosticSelection,
};
use super::identity::product_executable_digest;
use super::state::{AdapterRegistry, LifecycleTransaction, RegistrySnapshot};
use super::{RegistryEffect, RegistryFailure};

pub const CLAUDE_PROFILE: &str = super::config_bundle::PROFILE_ID;
const MAX_REVISION: u64 = 9_007_199_254_740_991;

fn fail(reason: &'static str) -> RegistryFailure {
    RegistryFailure::new("lifecycle", reason)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LifecycleBoundary {
    AfterInstallIntent,
    AfterArtifactCreation,
    BeforeInstallCompletion,
    AfterPassiveEnabledObservation,
    AfterFinalConnectionCatalogVerification,
    BeforeFinalAdmission,
    AfterGateClosed,
    AfterConnectionIntent,
    AfterTargetReplacement,
    BeforeConnectionCompletion,
    BeforePlanReturn,
    BeforeInspectionReturn,
}

fn lifecycle_boundary(point: LifecycleBoundary) -> Result<(), RegistryFailure> {
    #[cfg(test)]
    tests::action(point);
    #[cfg(test)]
    if tests::INTERRUPTION.with(|selected| selected.get() == Some(point)) {
        return Err(fail("lifecycle_interrupted"));
    }
    let _ = point;
    Ok(())
}

fn advance(document: &mut RegistryDocument) -> Result<u64, RegistryFailure> {
    let revision = document
        .revision
        .checked_add(1)
        .filter(|n| *n <= MAX_REVISION)
        .ok_or_else(|| fail("revision_limit"))?;
    document.revision = revision;
    Ok(revision)
}

fn locator(path: &Path) -> Result<String, RegistryFailure> {
    path.to_str()
        .filter(|path| path.len() <= 4096)
        .map(str::to_owned)
        .ok_or_else(|| fail("invalid_lifecycle_context"))
}

/// Paths are captured locally by the operator surface, never by an adapter plan.
pub struct LocalInstallation {
    pub scope: String,
    pub profile: String,
    pub target: PathBuf,
    pub binary: PathBuf,
}

#[derive(Clone, Debug, Serialize)]
pub struct LocalLifecycleResult {
    pub installed: bool,
    pub desired_enabled: bool,
    pub local_connection: &'static str,
    pub effect: &'static str,
    pub native_verification: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery: Option<RecoveryOutcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<LocalLifecyclePreview>,
}

#[derive(Clone, Debug, Serialize)]
pub struct LocalLifecyclePreview {
    pub artifact_action: &'static str,
    pub add_callbacks: usize,
    pub remove_callbacks: usize,
    pub preserve_slots: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryOutcome {
    OperationNotApplied,
}

impl LocalLifecycleResult {
    fn from_record(record: Option<&InstallationRecord>, effect: RegistryEffect) -> Self {
        Self {
            installed: record.is_some_and(|record| record.installed),
            desired_enabled: record.is_some_and(|record| record.desired_enabled),
            local_connection: if record.is_some_and(|record| record.pending.is_some()) {
                "pending"
            } else if record.is_some_and(|record| record.desired_enabled) {
                "connected"
            } else {
                "disconnected"
            },
            effect: match effect {
                RegistryEffect::NoChange => "no_change",
                RegistryEffect::AppliedVerified => "applied_verified",
                RegistryEffect::EffectUnconfirmed => "effect_unconfirmed",
            },
            native_verification: "unverified",
            recovery: None,
            preview: None,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct LocalInstallationStatus {
    pub profile: &'static str,
    pub installed: bool,
    pub desired_enabled: bool,
    pub local_connection: &'static str,
    pub integrity: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_operation: Option<ConfigIntent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    pub native_verification: &'static str,
    pub host_trust: &'static str,
    pub observed_execution: &'static str,
    pub effective_support: &'static str,
}

/// One passive product observation. Its metadata owns no descriptors and
/// cannot authorize execution, configuration mutation or economic effects.
pub struct PassiveAdapterInspection {
    snapshot: RegistrySnapshot,
    installations: Vec<(String, LocalInstallationStatus)>,
}

impl PassiveAdapterInspection {
    pub fn snapshot(&self) -> &RegistrySnapshot {
        &self.snapshot
    }

    pub fn local_installations<'a>(
        &'a self,
        selected: Option<&'a str>,
    ) -> impl Iterator<Item = &'a LocalInstallationStatus> + 'a {
        self.installations
            .iter()
            .filter(move |(id, _)| selected.is_none_or(|selected| selected == id))
            .map(|(_, status)| status)
    }
}

pub struct ConfigLifecycle {
    registry: AdapterRegistry,
    contract: HostContract,
}

#[derive(Clone, Debug, Serialize)]
pub struct ConfigPlanSlotPreview {
    pub slot: ConfigSlot,
    pub action: SlotAction,
}

/// A redacted observation, never an apply token or installation authority.
#[derive(Clone, Debug, Serialize)]
pub struct ConfigPlanPreview {
    pub profile: &'static str,
    pub consumer_adapter_id: &'static str,
    pub planned_intent: ConfigIntent,
    pub plan_validation: &'static str,
    pub configuration_effect: &'static str,
    pub configuration_change_required: Option<bool>,
    pub apply_available: bool,
    pub execution_attempted: bool,
    pub filesystem_effect: &'static str,
    pub native_verification: &'static str,
    pub slots: Vec<ConfigPlanSlotPreview>,
}

fn preview_failure(failure: RegistryFailure) -> DiagnosticFailure {
    DiagnosticFailure {
        stage: "profile",
        reason: failure.reason,
        execution_attempted: false,
    }
}

impl ConfigLifecycle {
    /// Construction is passive and does not initialize any state or lock.
    pub fn new(state_root: PathBuf, contract: HostContract) -> Self {
        Self {
            registry: AdapterRegistry::new(state_root, contract.clone()),
            contract,
        }
    }

    /// Execute an explicitly trusted planner only for a validated preview of
    /// the existing consumer. Trusted code retains ambient authority.
    pub fn preview_registered_plan(
        &self,
        planner_id: &str,
        selection: &LocalInstallation,
        intent: ConfigIntent,
        dry_run: bool,
        cancellation: &Cancellation,
    ) -> Result<ConfigPlanPreview, DiagnosticFailure> {
        let local = || -> Result<_, RegistryFailure> {
            if intent == ConfigIntent::Install {
                return Err(fail("unsupported_planning_intent"));
            }
            let context = self.context("claude_code", selection)?;
            let witness = self.registry.observe_passively()?;
            let snapshot = witness.snapshot();
            let document = snapshot
                .document()
                .ok_or_else(|| fail("installation_unavailable"))?;
            let (binding, record) = document
                .installations
                .iter()
                .next()
                .ok_or_else(|| fail("installation_unavailable"))?;
            if !record.installed || record.context != context {
                return Err(fail("installation_context_changed"));
            }
            if record.pending.is_some() {
                return Err(fail("pending_operation"));
            }
            if intent == ConfigIntent::Uninstall && record.desired_enabled {
                return Err(fail("disable_required"));
            }
            verify_package(record, &selection.binary)?;
            let artifact = verify_artifact(document, binding, record)?;
            let target = FileObservation::capture(&selection.target, false)?;
            let owned = observe_owned(binding, record, &target)?;
            if owned != record.desired_enabled {
                return Err(fail("connection_changed"));
            }
            let request = connection_request(document, binding, record, intent, &target, owned)?;
            let value = super::config_bundle::validate_request_value(&request)?;
            Ok((witness, snapshot, artifact, target, request, value))
        };
        let (witness, snapshot, artifact, target, request, request_value) =
            local().map_err(preview_failure)?;
        let document = snapshot.document().unwrap();
        let (binding, record) = document.installations.iter().next().unwrap();
        let check = || -> Result<(), DiagnosticFailure> {
            (|| {
                witness.check()?;
                verify_package(record, &selection.binary)?;
                artifact.check()?;
                target.check()?;
                witness.check()?;
                artifact.check()?;
                target.check()
            })()
            .map_err(preview_failure)
        };
        let dispatcher = DiagnosticDispatcher::new(
            AdapterRegistry::new(
                self.registry.root().map_err(preview_failure)?.to_path_buf(),
                self.contract.clone(),
            ),
            self.contract.clone(),
        );
        let prepared = dispatcher.prepare_config_plan(
            planner_id,
            DiagnosticSelection {
                scope: DiagnosticScope::User,
                configuration: b"{}".to_vec(),
            },
            "claude_code",
            CLAUDE_PROFILE,
            serde_json::to_value(super::config_bundle::validator_ref())
                .map_err(|_| preview_failure(fail("validator_ref_encoding_refused")))?,
            request_value,
        )?;
        dispatcher.check_config_plan(&prepared, cancellation)?;
        check()?;
        if dry_run {
            let preview = ConfigPlanPreview {
                profile: CLAUDE_PROFILE,
                consumer_adapter_id: "claude_code",
                planned_intent: intent,
                plan_validation: "external_plan_required",
                configuration_effect: "not_applied",
                configuration_change_required: None,
                apply_available: false,
                execution_attempted: false,
                filesystem_effect: "unchanged",
                native_verification: "unverified",
                slots: Vec::new(),
            };
            lifecycle_boundary(LifecycleBoundary::BeforePlanReturn).map_err(preview_failure)?;
            #[cfg(test)]
            let prepared = if tests::EXPIRE_PLAN.with(|flag| flag.get()) {
                dispatcher.expire_config_plan_for_test(prepared)
            } else {
                prepared
            };
            dispatcher.finish_config_plan(&prepared, cancellation)?;
            return Ok(preview);
        }
        let candidate = dispatcher.plan_config_registered(&prepared, cancellation, check)?;
        let attempted = |mut error: DiagnosticFailure| {
            error.execution_attempted = true;
            error
        };
        let plan = super::config_bundle::validate_external_plan(&request, &candidate)
            .map_err(preview_failure)
            .map_err(attempted)?;
        // Consume the returned, independently validated plan through the same
        // renderer as builtin mutations, without invoking its writer.
        let rendered = render_connection_plan(binding, record, intent, &target, &plan)
            .map_err(preview_failure)
            .map_err(attempted)?;
        let configuration_change_required = rendered.raw.as_deref() != target.raw();
        let slots = plan
            .changes()
            .iter()
            .map(|change| ConfigPlanSlotPreview {
                slot: change.slot,
                action: change.action,
            })
            .collect();
        dispatcher
            .check_config_plan(&prepared, cancellation)
            .map_err(attempted)?;
        check().map_err(attempted)?;
        let preview = ConfigPlanPreview {
            profile: CLAUDE_PROFILE,
            consumer_adapter_id: "claude_code",
            planned_intent: intent,
            plan_validation: "validated",
            configuration_effect: "not_applied",
            configuration_change_required: Some(configuration_change_required),
            apply_available: false,
            execution_attempted: true,
            filesystem_effect: "not_asserted",
            native_verification: "unverified",
            slots,
        };
        lifecycle_boundary(LifecycleBoundary::BeforePlanReturn)
            .map_err(preview_failure)
            .map_err(attempted)?;
        #[cfg(test)]
        let prepared = if tests::EXPIRE_PLAN.with(|flag| flag.get()) {
            dispatcher.expire_config_plan_for_test(prepared)
        } else {
            prepared
        };
        dispatcher
            .finish_config_plan(&prepared, cancellation)
            .map_err(attempted)?;
        Ok(preview)
    }

    fn context(
        &self,
        adapter_id: &str,
        selection: &LocalInstallation,
    ) -> Result<InstallationContext, RegistryFailure> {
        if adapter_id != "claude_code"
            || selection.scope != "user"
            || selection.profile != CLAUDE_PROFILE
        {
            return Err(fail("unsupported_installation_profile"));
        }
        Ok(InstallationContext {
            scope: selection.scope.clone(),
            state_root: locator(self.registry.root()?)?,
            target_path: locator(&selection.target)?,
        })
    }

    pub fn install(
        &self,
        adapter_id: &str,
        selection: &LocalInstallation,
        dry_run: bool,
    ) -> Result<LocalLifecycleResult, RegistryFailure> {
        let context = self.context(adapter_id, selection)?;
        let binary = ProductBinary {
            path: locator(&selection.binary)?,
            sha256: product_executable_digest(&selection.binary)?,
        };
        let snapshot = self.registry.read()?;
        let mut document = snapshot
            .document()
            .cloned()
            .unwrap_or_else(|| RegistryDocument {
                schema_version: 2,
                registry_id: uuid::Uuid::new_v4().simple().to_string(),
                revision: 0,
                adapters: BTreeMap::new(),
                installations: BTreeMap::new(),
            });
        if let Some((binding, record)) = document.installations.iter().next() {
            if record.context != context || record.binary != binary {
                return Err(fail("installation_context_changed"));
            }
            if record.pending.is_some() {
                if dry_run {
                    return Err(fail("pending_operation"));
                }
                return self.reconcile_install(&snapshot, binding, record);
            }
            let artifact = verify_artifact(&document, binding, record)?;
            let target = record
                .desired_enabled
                .then(|| verify_connected(binding, record))
                .transpose()?;
            artifact.check()?;
            if let Some(target) = &target {
                target.check()?;
            }
            if !dry_run {
                let transaction = self.registry.begin_admission(snapshot.stamp())?;
                transaction.verify()?;
                artifact.check()?;
                if let Some(target) = &target {
                    target.check()?;
                }
                transaction.verify()?;
                artifact.check()?;
                if let Some(target) = &target {
                    target.check()?;
                }
            }
            let mut result =
                LocalLifecycleResult::from_record(Some(record), RegistryEffect::NoChange);
            if dry_run {
                result.preview = Some(LocalLifecyclePreview {
                    artifact_action: "preserve",
                    add_callbacks: 0,
                    remove_callbacks: 0,
                    preserve_slots: 3,
                });
            }
            return Ok(result);
        }
        document.schema_version = 2;
        let revision = advance(&mut document)?;
        let (binding, record) =
            InstallationRecord::prepare_install(&document.registry_id, revision, context, binary)?;
        let bytes = record.artifact_bytes(&document.registry_id, &binding)?;
        let artifact_path = artifact_path(self.registry.root()?, &record);
        let before = FileObservation::capture(&artifact_path, true)?;
        if before.raw().is_some() {
            return Err(fail("artifact_already_exists"));
        }
        document.installations.insert(binding.clone(), record);
        document.encode(&self.contract)?;
        if dry_run {
            // Preview reports current state, not proposed successful installation.
            let mut result = LocalLifecycleResult::from_record(None, RegistryEffect::NoChange);
            result.preview = Some(LocalLifecyclePreview {
                artifact_action: "create",
                add_callbacks: 0,
                remove_callbacks: 0,
                preserve_slots: 3,
            });
            return Ok(result);
        }
        let mut transaction = self.registry.begin_lifecycle(snapshot.stamp(), true)?;
        transaction.commit(&document)?;
        let result = (|| {
            lifecycle_boundary(LifecycleBoundary::AfterInstallIntent)?;
            // The intent commit legitimately creates the owned state namespace.
            // Reobserve only this artifact's still-absent condition under the
            // same reservation; never adopt an appeared file or stale catalog.
            let current = FileObservation::capture(&artifact_path, true)?;
            if current.fingerprint() != before.fingerprint() {
                return Err(fail("artifact_already_exists"));
            }
            transaction.verify()?;
            current.replace(&bytes, || transaction.verify())?;
            lifecycle_boundary(LifecycleBoundary::AfterArtifactCreation)?;
            finish_install(&mut transaction, &mut document, &binding, &bytes)?;
            Ok(LocalLifecycleResult::from_record(
                document.installations.get(&binding),
                RegistryEffect::AppliedVerified,
            ))
        })();
        result.map_err(|failure| transaction.qualify(failure))
    }

    fn reconcile_install(
        &self,
        snapshot: &super::state::RegistrySnapshot,
        binding: &str,
        record: &InstallationRecord,
    ) -> Result<LocalLifecycleResult, RegistryFailure> {
        let pending = record
            .pending
            .as_ref()
            .ok_or_else(|| fail("pending_operation"))?;
        if pending.operation != super::config_profile::ConfigIntent::Install {
            return Err(fail("pending_operation"));
        }
        let mut transaction = self.registry.begin_lifecycle(snapshot.stamp(), false)?;
        let mut document = transaction
            .document()
            .cloned()
            .ok_or_else(|| fail("installation_unavailable"))?;
        let result = (|| {
            let artifact =
                FileObservation::capture(&artifact_path(transaction.root()?, record), true)?;
            let actual = artifact.fingerprint();
            if actual == pending.artifact.after {
                let expected = record.artifact_bytes(&document.registry_id, binding)?;
                if artifact.raw() != Some(expected.as_slice()) {
                    return Err(fail("artifact_changed"));
                }
                finish_install(&mut transaction, &mut document, binding, &expected)?;
                Ok(LocalLifecycleResult::from_record(
                    document.installations.get(binding),
                    RegistryEffect::AppliedVerified,
                ))
            } else if actual == pending.artifact.before {
                // A recovery pass never recreates an absent artifact or activates hooks.
                artifact.check()?;
                advance(&mut document)?;
                document.installations.remove(binding);
                transaction.commit(&document)?;
                artifact.check()?;
                let mut result =
                    LocalLifecycleResult::from_record(None, RegistryEffect::AppliedVerified);
                result.recovery = Some(RecoveryOutcome::OperationNotApplied);
                Ok(result)
            } else {
                Err(fail("pending_artifact_conflict"))
            }
        })();
        result.map_err(|failure| transaction.qualify(failure))
    }

    /// Legacy writers share the lifecycle reservation but retain their existing
    /// ownership and return types. No guard escapes or spans a later state purge.
    pub fn run_legacy_claude<T>(
        &self,
        target: &Path,
        activate: bool,
        action: impl FnOnce(&Path) -> T,
    ) -> Result<Option<T>, RegistryFailure> {
        let snapshot = self.registry.read()?;
        let before = FileObservation::capture(target, false)?;
        if activate
            && snapshot
                .document()
                .is_some_and(|document| !document.installations.is_empty())
        {
            return Err(fail("canonical_installation_conflict"));
        }
        if activate {
            // Expected legacy entries may already exist; only orphan canonical
            // callbacks are refused here, never adopted by legacy ownership.
            config_settings::ensure_no_canonical_callbacks(&before.document()?)?;
        }
        let parent = super::parents::ParentObservation::capture(
            self.registry
                .root()?
                .parent()
                .ok_or_else(|| fail("invalid_lifecycle_context"))?,
        )?;
        if !activate && parent.needs_preparation() && before.raw().is_none() {
            parent.check()?;
            before.check()?;
            if self.registry.read()?.stamp() != snapshot.stamp() {
                return Err(fail("registry_changed"));
            }
            before.check()?;
            parent.check()?;
            return Ok(None);
        }
        let mut transaction = self.registry.begin_legacy(snapshot.stamp(), activate)?;
        let result = (|| {
            if activate {
                transaction.prepare_legacy_marker_root()?;
            }
            before.check()?;
            transaction.verify()?;
            let result = action(transaction.root()?);
            // A legacy closure has its own mutation reporting; a later shared
            // integrity failure cannot truthfully assert that it made no changes.
            transaction.note_configuration_change();
            transaction.verify()?;
            Ok(Some(result))
        })();
        result.map_err(|failure| transaction.qualify(failure))
    }

    /// Passive inspection reports integrity separately from recorded intent.
    /// It never reconciles a pending operation or initializes coordination state.
    pub fn inspect_installation(
        &self,
        adapter_id: &str,
    ) -> Result<Option<LocalInstallationStatus>, RegistryFailure> {
        Ok(self
            .inspect_adapters()?
            .local_installations(Some(adapter_id))
            .next()
            .cloned())
    }

    /// Registration and installation facts share the same original catalog.
    /// Catalog drift refuses the whole query; local damage preserves recorded
    /// intent while downgrading only its integrity observation.
    pub fn inspect_adapters(&self) -> Result<PassiveAdapterInspection, RegistryFailure> {
        let witness = self.registry.observe_passively()?;
        let snapshot = witness.snapshot();
        let mut installations = Vec::new();
        let mut proofs = Vec::new();
        if let Some(document) = snapshot.document() {
            for (binding, record) in &document.installations {
                let (status, proof) = self.inspect_record(document, binding, record);
                let index = installations.len();
                installations.push((record.adapter_id.clone(), status));
                if let Some((artifact, target)) = proof {
                    proofs.push((index, record, artifact, target));
                }
            }
        }
        lifecycle_boundary(LifecycleBoundary::BeforeInspectionReturn)?;
        witness.check()?;
        for (index, record, artifact, target) in proofs {
            let current = verify_package(record, Path::new(&record.binary.path))
                .and_then(|_| artifact.check())
                .and_then(|_| target.check());
            if let Err(failure) = current {
                let status = &mut installations[index].1;
                status.integrity = "unknown";
                status.local_connection = "unknown";
                status.reason = Some(failure.reason);
            }
        }
        witness.check()?;
        Ok(PassiveAdapterInspection {
            snapshot,
            installations,
        })
    }

    fn inspect_record(
        &self,
        document: &RegistryDocument,
        binding: &str,
        record: &InstallationRecord,
    ) -> (
        LocalInstallationStatus,
        Option<(FileObservation, FileObservation)>,
    ) {
        let current = (|| {
            if record.context.state_root != locator(self.registry.root()?)? {
                return Err(fail("installation_context_changed"));
            }
            if record.pending.is_some() {
                return Err(fail("pending_operation"));
            }
            let artifact = verify_artifact(document, binding, record)?;
            verify_package(record, Path::new(&record.binary.path))?;
            let target = FileObservation::capture(Path::new(&record.context.target_path), false)?;
            let owned = observe_owned(binding, record, &target)?;
            if owned != record.desired_enabled {
                return Err(fail("connection_changed"));
            }
            if record.desired_enabled {
                config_settings::ensure_activation_compatible(
                    &target.document()?,
                    &callbacks(record, binding),
                )?;
            }
            artifact.check()?;
            target.check()?;
            Ok((artifact, target))
        })();
        let reason = current
            .as_ref()
            .err()
            .map(|failure: &RegistryFailure| failure.reason);
        let status = LocalInstallationStatus {
            profile: CLAUDE_PROFILE,
            installed: record.installed,
            desired_enabled: record.desired_enabled,
            local_connection: if record.pending.is_some() {
                "pending"
            } else if current.is_err() {
                "unknown"
            } else if record.desired_enabled {
                "connected"
            } else {
                "disconnected"
            },
            integrity: if current.is_ok() {
                "verified"
            } else {
                "unknown"
            },
            reason,
            pending_operation: record.pending.as_ref().map(|pending| pending.operation),
            native_verification: "unverified",
            host_trust: "unknown",
            observed_execution: "unknown",
            effective_support: "unknown",
        };
        (status, current.ok())
    }

    /// Explicit local connection changes. The gate remains closed until actual
    /// target readback and the completion commit establish coherent evidence.
    pub fn change_connection(
        &self,
        adapter_id: &str,
        selection: &LocalInstallation,
        intent: ConfigIntent,
        dry_run: bool,
    ) -> Result<LocalLifecycleResult, RegistryFailure> {
        if intent == ConfigIntent::Install {
            return self.install(adapter_id, selection, dry_run);
        }
        let context = self.context(adapter_id, selection)?;
        let snapshot = self.registry.read()?;
        let document = snapshot
            .document()
            .ok_or_else(|| fail("installation_unavailable"))?;
        let (binding, record) = document
            .installations
            .iter()
            .next()
            .ok_or_else(|| fail("installation_unavailable"))?;
        if record.context != context {
            return Err(fail("installation_context_changed"));
        }
        if record
            .pending
            .as_ref()
            .is_some_and(|pending| pending.operation != intent)
        {
            return Err(fail("pending_operation"));
        }
        if intent == ConfigIntent::Uninstall && record.desired_enabled {
            return Err(fail("disable_required"));
        }
        if intent == ConfigIntent::Enable {
            verify_package(record, &selection.binary)?;
        }
        if dry_run {
            if record.pending.is_some() {
                return Err(fail("pending_operation"));
            }
            let artifact = verify_artifact(document, binding, record)?;
            let target = FileObservation::capture(Path::new(&record.context.target_path), false)?;
            let owned = observe_owned(binding, record, &target)?;
            if record.desired_enabled && !owned {
                return Err(fail("connection_changed"));
            }
            let planned = connection_bytes(document, binding, record, intent, &target, owned)?;
            artifact.check()?;
            target.check()?;
            let mut result =
                LocalLifecycleResult::from_record(Some(record), RegistryEffect::NoChange);
            result.preview = Some(planned.preview);
            return Ok(result);
        }
        let binding = binding.clone();
        let mut transaction = self.registry.begin_admission(snapshot.stamp())?;
        let mut document = transaction
            .document()
            .cloned()
            .ok_or_else(|| fail("installation_unavailable"))?;
        let result = (|| {
            if intent == ConfigIntent::Disable {
                let record = &document.installations[&binding];
                if record.pending.is_none() && record.desired_enabled {
                    let base_revision = record.revision;
                    let expected = FileFingerprint::File {
                        sha256: record.artifact_sha256.clone(),
                    };
                    let historical = record.connection.as_ref().map(|connection| TargetDelta {
                        before: connection.target.clone(),
                        after: connection.target.clone(),
                        owned_before: connection.owned.clone(),
                        owned_after: connection.owned.clone(),
                    });
                    let revision = advance(&mut document)?;
                    let record = document.installations.get_mut(&binding).unwrap();
                    record.revision = revision;
                    record.desired_enabled = false;
                    record.pending = Some(PendingIntent {
                        transaction_id: uuid::Uuid::new_v4().to_string(),
                        operation: intent,
                        phase: IntentPhase::GateClosed,
                        base_revision,
                        artifact: FileDelta {
                            before: expected.clone(),
                            after: expected,
                        },
                        target: historical,
                    });
                    transaction.commit(&document)?;
                    lifecycle_boundary(LifecycleBoundary::AfterGateClosed)?;
                }
            }
            let record = document.installations[&binding].clone();
            let artifact =
                FileObservation::capture(&artifact_path(transaction.root()?, &record), true)?;
            let expected_artifact = record.artifact_bytes(&document.registry_id, &binding)?;
            let removed_artifact = intent == ConfigIntent::Uninstall
                && record
                    .pending
                    .as_ref()
                    .is_some_and(|p| p.phase == IntentPhase::Prepared)
                && artifact.fingerprint() == FileFingerprint::Absent;
            if !removed_artifact && artifact.raw() != Some(expected_artifact.as_slice()) {
                return Err(fail("artifact_changed"));
            }
            let observed = FileObservation::capture(Path::new(&record.context.target_path), false)?;
            let owned = observe_owned(&binding, &record, &observed)?;
            if let Some(pending) = record
                .pending
                .as_ref()
                .filter(|p| p.phase == IntentPhase::Prepared)
            {
                let delta = pending
                    .target
                    .as_ref()
                    .ok_or_else(|| fail("pending_operation"))?;
                if observed.fingerprint() == delta.after && owned == !delta.owned_after.is_empty() {
                    let target = if observed.parent_missing() && intent != ConfigIntent::Enable {
                        TargetAccess::Observed(Box::new(observed))
                    } else {
                        TargetAccess::Locked(Box::new(observed.lock_target()?))
                    };
                    return complete_connection(
                        &mut transaction,
                        &mut document,
                        &binding,
                        intent,
                        &target,
                        artifact,
                        &selection.binary,
                    );
                }
                if intent == ConfigIntent::Enable {
                    if observed.fingerprint() != delta.before || owned {
                        return Err(fail("pending_target_conflict"));
                    }
                    let target = if observed.parent_missing() {
                        TargetAccess::Observed(Box::new(observed))
                    } else {
                        TargetAccess::Locked(Box::new(observed.lock_target()?))
                    };
                    target.verify()?;
                    artifact.check()?;
                    transaction.verify()?;
                    let revision = advance(&mut document)?;
                    let record = document.installations.get_mut(&binding).unwrap();
                    record.revision = revision;
                    record.pending = None;
                    record.connection = Some(ConnectionEvidence {
                        target: target.observation().fingerprint(),
                        owned: Vec::new(),
                    });
                    transaction.commit(&document)?;
                    target.verify()?;
                    artifact.check()?;
                    transaction.verify()?;
                    target.verify()?;
                    artifact.check()?;
                    let mut result = LocalLifecycleResult::from_record(
                        document.installations.get(&binding),
                        RegistryEffect::AppliedVerified,
                    );
                    result.recovery = Some(RecoveryOutcome::OperationNotApplied);
                    return Ok(result);
                }
                if removed_artifact || (observed.fingerprint() != delta.before && !owned) {
                    return Err(fail("pending_target_conflict"));
                }
            }
            if record.pending.is_none()
                && intent != ConfigIntent::Uninstall
                && record.desired_enabled == (intent == ConfigIntent::Enable)
            {
                if intent == ConfigIntent::Enable {
                    config_settings::ensure_activation_compatible(
                        &observed.document()?,
                        &callbacks(&record, &binding),
                    )?;
                }
                if owned != record.desired_enabled {
                    return Err(fail("connection_changed"));
                }
                transaction.verify()?;
                artifact.check()?;
                observed.check()?;
                transaction.verify()?;
                artifact.check()?;
                observed.check()?;
                return Ok(LocalLifecycleResult::from_record(
                    Some(&record),
                    RegistryEffect::NoChange,
                ));
            }
            let planned = connection_bytes(&document, &binding, &record, intent, &observed, owned)?;
            let changes_target = planned.raw.as_deref() != observed.raw();
            // Absence-only teardown keeps its original parent observation and
            // creates neither host directories nor target coordination state.
            let mut target = if !changes_target && observed.parent_missing() {
                TargetAccess::Observed(Box::new(observed))
            } else {
                TargetAccess::Locked(Box::new(observed.lock_target()?))
            };
            let before = target.observation().fingerprint();
            let after = FileFingerprint::from_bytes(planned.raw.as_deref());
            let base_revision = record
                .pending
                .as_ref()
                .map_or(record.revision, |p| p.base_revision);
            let transaction_id = record.pending.as_ref().map_or_else(
                || uuid::Uuid::new_v4().to_string(),
                |p| p.transaction_id.clone(),
            );
            let expected = FileFingerprint::File {
                sha256: record.artifact_sha256.clone(),
            };
            let revision = advance(&mut document)?;
            let record = document.installations.get_mut(&binding).unwrap();
            record.revision = revision;
            record.desired_enabled = false;
            record.pending = Some(PendingIntent {
                transaction_id,
                operation: intent,
                phase: IntentPhase::Prepared,
                base_revision,
                artifact: FileDelta {
                    before: expected.clone(),
                    after: if intent == ConfigIntent::Uninstall {
                        FileFingerprint::Absent
                    } else {
                        expected
                    },
                },
                target: Some(TargetDelta {
                    before,
                    after: after.clone(),
                    owned_before: if owned {
                        record.owned_hooks(&binding)
                    } else {
                        Vec::new()
                    },
                    owned_after: if intent == ConfigIntent::Enable {
                        record.owned_hooks(&binding)
                    } else {
                        Vec::new()
                    },
                }),
            });
            target.verify()?;
            artifact.check()?;
            transaction.verify()?;
            transaction.commit(&document)?;
            lifecycle_boundary(LifecycleBoundary::AfterConnectionIntent)?;
            if changes_target {
                target.replace(
                    planned
                        .raw
                        .as_deref()
                        .ok_or_else(|| fail("invalid_target_plan"))?,
                    || transaction.verify(),
                )?;
            }
            lifecycle_boundary(LifecycleBoundary::AfterTargetReplacement)?;
            if target.observation().fingerprint() != after {
                return Err(fail("target_readback_changed"));
            }
            complete_connection(
                &mut transaction,
                &mut document,
                &binding,
                intent,
                &target,
                artifact,
                &selection.binary,
            )
        })();
        result.map_err(|failure| transaction.qualify(failure))
    }

    /// Missing/disabled/pending exits before artifact, package, target or stdin.
    /// An enabled callback is admitted under the existing noncreating reservation.
    /// That short reservation is released before the caller runs any handler.
    pub fn admit_callback(
        &self,
        binding: &str,
        installation: &str,
        slot: ConfigSlot,
        consumer_binary: &Path,
    ) -> Result<bool, RegistryFailure> {
        if !uuid_valid(binding) || !uuid_valid(installation) {
            return Err(fail("invalid_callback_reference"));
        }
        let snapshot = self.registry.read()?;
        if !potentially_enabled(snapshot.document(), binding, installation) {
            return Ok(false);
        }
        lifecycle_boundary(LifecycleBoundary::AfterPassiveEnabledObservation)?;
        let transaction = self.registry.begin_admission(snapshot.stamp())?;
        let document = transaction
            .document()
            .ok_or_else(|| fail("installation_unavailable"))?;
        if !potentially_enabled(Some(document), binding, installation) {
            return Ok(false);
        }
        let record = &document.installations[binding];
        if Path::new(&record.binary.path) != consumer_binary
            || product_executable_digest(consumer_binary)? != record.binary.sha256
            || record.context.state_root != locator(transaction.root()?)?
        {
            return Err(fail("consumer_identity_changed"));
        }
        let artifact = verify_artifact(document, binding, record)?;
        if !record
            .artifact(&document.registry_id, binding)
            .slots
            .iter()
            .any(|candidate| candidate.slot == slot)
        {
            return Err(fail("invalid_callback_slot"));
        }
        let target = verify_connected(binding, record)?;
        lifecycle_boundary(LifecycleBoundary::BeforeFinalAdmission)?;
        transaction.verify()?;
        artifact.check()?;
        target.check()?;
        transaction.verify()?;
        artifact.check()?;
        target.check()?;
        Ok(true)
    }
}

enum TargetAccess {
    Observed(Box<FileObservation>),
    Locked(Box<LockedTarget>),
}

impl TargetAccess {
    fn observation(&self) -> &FileObservation {
        match self {
            Self::Observed(value) => value,
            Self::Locked(value) => value.observation(),
        }
    }
    fn verify(&self) -> Result<(), RegistryFailure> {
        if let Self::Locked(value) = self {
            value.verify()?;
        }
        self.observation().check()
    }
    fn replace(
        &mut self,
        raw: &[u8],
        reservation: impl Fn() -> Result<(), RegistryFailure>,
    ) -> Result<(), RegistryFailure> {
        match self {
            Self::Locked(value) => value.replace(raw, reservation),
            Self::Observed(_) => Err(fail("unreserved_target_mutation")),
        }
    }
}

fn verify_package(record: &InstallationRecord, binary: &Path) -> Result<(), RegistryFailure> {
    if record.binary.path != locator(binary)?
        || product_executable_digest(binary)? != record.binary.sha256
    {
        return Err(fail("consumer_identity_changed"));
    }
    Ok(())
}

fn observe_owned(
    binding: &str,
    record: &InstallationRecord,
    target: &FileObservation,
) -> Result<bool, RegistryFailure> {
    let observed = config_settings::observe_correlated(
        &target.document()?,
        &callbacks(record, binding),
        binding,
        &record.installation_id,
    )?;
    let count = observed
        .iter()
        .filter(|value| **value == CallbackPresence::ExactOwned)
        .count();
    if count == 0 {
        return Ok(false);
    }
    if count != 3 {
        return Err(fail("partial_owned_callbacks"));
    }
    let hooks = record.owned_hooks(binding);
    let completed = record.connection.as_ref().is_some_and(|c| c.owned == hooks);
    let pending_after = record
        .pending
        .as_ref()
        .filter(|p| p.phase == IntentPhase::Prepared)
        .and_then(|p| p.target.as_ref())
        .is_some_and(|delta| delta.after == target.fingerprint() && delta.owned_after == hooks);
    if !completed && !pending_after {
        return Err(fail("unproved_callback_ownership"));
    }
    Ok(true)
}

struct PlannedConnection {
    raw: Option<Vec<u8>>,
    preview: LocalLifecyclePreview,
}

fn connection_bytes(
    document: &RegistryDocument,
    binding: &str,
    record: &InstallationRecord,
    intent: ConfigIntent,
    target: &FileObservation,
    owned: bool,
) -> Result<PlannedConnection, RegistryFailure> {
    let request = connection_request(document, binding, record, intent, target, owned)?;
    let plan = config_profile::build_plan(&request)?;
    render_connection_plan(binding, record, intent, target, &plan)
}

fn connection_request(
    document: &RegistryDocument,
    binding: &str,
    record: &InstallationRecord,
    intent: ConfigIntent,
    target: &FileObservation,
    owned: bool,
) -> Result<ProfileRequest, RegistryFailure> {
    if intent == ConfigIntent::Enable {
        config_settings::ensure_activation_compatible(
            &target.document()?,
            &callbacks(record, binding),
        )?;
    }
    let artifact = record.artifact(&document.registry_id, binding);
    let hooks = record.owned_hooks(binding);
    let request = ProfileRequest {
        schema_version: 1,
        intent,
        binding_id: binding.into(),
        installation_id: record.installation_id.clone(),
        target_revision: match target.fingerprint() {
            FileFingerprint::Absent => None,
            FileFingerprint::File { sha256 } => Some(sha256),
        },
        slots: artifact
            .slots
            .iter()
            .zip(&hooks)
            .map(|(slot, hook)| SlotRequest {
                slot: slot.slot,
                callback_ref: slot.callback_ref.clone(),
                desired: if intent == ConfigIntent::Enable {
                    DesiredSlot::Present
                } else {
                    DesiredSlot::Absent
                },
                observed: if owned {
                    SlotObserved::ExactOwned
                } else {
                    SlotObserved::Absent
                },
                owned_digest: owned.then(|| hook.sha256.clone()),
            })
            .collect(),
    };
    config_profile::build_plan(&request)?;
    Ok(request)
}

fn render_connection_plan(
    binding: &str,
    record: &InstallationRecord,
    intent: ConfigIntent,
    target: &FileObservation,
    plan: &config_profile::ValidatedConnectionPlan,
) -> Result<PlannedConnection, RegistryFailure> {
    let old = target.document()?;
    let adds = plan.changes().iter().any(|c| c.action == SlotAction::Add);
    let removes = plan
        .changes()
        .iter()
        .any(|c| c.action == SlotAction::Remove);
    if adds && removes {
        return Err(fail("invalid_target_plan"));
    }
    let new = if adds || removes {
        config_settings::connect(&old, &callbacks(record, binding), adds)?
    } else {
        old.clone()
    };
    let preview = LocalLifecyclePreview {
        artifact_action: if intent == ConfigIntent::Uninstall {
            "remove"
        } else {
            "preserve"
        },
        add_callbacks: plan
            .changes()
            .iter()
            .filter(|c| c.action == SlotAction::Add)
            .count(),
        remove_callbacks: plan
            .changes()
            .iter()
            .filter(|c| c.action == SlotAction::Remove)
            .count(),
        preserve_slots: plan
            .changes()
            .iter()
            .filter(|c| c.action == SlotAction::Preserve)
            .count(),
    };
    if old == new {
        return Ok(PlannedConnection {
            raw: target.raw().map(<[u8]>::to_vec),
            preview,
        });
    }
    let raw = serde_json::to_vec_pretty(&new).map_err(|_| fail("invalid_target_plan"))?;
    if raw.len() > 1024 * 1024 {
        return Err(fail("configuration_limit"));
    }
    Ok(PlannedConnection {
        raw: Some(raw),
        preview,
    })
}

fn complete_connection(
    transaction: &mut LifecycleTransaction<'_>,
    document: &mut RegistryDocument,
    binding: &str,
    intent: ConfigIntent,
    target: &TargetAccess,
    artifact: FileObservation,
    binary: &Path,
) -> Result<LocalLifecycleResult, RegistryFailure> {
    let record = &document.installations[binding];
    let pending = record
        .pending
        .as_ref()
        .filter(|p| p.phase == IntentPhase::Prepared && p.operation == intent)
        .ok_or_else(|| fail("pending_operation"))?;
    let delta = pending
        .target
        .as_ref()
        .ok_or_else(|| fail("pending_operation"))?;
    if target.observation().fingerprint() != delta.after
        || observe_owned(binding, record, target.observation())? != !delta.owned_after.is_empty()
    {
        return Err(fail("pending_target_conflict"));
    }
    if intent == ConfigIntent::Enable {
        verify_package(record, binary)?;
        config_settings::ensure_activation_compatible(
            &target.observation().document()?,
            &callbacks(record, binding),
        )?;
    }
    target.verify()?;
    artifact.check()?;
    transaction.verify()?;
    let final_artifact = if intent == ConfigIntent::Uninstall {
        if artifact.fingerprint() != pending.artifact.before
            && artifact.fingerprint() != pending.artifact.after
        {
            return Err(fail("pending_artifact_conflict"));
        }
        let removed = artifact.remove(|| {
            target.verify()?;
            transaction.verify()
        })?;
        if artifact.raw().is_some() {
            transaction.note_configuration_change();
        }
        removed
    } else {
        if artifact.fingerprint() != pending.artifact.after {
            return Err(fail("artifact_changed"));
        }
        artifact
    };
    target.verify()?;
    final_artifact.check()?;
    transaction.verify()?;
    lifecycle_boundary(LifecycleBoundary::BeforeConnectionCompletion)?;
    let revision = advance(document)?;
    if intent == ConfigIntent::Uninstall {
        document.installations.remove(binding);
    } else {
        let record = document.installations.get_mut(binding).unwrap();
        record.revision = revision;
        record.desired_enabled = intent == ConfigIntent::Enable;
        record.pending = None;
        record.connection = Some(ConnectionEvidence {
            target: target.observation().fingerprint(),
            owned: if intent == ConfigIntent::Enable {
                record.owned_hooks(binding)
            } else {
                Vec::new()
            },
        });
    }
    transaction.commit(document)?;
    target.verify()?;
    final_artifact.check()?;
    transaction.verify()?;
    lifecycle_boundary(LifecycleBoundary::AfterFinalConnectionCatalogVerification)?;
    final_artifact.check()?;
    target.verify()?;
    Ok(LocalLifecycleResult::from_record(
        document.installations.get(binding),
        RegistryEffect::AppliedVerified,
    ))
}

fn potentially_enabled(
    document: Option<&RegistryDocument>,
    binding: &str,
    installation: &str,
) -> bool {
    document
        .and_then(|document| document.installations.get(binding))
        .is_some_and(|record| {
            record.installation_id == installation
                && record.installed
                && record.desired_enabled
                && record.pending.is_none()
        })
}

fn artifact_path(root: &Path, record: &InstallationRecord) -> PathBuf {
    root.join("host-adapters/installations")
        .join(format!("{}.json", record.installation_id))
}

fn verify_artifact(
    document: &RegistryDocument,
    binding: &str,
    record: &InstallationRecord,
) -> Result<FileObservation, RegistryFailure> {
    let expected = record.artifact_bytes(&document.registry_id, binding)?;
    let artifact = FileObservation::capture(
        &artifact_path(Path::new(&record.context.state_root), record),
        true,
    )?;
    if artifact.raw() != Some(expected.as_slice()) {
        return Err(fail("artifact_changed"));
    }
    Ok(artifact)
}

fn finish_install(
    transaction: &mut LifecycleTransaction<'_>,
    document: &mut RegistryDocument,
    binding: &str,
    expected: &[u8],
) -> Result<(), RegistryFailure> {
    let record = document
        .installations
        .get(binding)
        .ok_or_else(|| fail("installation_unavailable"))?;
    if product_executable_digest(Path::new(&record.binary.path))? != record.binary.sha256 {
        return Err(fail("consumer_identity_changed"));
    }
    let artifact = verify_artifact(document, binding, record)?;
    if artifact.raw() != Some(expected) {
        return Err(fail("artifact_changed"));
    }
    artifact.check()?;
    transaction.verify()?;
    lifecycle_boundary(LifecycleBoundary::BeforeInstallCompletion)?;
    let revision = advance(document)?;
    let record = document.installations.get_mut(binding).unwrap();
    record.revision = revision;
    record.installed = true;
    record.pending = None;
    transaction.commit(document)?;
    artifact.check()?;
    transaction.verify()
}

fn callbacks(record: &InstallationRecord, binding: &str) -> [OwnedCallback; 3] {
    record
        .owned_hooks(binding)
        .into_iter()
        .map(|hook| OwnedCallback {
            event: hook.slot.native_event(),
            command: hook.handler.command,
        })
        .collect::<Vec<_>>()
        .try_into()
        .unwrap_or_else(|_| unreachable!("closed three-slot profile"))
}

fn verify_connected(
    binding: &str,
    record: &InstallationRecord,
) -> Result<FileObservation, RegistryFailure> {
    let target = FileObservation::capture(Path::new(&record.context.target_path), false)?;
    config_settings::ensure_activation_compatible(
        &target.document()?,
        &callbacks(record, binding),
    )?;
    let presence = config_settings::observe_correlated(
        &target.document()?,
        &callbacks(record, binding),
        binding,
        &record.installation_id,
    )?;
    if presence != [CallbackPresence::ExactOwned; 3] {
        return Err(fail("connection_changed"));
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    thread_local! {
        pub(super) static INTERRUPTION: std::cell::Cell<Option<LifecycleBoundary>> = const { std::cell::Cell::new(None) };
        pub(super) static EXPIRE_PLAN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    type BoundaryAction = (LifecycleBoundary, Box<dyn FnOnce()>);
    thread_local! { static ACTION: std::cell::RefCell<Option<BoundaryAction>> = const { std::cell::RefCell::new(None) }; }
    pub(super) fn action(point: LifecycleBoundary) {
        let action = ACTION.with(|slot| {
            let mut selected = slot.borrow_mut();
            if selected.as_ref().is_some_and(|(at, _)| *at == point) {
                selected.take().map(|(_, action)| action)
            } else {
                None
            }
        });
        if let Some(action) = action {
            action();
        }
    }

    struct Interruption;
    impl Interruption {
        fn at(point: LifecycleBoundary) -> Self {
            INTERRUPTION.with(|selected| selected.set(Some(point)));
            Self
        }
    }
    impl Drop for Interruption {
        fn drop(&mut self) {
            INTERRUPTION.with(|selected| selected.set(None));
        }
    }

    fn fixture() -> (tempfile::TempDir, ConfigLifecycle, LocalInstallation) {
        let home = tempfile::tempdir().unwrap();
        let binary = home.path().join("libra-governor");
        fs::copy("/bin/sh", &binary).unwrap();
        let selection = LocalInstallation {
            scope: "user".into(),
            profile: CLAUDE_PROFILE.into(),
            target: home.path().join("home/.claude/settings.json"),
            binary,
        };
        let lifecycle =
            ConfigLifecycle::new(home.path().join("state"), HostContract::load().unwrap());
        (home, lifecycle, selection)
    }

    #[test]
    fn registered_plan_final_boundary_cancellation_and_expiry_prevent_return() {
        for dry_run in [true, false] {
            for expire in [true, false] {
                let (home, lifecycle, selection) = fixture();
                lifecycle.install("claude_code", &selection, false).unwrap();
                let script = home.path().join("planner.py");
                let log = home.path().join("operations.log");
                let registry_file = home.path().join("state/host-adapters/registry.json");
                fs::write(
                    &script,
                    include_str!("../../tests/fixtures/external_config_driver.py"),
                )
                .unwrap();
                let mut manifest: serde_json::Value = serde_json::from_str(include_str!("../../../protocol/contracts/host-adapter/v1/fixtures/valid-manifest-synthetic.json")).unwrap();
                manifest["adapter_id"] = serde_json::json!("terminal_plan_fixture");
                manifest["roles"] = serde_json::json!(["ConfigDriver"]);
                manifest["host_version_constraints"][0]["provider"] =
                    serde_json::json!("claude_code");
                manifest["launch"] = serde_json::json!({"executable":"/usr/bin/python3","argv":[script,registry_file,log,"normal"]});
                manifest["runtime_files"] = serde_json::json!([
                    {"path":"/usr/bin/python3","kind":"entrypoint","digest":super::super::config_bundle::sha256(&fs::read("/usr/bin/python3").unwrap())},
                    {"path":script,"kind":"entrypoint","digest":super::super::config_bundle::sha256(&fs::read(&script).unwrap())}
                ]);
                let manifest = lifecycle
                    .contract
                    .validate_manifest(&serde_json::to_vec(&manifest).unwrap())
                    .unwrap();
                lifecycle
                    .registry
                    .register(manifest, lifecycle.registry.read().unwrap().stamp())
                    .unwrap();
                let review = lifecycle
                    .registry
                    .review_trust("terminal_plan_fixture")
                    .unwrap();
                lifecycle
                    .registry
                    .confirm_trust(
                        "terminal_plan_fixture",
                        review.manifest_digest(),
                        review.confirmation_digest(),
                        lifecycle.registry.read().unwrap().stamp(),
                    )
                    .unwrap();
                let before = fs::read(&registry_file).unwrap();
                let cancellation = Cancellation::default();
                let stopped = cancellation.clone();
                let called = std::rc::Rc::new(std::cell::Cell::new(false));
                let published = called.clone();
                ACTION.with(|action| {
                    *action.borrow_mut() = Some((
                        LifecycleBoundary::BeforePlanReturn,
                        Box::new(move || {
                            published.set(true);
                            if expire {
                                EXPIRE_PLAN.with(|flag| flag.set(true));
                            } else {
                                stopped.cancel();
                            }
                        }),
                    ))
                });
                let result = lifecycle.preview_registered_plan(
                    "terminal_plan_fixture",
                    &selection,
                    ConfigIntent::Enable,
                    dry_run,
                    &cancellation,
                );
                EXPIRE_PLAN.with(|flag| flag.set(false));
                let failure = result.unwrap_err();
                assert!(called.get());
                assert_eq!(failure.stage, if expire { "context" } else { "execution" });
                assert_eq!(
                    failure.reason,
                    if expire {
                        "diagnostic context expired"
                    } else {
                        "diagnostic cancelled"
                    }
                );
                assert_eq!(failure.execution_attempted, !dry_run);
                assert_eq!(fs::read(&registry_file).unwrap(), before);
                assert!(!selection.target.exists());
                if dry_run {
                    assert!(!log.exists());
                } else {
                    assert_eq!(
                        fs::read_to_string(&log).unwrap(),
                        "handshake\nplan_config\n"
                    );
                }
            }
        }
    }

    #[test]
    fn passive_inspection_refuses_replaced_original_catalog_before_return() {
        let (home, lifecycle, selection) = fixture();
        lifecycle.install("claude_code", &selection, false).unwrap();
        let catalog = home.path().join("state/host-adapters/registry.json");
        let original = fs::read(&catalog).unwrap();
        let replacement = catalog.clone();
        let retained = home.path().join("retained-catalog");
        let changed = std::rc::Rc::new(std::cell::Cell::new(false));
        let marked = changed.clone();
        ACTION.with(|slot| {
            *slot.borrow_mut() = Some((
                LifecycleBoundary::BeforeInspectionReturn,
                Box::new(move || {
                    let bytes = fs::read(&replacement).unwrap();
                    fs::rename(&replacement, retained).unwrap();
                    fs::write(&replacement, bytes).unwrap();
                    fs::set_permissions(&replacement, fs::Permissions::from_mode(0o600)).unwrap();
                    marked.set(true);
                }),
            ));
        });
        let result = lifecycle.inspect_installation("claude_code");
        assert!(changed.get());
        assert_eq!(fs::read(&catalog).unwrap(), original);
        assert!(!selection.target.exists());
        assert!(result.is_err(), "same-byte catalog replacement was adopted");
    }

    #[test]
    fn passive_inspection_refuses_namespace_replacement_and_state_appearance() {
        for component in ["root", "adapters", "absent"] {
            let (home, lifecycle, selection) = fixture();
            if component != "absent" {
                lifecycle.install("claude_code", &selection, false).unwrap();
            }
            let root = home.path().join("state");
            let source = if component == "adapters" {
                root.join("host-adapters")
            } else {
                root.clone()
            };
            let retained = home.path().join("retained-namespace");
            let catalog = root.join("host-adapters/registry.json");
            let original = fs::read(&catalog).ok();
            let replacement_bytes = original.clone();
            let destination = catalog.clone();
            let changed = std::rc::Rc::new(std::cell::Cell::new(false));
            let marked = changed.clone();
            ACTION.with(|slot| {
                *slot.borrow_mut() = Some((
                    LifecycleBoundary::BeforeInspectionReturn,
                    Box::new(move || {
                        if replacement_bytes.is_some() {
                            fs::rename(&source, retained).unwrap();
                        }
                        fs::create_dir_all(destination.parent().unwrap()).unwrap();
                        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
                        fs::set_permissions(
                            destination.parent().unwrap(),
                            fs::Permissions::from_mode(0o700),
                        )
                        .unwrap();
                        if let Some(bytes) = replacement_bytes {
                            fs::write(&destination, bytes).unwrap();
                            fs::set_permissions(&destination, fs::Permissions::from_mode(0o600))
                                .unwrap();
                        }
                        marked.set(true);
                    }),
                ));
            });
            let failure = lifecycle
                .inspect_adapters()
                .err()
                .expect("changed namespace must refuse query");
            assert!(changed.get());
            assert_eq!(failure.effect, RegistryEffect::NoChange);
            assert_eq!(fs::read(&catalog).ok(), original);
            assert!(!selection.target.exists());
        }
    }

    #[test]
    fn passive_inspection_keeps_real_pending_intent_without_reconciliation() {
        let (home, lifecycle, selection) = fixture();
        let interruption = Interruption::at(LifecycleBoundary::AfterArtifactCreation);
        assert!(lifecycle.install("claude_code", &selection, false).is_err());
        drop(interruption);
        let catalog = home.path().join("state/host-adapters/registry.json");
        let before = fs::read(&catalog).unwrap();
        let (_, record) = installed_record(&lifecycle);
        let artifact = artifact_path(lifecycle.registry.root().unwrap(), &record);
        let artifact_before = fs::read(&artifact).unwrap();
        let inspection = lifecycle.inspect_adapters().unwrap();
        let status = inspection
            .local_installations(Some("claude_code"))
            .next()
            .unwrap();
        assert!(!status.installed);
        assert!(!status.desired_enabled);
        assert_eq!(status.pending_operation, Some(ConfigIntent::Install));
        assert_eq!(status.local_connection, "pending");
        assert_eq!(status.integrity, "unknown");
        assert_eq!(status.reason, Some("pending_operation"));
        assert_eq!(status.native_verification, "unverified");
        assert_eq!(fs::read(&catalog).unwrap(), before);
        assert_eq!(fs::read(&artifact).unwrap(), artifact_before);
        assert!(!selection.target.exists());
    }

    #[test]
    fn passive_inspection_keeps_enabled_intent_when_local_proofs_change() {
        for component in [
            "artifact",
            "package",
            "target",
            "late_target",
            "late_artifact",
            "late_package",
        ] {
            let (home, lifecycle, selection) = fixture();
            fs::create_dir_all(selection.target.parent().unwrap()).unwrap();
            fs::write(&selection.target, b"{\"future\":true}").unwrap();
            lifecycle.install("claude_code", &selection, false).unwrap();
            lifecycle
                .change_connection("claude_code", &selection, ConfigIntent::Enable, false)
                .unwrap();
            let (_, record) = installed_record(&lifecycle);
            let changed = match component {
                "artifact" | "late_artifact" => {
                    artifact_path(lifecycle.registry.root().unwrap(), &record)
                }
                "package" | "late_package" => selection.binary.clone(),
                _ => selection.target.clone(),
            };
            if component.starts_with("late_") {
                let destination = changed.clone();
                ACTION.with(|slot| {
                    *slot.borrow_mut() = Some((
                        LifecycleBoundary::BeforeInspectionReturn,
                        Box::new(move || fs::write(destination, b"{}").unwrap()),
                    ));
                });
            } else {
                fs::write(&changed, b"{}").unwrap();
            }
            let catalog = home.path().join("state/host-adapters/registry.json");
            let before = fs::read(&catalog).unwrap();
            let inspection = lifecycle.inspect_adapters().unwrap();
            let status = inspection
                .local_installations(Some("claude_code"))
                .next()
                .unwrap();
            assert!(status.installed);
            assert!(status.desired_enabled);
            assert_eq!(status.local_connection, "unknown");
            assert_eq!(status.integrity, "unknown");
            assert!(status.reason.is_some());
            assert_eq!(status.host_trust, "unknown");
            assert_eq!(status.observed_execution, "unknown");
            assert_eq!(fs::read(&changed).unwrap(), b"{}");
            assert_eq!(fs::read(&catalog).unwrap(), before);
        }
    }

    #[test]
    fn passive_inspection_refuses_a_valid_concurrent_catalog_writer() {
        let (home, lifecycle, selection) = fixture();
        lifecycle.install("claude_code", &selection, false).unwrap();
        let registry =
            AdapterRegistry::new(home.path().join("state"), HostContract::load().unwrap());
        let contract = HostContract::load().unwrap();
        let manifest = contract.validate_manifest(include_bytes!("../../../protocol/contracts/host-adapter/v1/fixtures/valid-manifest-synthetic.json")).unwrap();
        let id = manifest.value()["adapter_id"].as_str().unwrap().to_owned();
        let changed = std::rc::Rc::new(std::cell::Cell::new(false));
        let marked = changed.clone();
        ACTION.with(|slot| {
            *slot.borrow_mut() = Some((
                LifecycleBoundary::BeforeInspectionReturn,
                Box::new(move || {
                    registry
                        .register(manifest, registry.read().unwrap().stamp())
                        .unwrap();
                    marked.set(true);
                }),
            ));
        });
        let failure = lifecycle
            .inspect_adapters()
            .err()
            .expect("query must not mix revisions");
        assert!(changed.get());
        assert_eq!(failure.effect, RegistryEffect::NoChange);
        let current = lifecycle.registry.read().unwrap();
        assert!(current.document().unwrap().adapters.contains_key(&id));
        assert_eq!(current.document().unwrap().installations.len(), 1);
        assert!(!selection.target.exists());
    }

    #[test]
    fn passive_inspection_objects_release_all_owned_descriptors() {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "host_runtime::config_lifecycle::tests::isolated_passive_inspection_fd_helper",
            "--ignored",
        ]);
        let mut child = super::super::exec::spawn_test_child(&mut command).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                return;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("passive inspection descriptor control exceeded deadline");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    #[ignore = "invoked by passive inspection descriptor lifecycle control"]
    fn isolated_passive_inspection_fd_helper() {
        let (_home, lifecycle, selection) = fixture();
        lifecycle.install("claude_code", &selection, false).unwrap();
        let descriptors = || fs::read_dir("/dev/fd").unwrap().count();
        let before = descriptors();
        let mut retained = Vec::new();
        for _ in 0..20 {
            let inspection = lifecycle.inspect_adapters().unwrap();
            assert!(inspection.snapshot().document().is_some());
            assert_eq!(
                inspection
                    .local_installations(None)
                    .next()
                    .unwrap()
                    .integrity,
                "verified"
            );
            retained.push(inspection);
            assert_eq!(descriptors(), before);
        }
        assert_eq!(retained.len(), 20);
        drop(retained);
        assert_eq!(descriptors(), before);
    }

    fn installed_record(lifecycle: &ConfigLifecycle) -> (String, InstallationRecord) {
        let snapshot = lifecycle.registry.read().unwrap();
        let (binding, record) = snapshot
            .document()
            .unwrap()
            .installations
            .iter()
            .next()
            .unwrap();
        (binding.clone(), record.clone())
    }

    #[test]
    fn enable_refuses_legacy_or_orphan_callbacks_but_disable_preserves_them() {
        for command in ["'/usr/bin/libra-governor' hook user-prompt-submit", "'/usr/bin/libra-governor' hook post-tool-use", "'/usr/bin/libra-governor' hook stop", "'/tmp/renamed-product' adapter-hook --state-root '/elsewhere' --binding 'other' --installation 'other' --slot 'prompt_submit'"] {
            let (_home, lifecycle, selection) = fixture();
            lifecycle.install("claude_code", &selection, false).unwrap();
            fs::create_dir_all(selection.target.parent().unwrap()).unwrap();
            let foreign = serde_json::json!({"future":true,"hooks":{"FutureEvent":[{"future":7,"hooks":[{"type":"command","command":command}]}]}});
            let raw = serde_json::to_vec(&foreign).unwrap();
            fs::write(&selection.target, &raw).unwrap();
            let stamp = lifecycle.registry.read().unwrap().stamp().clone();
            assert_eq!(lifecycle.change_connection("claude_code", &selection, ConfigIntent::Enable, false).unwrap_err().reason, "conflicting_active_callbacks");
            assert_eq!(*lifecycle.registry.read().unwrap().stamp(), stamp);
            assert_eq!(fs::read(&selection.target).unwrap(), raw);
            fs::write(&selection.target, b"{}").unwrap();
            lifecycle.change_connection("claude_code", &selection, ConfigIntent::Enable, false).unwrap();
            let (binding, record) = installed_record(&lifecycle);
            let mut enabled: serde_json::Value = serde_json::from_slice(&fs::read(&selection.target).unwrap()).unwrap();
            enabled["hooks"]["FutureEvent"] = foreign["hooks"]["FutureEvent"].clone();
            fs::write(&selection.target, serde_json::to_vec(&enabled).unwrap()).unwrap();
            assert!(lifecycle.admit_callback(&binding, &record.installation_id, ConfigSlot::PromptSubmit, &selection.binary).is_err());
            assert!(lifecycle.install("claude_code", &selection, false).is_err());
            lifecycle.change_connection("claude_code", &selection, ConfigIntent::Disable, false).unwrap();
            let actual: serde_json::Value = serde_json::from_slice(&fs::read(&selection.target).unwrap()).unwrap();
            assert_eq!(actual["hooks"]["FutureEvent"], foreign["hooks"]["FutureEvent"]);
            assert_eq!(actual["hooks"].as_object().unwrap().len(), 1);
        }
    }

    #[test]
    fn partial_completed_callbacks_refuse_cleanup_without_removing_the_remaining_entries() {
        let (_home, lifecycle, selection) = fixture();
        lifecycle.install("claude_code", &selection, false).unwrap();
        lifecycle
            .change_connection("claude_code", &selection, ConfigIntent::Enable, false)
            .unwrap();
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&selection.target).unwrap()).unwrap();
        value["hooks"].as_object_mut().unwrap().remove("Stop");
        let raw = serde_json::to_vec(&value).unwrap();
        fs::write(&selection.target, &raw).unwrap();
        assert_eq!(
            lifecycle
                .change_connection("claude_code", &selection, ConfigIntent::Disable, false)
                .unwrap_err()
                .effect,
            RegistryEffect::EffectUnconfirmed
        );
        assert_eq!(fs::read(&selection.target).unwrap(), raw);
        let (_, record) = installed_record(&lifecycle);
        assert!(record.pending.is_some());
        assert!(!record.desired_enabled);
    }

    #[test]
    fn legacy_guard_preserves_metadata_only_registration_and_creates_no_canonical_state() {
        let (home, lifecycle, selection) = fixture();
        let mut value: serde_json::Value = serde_json::from_str(include_str!(
            "../../../protocol/contracts/host-adapter/v1/fixtures/valid-manifest-synthetic.json"
        ))
        .unwrap();
        value["adapter_id"] = serde_json::json!("future_fixture");
        let manifest = lifecycle
            .contract
            .validate_manifest(&serde_json::to_vec(&value).unwrap())
            .unwrap();
        let snapshot = lifecycle.registry.read().unwrap();
        lifecycle
            .registry
            .register(manifest, snapshot.stamp())
            .unwrap();
        let before = lifecycle.registry.read().unwrap().stamp().clone();
        let called = lifecycle
            .run_legacy_claude(&selection.target, true, |root| {
                fs::write(root.join("install.json"), b"legacy marker").unwrap();
                true
            })
            .unwrap();
        assert_eq!(called, Some(true));
        assert_eq!(*lifecycle.registry.read().unwrap().stamp(), before);
        assert!(lifecycle
            .registry
            .read()
            .unwrap()
            .document()
            .unwrap()
            .installations
            .is_empty());
        assert!(!home
            .path()
            .join("state/host-adapters/installations")
            .exists());
    }

    #[test]
    fn disable_wins_between_passive_enabled_observation_and_final_admission() {
        let (home, lifecycle, selection) = fixture();
        lifecycle.install("claude_code", &selection, false).unwrap();
        lifecycle
            .change_connection("claude_code", &selection, ConfigIntent::Enable, false)
            .unwrap();
        let (binding, record) = installed_record(&lifecycle);
        let disabling =
            ConfigLifecycle::new(home.path().join("state"), HostContract::load().unwrap());
        let selected = LocalInstallation {
            scope: selection.scope.clone(),
            profile: selection.profile.clone(),
            target: selection.target.clone(),
            binary: selection.binary.clone(),
        };
        ACTION.with(|slot| {
            *slot.borrow_mut() = Some((
                LifecycleBoundary::AfterPassiveEnabledObservation,
                Box::new(move || {
                    disabling
                        .change_connection("claude_code", &selected, ConfigIntent::Disable, false)
                        .unwrap();
                }),
            ))
        });
        let admitted = lifecycle.admit_callback(
            &binding,
            &record.installation_id,
            ConfigSlot::PromptSubmit,
            &selection.binary,
        );
        assert!(admitted.is_err() || admitted == Ok(false));
        let (_, current) = installed_record(&lifecycle);
        assert!(!current.desired_enabled);
        assert!(current.pending.is_none());
        for name in ["host_id", "daemon.log", "daemon.sock", "ledger.sqlite3"] {
            assert!(!home.path().join("state").join(name).exists());
        }
    }

    #[test]
    fn final_connection_bracket_refuses_postcommit_artifact_or_target_drift() {
        for modify_artifact in [false, true] {
            let (home, lifecycle, selection) = fixture();
            lifecycle.install("claude_code", &selection, false).unwrap();
            let (binding, record) = installed_record(&lifecycle);
            let path = if modify_artifact {
                artifact_path(lifecycle.registry.root().unwrap(), &record)
            } else {
                selection.target.clone()
            };
            let changed = path.clone();
            ACTION.with(|slot| {
                *slot.borrow_mut() = Some((
                    LifecycleBoundary::AfterFinalConnectionCatalogVerification,
                    Box::new(move || fs::write(changed, b"{\"later_user_change\":true}").unwrap()),
                ))
            });
            assert_eq!(
                lifecycle
                    .change_connection("claude_code", &selection, ConfigIntent::Enable, false)
                    .unwrap_err()
                    .effect,
                RegistryEffect::EffectUnconfirmed
            );
            assert_eq!(fs::read(path).unwrap(), b"{\"later_user_change\":true}");
            // Completion was actually committed; no stale metadata rollback is allowed.
            let (_, current) = installed_record(&lifecycle);
            assert!(current.desired_enabled);
            assert!(current.pending.is_none());
            assert!(lifecycle
                .admit_callback(
                    &binding,
                    &record.installation_id,
                    ConfigSlot::PromptSubmit,
                    &selection.binary
                )
                .is_err());
            assert_eq!(
                lifecycle
                    .inspect_installation("claude_code")
                    .unwrap()
                    .unwrap()
                    .local_connection,
                "unknown"
            );
            for name in ["host_id", "daemon.log", "daemon.sock", "ledger.sqlite3"] {
                assert!(!home.path().join("state").join(name).exists());
            }
        }
    }

    #[test]
    fn pending_enable_recovery_cannot_complete_while_the_target_reservation_is_held() {
        use std::os::unix::fs::MetadataExt;
        for boundary in [
            LifecycleBoundary::AfterConnectionIntent,
            LifecycleBoundary::AfterTargetReplacement,
        ] {
            let (_home, lifecycle, selection) = fixture();
            fs::create_dir_all(selection.target.parent().unwrap()).unwrap();
            fs::write(&selection.target, b"{\"future\":true}").unwrap();
            lifecycle.install("claude_code", &selection, false).unwrap();
            let interruption = Interruption::at(boundary);
            lifecycle
                .change_connection("claude_code", &selection, ConfigIntent::Enable, false)
                .unwrap_err();
            drop(interruption);
            let before = fs::read(&selection.target).unwrap();
            let inode = fs::metadata(&selection.target).unwrap().ino();
            let stamp = lifecycle.registry.read().unwrap().stamp().clone();
            let guard = crate::write_lock::acquire_private(&selection.target).unwrap();
            assert!(lifecycle
                .change_connection("claude_code", &selection, ConfigIntent::Enable, false)
                .is_err());
            assert_eq!(*lifecycle.registry.read().unwrap().stamp(), stamp);
            assert_eq!(fs::read(&selection.target).unwrap(), before);
            drop(guard);
            let result = lifecycle
                .change_connection("claude_code", &selection, ConfigIntent::Enable, false)
                .unwrap();
            assert_eq!(fs::read(&selection.target).unwrap(), before);
            assert_eq!(fs::metadata(&selection.target).unwrap().ino(), inode);
            if boundary == LifecycleBoundary::AfterConnectionIntent {
                assert_eq!(result.recovery, Some(RecoveryOutcome::OperationNotApplied));
                assert!(!result.desired_enabled);
            } else {
                assert!(result.desired_enabled);
            }
        }
    }

    #[test]
    fn admitted_callback_releases_reservation_before_handler_and_disable_prevents_later_admission()
    {
        let (_home, lifecycle, selection) = fixture();
        lifecycle.install("claude_code", &selection, false).unwrap();
        lifecycle
            .change_connection("claude_code", &selection, ConfigIntent::Enable, false)
            .unwrap();
        let (binding, record) = installed_record(&lifecycle);
        let (admitted_tx, admitted_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let lifecycle = &lifecycle;
            let selection = &selection;
            let binding = &binding;
            let record = &record;
            let handler = scope.spawn(move || {
                assert!(lifecycle
                    .admit_callback(
                        binding,
                        &record.installation_id,
                        ConfigSlot::TurnCompleted,
                        &selection.binary
                    )
                    .unwrap());
                admitted_tx.send(()).unwrap();
                // Models work after admission: it cannot hold the root guard.
                resume_rx
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap();
                true
            });
            admitted_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
            lifecycle
                .change_connection("claude_code", selection, ConfigIntent::Disable, false)
                .unwrap();
            assert!(!lifecycle
                .admit_callback(
                    binding,
                    &record.installation_id,
                    ConfigSlot::TurnCompleted,
                    &selection.binary
                )
                .unwrap());
            resume_tx.send(()).unwrap();
            assert!(handler.join().unwrap());
        });
    }

    #[test]
    fn enabled_gate_refuses_missing_or_replaced_reservation_without_recreation() {
        for replace_after_acquisition in [false, true] {
            let (_home, lifecycle, selection) = fixture();
            lifecycle.install("claude_code", &selection, false).unwrap();
            lifecycle
                .change_connection("claude_code", &selection, ConfigIntent::Enable, false)
                .unwrap();
            let (binding, record) = installed_record(&lifecycle);
            let lock = crate::write_lock::derive_lock_path(
                &super::super::state::reservation_target(lifecycle.registry.root().unwrap())
                    .unwrap(),
            )
            .unwrap();
            if replace_after_acquisition {
                let original = lock.clone();
                let moved = lock.with_extension("retained-lock");
                ACTION.with(|slot| {
                    *slot.borrow_mut() = Some((
                        LifecycleBoundary::BeforeFinalAdmission,
                        Box::new(move || {
                            fs::rename(&original, &moved).unwrap();
                            fs::write(&original, b"foreign reservation").unwrap();
                            use std::os::unix::fs::PermissionsExt;
                            fs::set_permissions(&original, fs::Permissions::from_mode(0o600))
                                .unwrap();
                        }),
                    ))
                });
            } else {
                fs::remove_file(&lock).unwrap();
            }
            assert!(lifecycle
                .admit_callback(
                    &binding,
                    &record.installation_id,
                    ConfigSlot::ToolCompleted,
                    &selection.binary
                )
                .is_err());
            if replace_after_acquisition {
                assert_eq!(fs::read(&lock).unwrap(), b"foreign reservation");
            } else {
                assert!(!lock.exists());
            }
        }
    }

    #[test]
    fn final_admission_refuses_target_drift_without_initializing_legacy_effects() {
        let (home, lifecycle, selection) = fixture();
        lifecycle.install("claude_code", &selection, false).unwrap();
        lifecycle
            .change_connection("claude_code", &selection, ConfigIntent::Enable, false)
            .unwrap();
        let (binding, record) = installed_record(&lifecycle);
        let target = selection.target.clone();
        ACTION.with(|slot| {
            *slot.borrow_mut() = Some((
                LifecycleBoundary::BeforeFinalAdmission,
                Box::new(move || {
                    fs::write(&target, b"{\"later_user_change\":true}").unwrap();
                }),
            ))
        });
        assert!(lifecycle
            .admit_callback(
                &binding,
                &record.installation_id,
                ConfigSlot::PromptSubmit,
                &selection.binary
            )
            .is_err());
        assert_eq!(
            fs::read(&selection.target).unwrap(),
            b"{\"later_user_change\":true}"
        );
        for name in ["host_id", "daemon.log", "daemon.sock", "ledger.sqlite3"] {
            assert!(!home.path().join("state").join(name).exists());
        }
    }

    #[test]
    fn recovery_artifact_removal_is_unconfirmed_if_completion_is_interrupted_again() {
        let (_home, lifecycle, selection) = fixture();
        lifecycle.install("claude_code", &selection, false).unwrap();
        let interruption = Interruption::at(LifecycleBoundary::AfterTargetReplacement);
        lifecycle
            .change_connection("claude_code", &selection, ConfigIntent::Uninstall, false)
            .unwrap_err();
        drop(interruption);
        let (_, record) = installed_record(&lifecycle);
        let path = artifact_path(lifecycle.registry.root().unwrap(), &record);
        assert!(path.exists());
        let interruption = Interruption::at(LifecycleBoundary::BeforeConnectionCompletion);
        assert_eq!(
            lifecycle
                .change_connection("claude_code", &selection, ConfigIntent::Uninstall, false)
                .unwrap_err()
                .effect,
            RegistryEffect::EffectUnconfirmed
        );
        drop(interruption);
        assert!(!path.exists());
        assert!(installed_record(&lifecycle).1.pending.is_some());
        assert!(
            !lifecycle
                .change_connection("claude_code", &selection, ConfigIntent::Uninstall, false)
                .unwrap()
                .installed
        );
    }

    #[test]
    fn interrupted_enable_reconciles_before_without_replay_and_after_without_rewrite() {
        for point in [
            LifecycleBoundary::AfterConnectionIntent,
            LifecycleBoundary::AfterTargetReplacement,
            LifecycleBoundary::BeforeConnectionCompletion,
        ] {
            let (_home, lifecycle, selection) = fixture();
            lifecycle.install("claude_code", &selection, false).unwrap();
            let interruption = Interruption::at(point);
            let failure = lifecycle
                .change_connection("claude_code", &selection, ConfigIntent::Enable, false)
                .unwrap_err();
            assert_eq!(failure.effect, RegistryEffect::EffectUnconfirmed);
            drop(interruption);
            let (binding, record) = installed_record(&lifecycle);
            assert!(!record.desired_enabled);
            assert!(record.pending.is_some());
            assert!(!lifecycle
                .admit_callback(
                    &binding,
                    &record.installation_id,
                    ConfigSlot::PromptSubmit,
                    &selection.binary
                )
                .unwrap());
            let before = fs::read(&selection.target).ok();
            let result = lifecycle
                .change_connection("claude_code", &selection, ConfigIntent::Enable, false)
                .unwrap();
            assert_eq!(fs::read(&selection.target).ok(), before);
            if point == LifecycleBoundary::AfterConnectionIntent {
                assert_eq!(result.recovery, Some(RecoveryOutcome::OperationNotApplied));
                assert!(!result.desired_enabled);
                assert!(!selection.target.exists());
            } else {
                assert!(result.desired_enabled);
                assert!(lifecycle
                    .admit_callback(
                        &binding,
                        &record.installation_id,
                        ConfigSlot::PromptSubmit,
                        &selection.binary
                    )
                    .unwrap());
            }
        }
    }

    #[test]
    fn ambiguous_enable_recovery_preserves_user_edit_and_pending_gate() {
        let (_home, lifecycle, selection) = fixture();
        lifecycle.install("claude_code", &selection, false).unwrap();
        let interruption = Interruption::at(LifecycleBoundary::AfterTargetReplacement);
        lifecycle
            .change_connection("claude_code", &selection, ConfigIntent::Enable, false)
            .unwrap_err();
        drop(interruption);
        let mut settings: serde_json::Value =
            serde_json::from_slice(&fs::read(&selection.target).unwrap()).unwrap();
        settings["future"] = serde_json::json!({"user":"keep"});
        let edited = serde_json::to_vec(&settings).unwrap();
        fs::write(&selection.target, &edited).unwrap();
        let stamp = lifecycle.registry.read().unwrap().stamp().clone();
        lifecycle
            .change_connection("claude_code", &selection, ConfigIntent::Enable, false)
            .unwrap_err();
        assert_eq!(fs::read(&selection.target).unwrap(), edited);
        assert_eq!(*lifecycle.registry.read().unwrap().stamp(), stamp);
        let (binding, record) = installed_record(&lifecycle);
        assert!(!lifecycle
            .admit_callback(
                &binding,
                &record.installation_id,
                ConfigSlot::TurnCompleted,
                &selection.binary
            )
            .unwrap());
        assert_eq!(
            lifecycle
                .change_connection("claude_code", &selection, ConfigIntent::Disable, false)
                .unwrap_err()
                .reason,
            "pending_operation"
        );
    }

    #[test]
    fn disable_closes_before_failed_artifact_acquisition_and_can_finish_explicitly() {
        let (_home, lifecycle, selection) = fixture();
        lifecycle.install("claude_code", &selection, false).unwrap();
        lifecycle
            .change_connection("claude_code", &selection, ConfigIntent::Enable, false)
            .unwrap();
        let (binding, record) = installed_record(&lifecycle);
        let path = artifact_path(lifecycle.registry.root().unwrap(), &record);
        let original = fs::read(&path).unwrap();
        fs::write(&path, b"foreign").unwrap();
        let before = fs::read(&selection.target).unwrap();
        assert_eq!(
            lifecycle
                .change_connection("claude_code", &selection, ConfigIntent::Disable, false)
                .unwrap_err()
                .effect,
            RegistryEffect::EffectUnconfirmed
        );
        let (_, pending) = installed_record(&lifecycle);
        assert!(!pending.desired_enabled);
        assert!(pending.pending.as_ref().unwrap().phase == IntentPhase::GateClosed);
        assert!(!lifecycle
            .admit_callback(
                &binding,
                &record.installation_id,
                ConfigSlot::ToolCompleted,
                &selection.binary
            )
            .unwrap());
        assert_eq!(fs::read(&selection.target).unwrap(), before);
        assert_eq!(fs::read(&path).unwrap(), b"foreign");
        fs::write(&path, original).unwrap();
        let result = lifecycle
            .change_connection("claude_code", &selection, ConfigIntent::Disable, false)
            .unwrap();
        assert!(!result.desired_enabled);
        assert_eq!(result.local_connection, "disconnected");
    }

    #[test]
    fn disable_prepared_before_and_after_recovery_preserves_foreign_settings() {
        for point in [
            LifecycleBoundary::AfterGateClosed,
            LifecycleBoundary::AfterConnectionIntent,
            LifecycleBoundary::AfterTargetReplacement,
            LifecycleBoundary::BeforeConnectionCompletion,
        ] {
            let (_home, lifecycle, selection) = fixture();
            fs::create_dir_all(selection.target.parent().unwrap()).unwrap();
            fs::write(&selection.target, b"{\"future\":{\"owner\":\"user\"}}").unwrap();
            lifecycle.install("claude_code", &selection, false).unwrap();
            lifecycle
                .change_connection("claude_code", &selection, ConfigIntent::Enable, false)
                .unwrap();
            let interruption = Interruption::at(point);
            lifecycle
                .change_connection("claude_code", &selection, ConfigIntent::Disable, false)
                .unwrap_err();
            drop(interruption);
            let (binding, record) = installed_record(&lifecycle);
            assert!(!lifecycle
                .admit_callback(
                    &binding,
                    &record.installation_id,
                    ConfigSlot::PromptSubmit,
                    &selection.binary
                )
                .unwrap());
            let result = lifecycle
                .change_connection("claude_code", &selection, ConfigIntent::Disable, false)
                .unwrap();
            assert_eq!(result.local_connection, "disconnected");
            let actual: serde_json::Value =
                serde_json::from_slice(&fs::read(&selection.target).unwrap()).unwrap();
            assert_eq!(actual, serde_json::json!({"future":{"owner":"user"}}));
        }
    }

    #[test]
    fn uninstall_recovers_owned_artifact_removal_and_never_recreates_missing_host_parent() {
        for point in [
            LifecycleBoundary::AfterConnectionIntent,
            LifecycleBoundary::AfterTargetReplacement,
            LifecycleBoundary::BeforeConnectionCompletion,
        ] {
            let (home, lifecycle, selection) = fixture();
            lifecycle.install("claude_code", &selection, false).unwrap();
            let interruption = Interruption::at(point);
            lifecycle
                .change_connection("claude_code", &selection, ConfigIntent::Uninstall, false)
                .unwrap_err();
            drop(interruption);
            assert!(!home.path().join("home").exists());
            let result = lifecycle
                .change_connection("claude_code", &selection, ConfigIntent::Uninstall, false)
                .unwrap();
            assert!(!result.installed);
            assert!(!home.path().join("home").exists());
            assert!(lifecycle
                .registry
                .read()
                .unwrap()
                .document()
                .unwrap()
                .installations
                .is_empty());
            assert_eq!(
                fs::read_dir(home.path().join("state/host-adapters/installations"))
                    .unwrap()
                    .count(),
                0
            );
        }
    }

    #[test]
    fn copied_unproved_callbacks_and_partial_owned_set_never_authorize_cleanup() {
        let (_home, lifecycle, selection) = fixture();
        lifecycle.install("claude_code", &selection, false).unwrap();
        let (binding, record) = installed_record(&lifecycle);
        fs::create_dir_all(selection.target.parent().unwrap()).unwrap();
        let foreign = config_settings::connect(
            &serde_json::json!({"future":true}),
            &callbacks(&record, &binding),
            true,
        )
        .unwrap();
        let raw = serde_json::to_vec(&foreign).unwrap();
        fs::write(&selection.target, &raw).unwrap();
        assert_eq!(
            lifecycle
                .change_connection("claude_code", &selection, ConfigIntent::Uninstall, false)
                .unwrap_err()
                .reason,
            "unproved_callback_ownership"
        );
        assert_eq!(fs::read(&selection.target).unwrap(), raw);
        // A partial set remains ambiguous even when exact commands are present.
        let mut partial = foreign;
        partial["hooks"].as_object_mut().unwrap().remove("Stop");
        let raw = serde_json::to_vec(&partial).unwrap();
        fs::write(&selection.target, &raw).unwrap();
        assert_eq!(
            lifecycle
                .change_connection("claude_code", &selection, ConfigIntent::Enable, false)
                .unwrap_err()
                .reason,
            "partial_owned_callbacks"
        );
        assert_eq!(fs::read(&selection.target).unwrap(), raw);
        assert!(installed_record(&lifecycle).1.pending.is_none());
    }

    #[test]
    fn real_install_produces_exact_artifact_and_closed_admission_without_host_state() {
        let (home, lifecycle, selection) = fixture();
        let result = lifecycle.install("claude_code", &selection, false).unwrap();
        assert!(result.installed);
        assert!(!result.desired_enabled);
        let snapshot = lifecycle.registry.read().unwrap();
        let document = snapshot.document().unwrap();
        let (binding, record) = document.installations.iter().next().unwrap();
        let artifact = verify_artifact(document, binding, record).unwrap();
        assert_eq!(
            artifact.raw().unwrap(),
            record
                .artifact_bytes(&document.registry_id, binding)
                .unwrap()
        );
        assert!(!selection.target.exists());
        assert!(!home.path().join("state/host_id").exists());
        assert!(!home.path().join("state/daemon.sock").exists());
        assert!(!home.path().join("state/ledger.sqlite3").exists());
        assert!(!lifecycle
            .admit_callback(
                binding,
                &record.installation_id,
                ConfigSlot::PromptSubmit,
                &selection.binary
            )
            .unwrap());
        let stamp = snapshot.stamp().clone();
        let repeated = lifecycle.install("claude_code", &selection, false).unwrap();
        assert_eq!(repeated.effect, "no_change");
        assert_eq!(*lifecycle.registry.read().unwrap().stamp(), stamp);
    }

    #[test]
    fn dry_run_and_missing_callback_do_not_initialize_anything() {
        let (home, lifecycle, selection) = fixture();
        let result = lifecycle.install("claude_code", &selection, true).unwrap();
        assert!(!result.installed);
        assert!(!home.path().join("state").exists());
        let binding = uuid::Uuid::new_v4().to_string();
        let installation = uuid::Uuid::new_v4().to_string();
        assert!(!lifecycle
            .admit_callback(
                &binding,
                &installation,
                ConfigSlot::TurnCompleted,
                &selection.binary
            )
            .unwrap());
        assert_eq!(fs::read_dir(home.path()).unwrap().count(), 1);
    }

    #[test]
    fn same_install_refuses_package_artifact_and_context_drift() {
        let (_home, lifecycle, mut selection) = fixture();
        lifecycle.install("claude_code", &selection, false).unwrap();
        let snapshot = lifecycle.registry.read().unwrap();
        let document = snapshot.document().unwrap();
        let (_, record) = document.installations.iter().next().unwrap();
        let artifact = artifact_path(lifecycle.registry.root().unwrap(), record);
        let original = fs::read(&artifact).unwrap();
        fs::write(&artifact, b"foreign modified artifact").unwrap();
        assert_eq!(
            lifecycle
                .install("claude_code", &selection, false)
                .unwrap_err()
                .reason,
            "artifact_changed"
        );
        assert_eq!(fs::read(&artifact).unwrap(), b"foreign modified artifact");
        fs::write(&artifact, original).unwrap();
        fs::write(&selection.binary, b"different executable").unwrap();
        assert_eq!(
            lifecycle
                .install("claude_code", &selection, false)
                .unwrap_err()
                .reason,
            "installation_context_changed"
        );
        selection.target = selection.target.with_file_name("other.json");
        assert!(lifecycle.install("claude_code", &selection, false).is_err());
    }

    #[test]
    fn interrupted_install_is_closed_and_retry_proves_before_or_after_without_replay() {
        for point in [
            LifecycleBoundary::AfterInstallIntent,
            LifecycleBoundary::AfterArtifactCreation,
            LifecycleBoundary::BeforeInstallCompletion,
        ] {
            let (_home, lifecycle, selection) = fixture();
            let interrupt = Interruption::at(point);
            let failure = lifecycle
                .install("claude_code", &selection, false)
                .unwrap_err();
            assert_eq!(failure.effect, RegistryEffect::EffectUnconfirmed);
            drop(interrupt);
            let snapshot = lifecycle.registry.read().unwrap();
            let document = snapshot.document().unwrap();
            let (binding, record) = document.installations.iter().next().unwrap();
            assert!(!record.installed);
            assert!(!record.desired_enabled);
            assert!(record.pending.is_some());
            assert!(!lifecycle
                .admit_callback(
                    binding,
                    &record.installation_id,
                    ConfigSlot::PromptSubmit,
                    &selection.binary
                )
                .unwrap());
            let artifact = artifact_path(lifecycle.registry.root().unwrap(), record);
            let exists = artifact.exists();
            assert_eq!(exists, point != LifecycleBoundary::AfterInstallIntent);
            let result = lifecycle.install("claude_code", &selection, false).unwrap();
            assert_eq!(result.installed, exists);
            assert_eq!(
                result.recovery,
                if exists {
                    None
                } else {
                    Some(RecoveryOutcome::OperationNotApplied)
                }
            );
            assert!(!result.desired_enabled);
            assert_eq!(artifact.exists(), exists);
            assert!(!selection.target.exists());
        }
    }

    #[test]
    fn interrupted_install_refuses_foreign_artifact_without_changing_pending_state() {
        let (_home, lifecycle, selection) = fixture();
        let interrupt = Interruption::at(LifecycleBoundary::AfterArtifactCreation);
        assert!(lifecycle.install("claude_code", &selection, false).is_err());
        drop(interrupt);
        let snapshot = lifecycle.registry.read().unwrap();
        let document = snapshot.document().unwrap();
        let (_, record) = document.installations.iter().next().unwrap();
        let artifact = artifact_path(lifecycle.registry.root().unwrap(), record);
        fs::write(&artifact, b"user changed artifact").unwrap();
        let stamp = snapshot.stamp().clone();
        assert_eq!(
            lifecycle
                .install("claude_code", &selection, false)
                .unwrap_err()
                .reason,
            "pending_artifact_conflict"
        );
        assert_eq!(lifecycle.registry.read().unwrap().stamp(), &stamp);
        assert_eq!(fs::read(&artifact).unwrap(), b"user changed artifact");
    }
}
