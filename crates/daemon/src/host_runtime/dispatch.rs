//! Explicit, freshly authorized external diagnostics. Probe claims confer no
//! host, policy, installation, identity or economic authority.

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::host_binding::{self, HostBindingOutcome, LibraNativeContext};
use libra_governor_domain::ExecutionIdentity;
use libra_governor_protocol::host_event::{
    validate_host_event, HostEventSource, ValidatedHostEvent,
};
use serde_json::{json, Value};
use uuid::Uuid;

use super::catalog::RegistryRecord;
use super::contract::{HostContract, ValidatedManifest};
use super::exec::{self, ExecFailure, TimeoutPhase, VerifiedLaunch};
use super::identity::measure_identity;
use super::state::AdapterRegistry;

pub use super::exec::Cancellation;

#[derive(Clone, Copy, Debug)]
pub enum DiagnosticScope {
    User,
    Project,
}

pub struct DiagnosticSelection {
    pub scope: DiagnosticScope,
    /// Bounded JSON validated against the registered configuration profile.
    pub configuration: Vec<u8>,
}

/// Explicit diagnostic assertions, not authenticated host identity. No reader
/// or initializer obtains identity on the caller's behalf.
pub struct NativeNormalizationInput {
    pub host_id: String,
    pub observed_at: String,
    pub source: HostEventSource,
    pub native_payload: Vec<u8>,
}

/// Validated adapter claims and pure binding candidates; never an admission
/// receipt, reusable host context or authorization to perform product effects.
pub struct CandidateNormalization {
    adapter_id: String,
    events: Vec<ValidatedHostEvent>,
    bindings: Vec<HostBindingOutcome>,
}

impl CandidateNormalization {
    pub fn adapter_id(&self) -> &str {
        &self.adapter_id
    }
    pub fn events(&self) -> &[ValidatedHostEvent] {
        &self.events
    }
    pub fn bindings(&self) -> &[HostBindingOutcome] {
        &self.bindings
    }
    pub fn verification_state(&self) -> &'static str {
        "unverified"
    }
}

/// Fixed local diagnostics never retain driver errors, payloads or paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagnosticFailure {
    pub stage: &'static str,
    pub reason: &'static str,
    /// The runner was invoked; this is not a claim that exec completed or that
    /// explicitly trusted code had no ambient side effects.
    pub execution_attempted: bool,
}

impl std::fmt::Display for DiagnosticFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "host adapter {}: {}", self.stage, self.reason)
    }
}

impl std::error::Error for DiagnosticFailure {}

/// Validated candidate shape, deliberately without Serialize or Debug.
pub struct CandidateProbe {
    adapter_id: String,
    candidate: Value,
}

impl CandidateProbe {
    pub fn adapter_id(&self) -> &str {
        &self.adapter_id
    }

    pub fn capability_count(&self) -> usize {
        self.candidate["capabilities"]
            .as_array()
            .map_or(0, Vec::len)
    }

    fn finish(
        self,
        cancellation: &Cancellation,
        expires: Instant,
    ) -> Result<Self, DiagnosticFailure> {
        let reason = if cancellation.cancelled() {
            Some(("execution", "diagnostic cancelled"))
        } else if Instant::now() >= expires {
            Some(("context", "diagnostic context expired"))
        } else {
            None
        };
        if let Some((stage, reason)) = reason {
            return Err(DiagnosticFailure {
                stage,
                reason,
                execution_attempted: true,
            });
        }
        Ok(self)
    }
}

/// No live attempt is retained between calls. A call returns only after its
/// lexical runner has verified owned leader/group cleanup, including errors.
/// The caller must keep SIGPIPE ignored and its default SIGCHLD disposition
/// stable, and must not let a foreign reaper consume these owned children.
/// Unsupported observed signal policy is refused before fork; this library
/// does not synchronize foreign process-global policy changes.
/// Portable pipe setup also requires callers to exclude concurrent foreign
/// process launches; its private fork lock coordinates only owned runners.
pub struct DiagnosticDispatcher {
    registry: AdapterRegistry,
    contract: HostContract,
    #[cfg(test)]
    after_copy: Option<Box<dyn FnOnce()>>,
    #[cfg(test)]
    expire_after_copy: bool,
}

struct Context {
    manifest: ValidatedManifest,
    fingerprint: Value,
    implementation: String,
    provider: String,
    configuration: Value,
    wire_context: Value,
    cwd: WorkingDirectory,
    expires: Instant,
}

pub(super) struct PreparedConfigPlan {
    context: Context,
    handshake: Value,
    request: Value,
}

struct WorkingDirectory {
    locator: PathBuf,
    canonical: PathBuf,
    file: File,
    identity: (u64, u64, u32, u32),
}

fn fail(stage: &'static str, reason: &'static str) -> DiagnosticFailure {
    DiagnosticFailure {
        stage,
        reason,
        execution_attempted: false,
    }
}

fn attempted(mut failure: DiagnosticFailure) -> DiagnosticFailure {
    failure.execution_attempted = true;
    failure
}

fn request_limit(manifest: &ValidatedManifest) -> Result<usize, DiagnosticFailure> {
    manifest.value()["input_limits"]["max_bytes"]
        .as_f64()
        .filter(|limit| (1.0..=1_048_576.0).contains(limit) && limit.fract() == 0.0)
        .map(|limit| limit as usize)
        .ok_or_else(|| fail("selection", "input limit refused"))
}

impl WorkingDirectory {
    fn capture(locator: PathBuf) -> Result<Self, DiagnosticFailure> {
        let canonical = fs::canonicalize(&locator)
            .map_err(|_| fail("context", "working directory unavailable"))?;
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&canonical)
            .map_err(|_| fail("context", "working directory unavailable"))?;
        let metadata = file
            .metadata()
            .map_err(|_| fail("context", "working directory unavailable"))?;
        // SAFETY: geteuid reads this process's current effective user identity.
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o022 != 0
        {
            return Err(fail("context", "working directory ownership refused"));
        }
        let identity = (
            metadata.dev(),
            metadata.ino(),
            metadata.uid(),
            metadata.mode(),
        );
        let cwd = Self {
            locator,
            canonical,
            file,
            identity,
        };
        cwd.check()?;
        Ok(cwd)
    }

    fn check(&self) -> Result<(), DiagnosticFailure> {
        if fs::canonicalize(&self.locator).ok().as_ref() != Some(&self.canonical) {
            return Err(fail("context", "working directory changed"));
        }
        for metadata in [self.file.metadata(), fs::symlink_metadata(&self.canonical)] {
            let metadata = metadata.map_err(|_| fail("context", "working directory changed"))?;
            if !metadata.is_dir()
                || (
                    metadata.dev(),
                    metadata.ino(),
                    metadata.uid(),
                    metadata.mode(),
                ) != self.identity
            {
                return Err(fail("context", "working directory changed"));
            }
        }
        Ok(())
    }
}

impl DiagnosticDispatcher {
    pub fn new(registry: AdapterRegistry, contract: HostContract) -> Self {
        Self {
            registry,
            contract,
            #[cfg(test)]
            after_copy: None,
            #[cfg(test)]
            expire_after_copy: false,
        }
    }

    fn selected(&self, id: &str) -> Result<(RegistryRecord, Value), DiagnosticFailure> {
        if matches!(id, "claude_code" | "codex") {
            return Err(fail("selection", "builtin diagnostics unavailable"));
        }
        let snapshot = self
            .registry
            .read()
            .map_err(|_| fail("registry", "registry verification failed"))?;
        let document = snapshot
            .document()
            .ok_or_else(|| fail("selection", "unknown adapter"))?;
        let record = document
            .adapters
            .get(id)
            .ok_or_else(|| fail("selection", "unknown adapter"))?;
        let trust = record
            .trust
            .as_ref()
            .ok_or_else(|| fail("trust", "explicit code trust required"))?;
        let fingerprint = json!({
            "registry_id":document.registry_id,
            "registration_revision":record.registration_revision,
            "trust_revision":record.trust_revision,
            "manifest":record.manifest.raw(),
            "manifest_digest":trust.manifest_digest,
            "implementation_digest":trust.implementation_digest,
            "confirmation_digest":trust.confirmation_digest,
            "confirmed_at":trust.confirmed_at,
        });
        Ok((record.clone(), fingerprint))
    }

    fn prepare(
        &self,
        id: &str,
        selection: DiagnosticSelection,
    ) -> Result<Context, DiagnosticFailure> {
        let expires = Instant::now() + Duration::from_secs(60);
        let (record, fingerprint) = self.selected(id)?;
        if !record.manifest.compatible() {
            return Err(fail("selection", "adapter protocol incompatible"));
        }
        let manifest = record.manifest;
        let declaration = manifest.value();
        let needs = &declaration["needs"];
        let environments = needs["environment"]
            .as_array()
            .ok_or_else(|| fail("selection", "invalid declared needs"))?;
        if environments
            .iter()
            .any(|key| !matches!(key.as_str(), Some("LANG" | "LC_ALL")))
            || needs["read_paths"]
                .as_array()
                .is_none_or(|paths| !paths.is_empty())
            || needs["write_paths"]
                .as_array()
                .is_none_or(|paths| !paths.is_empty())
        {
            return Err(fail("selection", "declared permissions not granted"));
        }
        let providers = declaration["host_version_constraints"]
            .as_array()
            .ok_or_else(|| fail("selection", "host provider unavailable"))?;
        if providers.len() != 1 {
            return Err(fail("selection", "host provider ambiguous"));
        }
        let provider = providers[0]["provider"]
            .as_str()
            .ok_or_else(|| fail("selection", "host provider unavailable"))?
            .to_owned();
        let configuration = self
            .contract
            .validate_settings(&manifest, &selection.configuration)
            .map_err(|_| fail("selection", "configuration refused"))?;
        let locator = match selection.scope {
            DiagnosticScope::User => self
                .registry
                .root()
                .map_err(|_| fail("registry", "registry verification failed"))?
                .join("host-adapters"),
            DiagnosticScope::Project => std::env::current_dir()
                .map_err(|_| fail("context", "working directory unavailable"))?,
        };
        let cwd = WorkingDirectory::capture(locator)?;
        let wire_context = match selection.scope {
            DiagnosticScope::User => json!({"scope":"host"}),
            DiagnosticScope::Project => {
                json!({"scope":"project_worktree", "project_ref":cwd.canonical.to_str().ok_or_else(|| fail("context", "working directory encoding refused"))?})
            }
        };
        let implementation = measure_identity(&manifest)
            .map_err(|_| fail("trust", "current code verification failed"))?;
        let trust = record
            .trust
            .ok_or_else(|| fail("trust", "explicit code trust required"))?;
        if implementation != trust.implementation_digest
            || manifest.digest() != trust.manifest_digest
        {
            return Err(fail("trust", "recorded code trust does not match"));
        }
        let context = Context {
            manifest,
            fingerprint,
            implementation,
            provider,
            configuration,
            wire_context,
            cwd,
            expires,
        };
        self.recheck(&context)?;
        Ok(context)
    }

    fn recheck(&self, context: &Context) -> Result<(), DiagnosticFailure> {
        if Instant::now() >= context.expires {
            return Err(fail("context", "diagnostic context expired"));
        }
        let (_, before) = self.selected(context.manifest.adapter_id())?;
        if before != context.fingerprint {
            return Err(fail("trust", "registration or trust changed"));
        }
        if measure_identity(&context.manifest)
            .map_err(|_| fail("trust", "current code verification failed"))?
            != context.implementation
        {
            return Err(fail("trust", "registered code changed"));
        }
        let (_, after) = self.selected(context.manifest.adapter_id())?;
        if after != context.fingerprint {
            return Err(fail("trust", "registration or trust changed"));
        }
        context.cwd.check()?;
        if Instant::now() >= context.expires {
            return Err(fail("context", "diagnostic context expired"));
        }
        Ok(())
    }

    fn invoke(
        &self,
        context: &Context,
        request: Value,
        cancellation: &Cancellation,
    ) -> Result<Value, DiagnosticFailure> {
        self.recheck(context)?;
        if cancellation.cancelled() {
            return Err(fail("execution", "diagnostic cancelled"));
        }
        let raw = serde_json::to_vec(&request)
            .map_err(|_| fail("protocol", "request encoding failed"))?;
        let validated = self
            .contract
            .validate_request(&raw)
            .map_err(|_| fail("protocol", "request refused"))?;
        let launch = &context.manifest.value()["launch"];
        let executable = CString::new(
            launch["executable"]
                .as_str()
                .ok_or_else(|| fail("selection", "launch refused"))?,
        )
        .map_err(|_| fail("selection", "launch refused"))?;
        let arguments = launch["argv"]
            .as_array()
            .ok_or_else(|| fail("selection", "launch refused"))?
            .iter()
            .map(|value| {
                CString::new(
                    value
                        .as_str()
                        .ok_or_else(|| fail("selection", "launch refused"))?,
                )
                .map_err(|_| fail("selection", "launch refused"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let directory = context
            .cwd
            .file
            .try_clone()
            .map_err(|_| fail("context", "working directory unavailable"))?;
        let max_request_bytes = request_limit(&context.manifest)?;
        // Recheck after request/FD preparation, immediately before admission.
        self.recheck(context)?;
        let response = exec::run_diagnostic(
            VerifiedLaunch {
                executable,
                arguments,
                directory,
                max_request_bytes,
            },
            &raw,
            cancellation,
        )
        .map_err(|error| DiagnosticFailure {
            stage: "execution",
            reason: match error.kind {
                ExecFailure::Cancelled => "diagnostic cancelled",
                ExecFailure::Timeout => match error.timeout_phase {
                    Some(TimeoutPhase::NotOwned) => "request deadline exceeded (not_owned)",
                    Some(TimeoutPhase::OwnedAwaitingReady) => {
                        "request deadline exceeded (owned_awaiting_ready)"
                    }
                    Some(TimeoutPhase::GroupReadyAwaitingExecStatus) => {
                        "request deadline exceeded (group_ready_awaiting_exec_status)"
                    }
                    Some(TimeoutPhase::ExecStatusClosed) => {
                        "request deadline exceeded (exec_status_closed)"
                    }
                    None => "request deadline exceeded",
                },
                ExecFailure::InputLimit => "request limit exceeded",
                ExecFailure::OutputLimit => "response limit exceeded",
                ExecFailure::ErrorLimit => "stderr limit exceeded",
                ExecFailure::NonzeroExit => "adapter exited unsuccessfully",
                ExecFailure::SignalContext => "caller signal policy refused",
                ExecFailure::Setup
                | ExecFailure::Exec
                | ExecFailure::Io
                | ExecFailure::OwnershipInterference => "owned execution failed",
            },
            execution_attempted: true,
        })?;
        let mark_attempted = |mut failure: DiagnosticFailure| {
            failure.execution_attempted = true;
            failure
        };
        self.recheck(context).map_err(mark_attempted)?;
        if cancellation.cancelled() {
            return Err(mark_attempted(fail("execution", "diagnostic cancelled")));
        }
        self.contract
            .validate_response(&validated, &response)
            .map_err(|_| mark_attempted(fail("protocol", "response correlation refused")))
    }

    fn accept_handshake(
        &self,
        context: &Context,
        request: Value,
        cancellation: &Cancellation,
    ) -> Result<(), DiagnosticFailure> {
        let hello = self.invoke(context, request, cancellation)?;
        if hello.get("error").is_some() || hello["selected_version"].as_f64() != Some(1.0) {
            return Err(attempted(fail("protocol", "handshake refused")));
        }
        Ok(())
    }

    pub(super) fn prepare_config_plan(
        &self,
        id: &str,
        selection: DiagnosticSelection,
        provider: &str,
        profile: &str,
        validator_ref: Value,
        request: Value,
    ) -> Result<PreparedConfigPlan, DiagnosticFailure> {
        if !matches!(selection.scope, DiagnosticScope::User) {
            return Err(fail("selection", "planning scope refused"));
        }
        let mut context = self.prepare(id, selection)?;
        if context.provider != provider
            || !context.manifest.value()["roles"]
                .as_array()
                .is_some_and(|roles| roles.iter().any(|role| role == "ConfigDriver"))
        {
            return Err(fail("selection", "planning role or provider refused"));
        }
        context.wire_context = json!({"scope":"host","profile":profile});
        let handshake = json!({"protocol":"horonom.host-adapter", "offered_versions":[1], "request_id":Uuid::new_v4().to_string(), "operation":"handshake"});
        let request = json!({"protocol":"horonom.host-adapter","protocol_version":1,"host_contract_version":1,"request_id":Uuid::new_v4().to_string(),"operation":"plan_config","input":{"product_id":"libra-governor","validator_ref":validator_ref,"context":context.wire_context,"request":request},"configuration":context.configuration});
        let limit = request_limit(&context.manifest)?;
        // Both requests are entirely known before execution. Refuse oversized
        // planning input before even the handshake may run.
        for value in [&handshake, &request] {
            let bytes = serde_json::to_vec(value)
                .map_err(|_| fail("protocol", "request encoding failed"))?;
            if bytes.len() > limit {
                return Err(fail("execution", "request limit exceeded"));
            }
            self.contract
                .validate_request(&bytes)
                .map_err(|_| fail("protocol", "request refused"))?;
        }
        self.recheck(&context)?;
        Ok(PreparedConfigPlan {
            context,
            handshake,
            request,
        })
    }

    pub(super) fn check_config_plan(
        &self,
        prepared: &PreparedConfigPlan,
        cancellation: &Cancellation,
    ) -> Result<(), DiagnosticFailure> {
        self.finish_config_plan(prepared, cancellation)?;
        self.recheck(&prepared.context)
    }

    pub(super) fn finish_config_plan(
        &self,
        prepared: &PreparedConfigPlan,
        cancellation: &Cancellation,
    ) -> Result<(), DiagnosticFailure> {
        if cancellation.cancelled() {
            return Err(fail("execution", "diagnostic cancelled"));
        }
        if Instant::now() >= prepared.context.expires {
            return Err(fail("context", "diagnostic context expired"));
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn expire_config_plan_for_test(
        &self,
        mut prepared: PreparedConfigPlan,
    ) -> PreparedConfigPlan {
        prepared.context.expires = Instant::now().min(prepared.context.expires);
        prepared
    }

    pub(super) fn plan_config_registered(
        &self,
        prepared: &PreparedConfigPlan,
        cancellation: &Cancellation,
        check_local: impl Fn() -> Result<(), DiagnosticFailure>,
    ) -> Result<Value, DiagnosticFailure> {
        self.check_config_plan(prepared, cancellation)?;
        check_local()?;
        self.accept_handshake(&prepared.context, prepared.handshake.clone(), cancellation)?;
        check_local().map_err(attempted)?;
        self.check_config_plan(prepared, cancellation)
            .map_err(attempted)?;
        check_local().map_err(attempted)?;
        let response = self
            .invoke(&prepared.context, prepared.request.clone(), cancellation)
            .map_err(attempted)?;
        let result = &response["result"];
        let reference = &prepared.request["input"]["validator_ref"];
        if response.get("error").is_some()
            || result["product_id"] != prepared.request["input"]["product_id"]
            || result["validator_ref"]["id"] != reference["id"]
            || result["validator_ref"]["digest"] != reference["digest"]
            || result["validator_ref"]["version"].as_f64() != reference["version"].as_f64()
        {
            return Err(attempted(fail(
                "profile",
                "planner result identity refused",
            )));
        }
        check_local().map_err(attempted)?;
        self.check_config_plan(prepared, cancellation)
            .map_err(attempted)?;
        check_local().map_err(attempted)?;
        self.finish_config_plan(prepared, cancellation)
            .map_err(attempted)?;
        Ok(result["plan"].clone())
    }

    fn probe_candidate(
        &self,
        context: &Context,
        cancellation: &Cancellation,
    ) -> Result<Value, DiagnosticFailure> {
        let handshake = json!({"protocol":"horonom.host-adapter", "offered_versions":[1], "request_id":Uuid::new_v4().to_string(), "operation":"handshake"});
        self.accept_handshake(context, handshake, cancellation)?;
        let request = json!({"protocol":"horonom.host-adapter", "protocol_version":1, "host_contract_version":1, "request_id":Uuid::new_v4().to_string(), "operation":"probe", "input":{"context":context.wire_context}, "configuration":context.configuration});
        let response = self
            .invoke(context, request, cancellation)
            .map_err(|mut error| {
                error.execution_attempted = true;
                error
            })?;
        let candidate = &response["result"]["snapshot"];
        let expected_adapter = json!({"id":context.manifest.adapter_id(), "version":context.manifest.value()["adapter_version"], "manifest_digest":context.manifest.digest(), "implementation_digest":context.implementation});
        if response.get("error").is_some()
            || candidate["adapter"] != expected_adapter
            || candidate["host"]["tool_provider"] != context.provider
            || candidate["context"] != context.wire_context
        {
            return Err(DiagnosticFailure {
                stage: "protocol",
                reason: "candidate identity or context refused",
                execution_attempted: true,
            });
        }
        self.recheck(context).map_err(|mut error| {
            error.execution_attempted = true;
            error
        })?;
        Ok(candidate.clone())
    }

    /// Normalize explicit diagnostic input into an all-or-nothing batch of
    /// unverified adapter claims and pure Libra binding candidates. There is
    /// no native context override or product effect path.
    pub fn normalize_registered(
        &mut self,
        id: &str,
        selection: DiagnosticSelection,
        input: NativeNormalizationInput,
        cancellation: &Cancellation,
    ) -> Result<CandidateNormalization, DiagnosticFailure> {
        if cancellation.cancelled() {
            return Err(fail("execution", "diagnostic cancelled"));
        }
        let source = &input.source;
        let lengths = [
            input.host_id.len(),
            input.observed_at.len(),
            input.native_payload.len(),
            source.native_event_name.len(),
            source.native_schema_ref.as_ref().map_or(0, String::len),
            source.native_event_id.as_ref().map_or(0, String::len),
            source.replay_key.as_ref().map_or(0, String::len),
        ];
        if lengths.into_iter().fold(0_usize, usize::saturating_add) > 1_048_576 {
            return Err(fail("input", "normalization input refused"));
        }
        let source = serde_json::to_value(source)
            .map_err(|_| fail("input", "normalization input refused"))?;
        let observation = self
            .contract
            .validate_normalization_observation(
                &input.native_payload,
                &input.host_id,
                &input.observed_at,
                source.clone(),
            )
            .map_err(|_| fail("input", "normalization input refused"))?;
        let context = self.prepare(id, selection)?;
        // Reuse the owning identity timestamp parser only as a private
        // comparison intermediate; this is not an observed identity.
        let identity: ExecutionIdentity = serde_json::from_value(json!({
            "envelope_version":1, "host_id":input.host_id, "observed_at":input.observed_at,
            "tool_provider":context.provider, "lineage_status":"unknown"
        }))
        .map_err(|_| fail("input", "normalization input refused"))?;
        let mut request = json!({"protocol":"horonom.host-adapter", "protocol_version":1,
            "host_contract_version":1, "request_id":Uuid::new_v4().to_string(), "operation":"normalize",
            "input":observation, "configuration":context.configuration});
        // The snapshot is not known yet. Check the complete known lower
        // bound, then check the actual request after the fresh probe.
        let lower = serde_json::to_vec(&request)
            .map_err(|_| fail("input", "normalization request refused"))?;
        let limit = request_limit(&context.manifest)?;
        if lower.len() > limit || self.contract.check_protocol_bounds(&lower).is_err() {
            return Err(fail("input", "normalization request refused"));
        }
        let snapshot = self.probe_candidate(&context, cancellation)?;
        request["input"]["capability_snapshot"] = snapshot.clone();
        let raw = serde_json::to_vec(&request)
            .map_err(|_| attempted(fail("input", "normalization request refused")))?;
        if raw.len() > limit || self.contract.validate_request(&raw).is_err() {
            return Err(attempted(fail("input", "normalization request refused")));
        }
        let response = self
            .invoke(&context, request, cancellation)
            .map_err(attempted)?;
        if response.get("error").is_some() {
            return Err(attempted(fail("protocol", "normalization refused")));
        }
        let returned = response["result"]["events"]
            .as_array()
            .ok_or_else(|| attempted(fail("protocol", "normalization refused")))?;
        let mut events = Vec::with_capacity(returned.len());
        for value in returned {
            let raw = serde_json::to_vec(value)
                .map_err(|_| attempted(fail("protocol", "normalization refused")))?;
            let event = validate_host_event(&raw)
                .map_err(|_| attempted(fail("protocol", "normalization refused")))?;
            if event.identity().host_id() != input.host_id
                || event.identity().tool_provider() != context.provider
                || event.identity().observed_at() != identity.observed_at()
                || event.adapter_id() != context.manifest.adapter_id()
                || Some(event.adapter_version()) != snapshot["adapter"]["version"].as_str()
                || Some(event.capability_snapshot_id()) != snapshot["snapshot_id"].as_str()
                || event.host_version() != snapshot["host"]["version"].as_str()
                || serde_json::to_value(event.source()).ok().as_ref() != Some(&source)
                || serde_json::to_value(event.scope()).ok().as_ref()
                    != Some(&context.wire_context["scope"])
            {
                return Err(attempted(fail(
                    "protocol",
                    "normalized event correlation refused",
                )));
            }
            events.push(event);
        }
        let bindings = events
            .iter()
            .cloned()
            .map(|event| host_binding::bind(event, LibraNativeContext::None))
            .collect();
        self.recheck(&context).map_err(attempted)?;
        let result = CandidateNormalization {
            adapter_id: id.to_owned(),
            events,
            bindings,
        };
        #[cfg(test)]
        if let Some(after_copy) = self.after_copy.take() {
            after_copy();
        }
        if cancellation.cancelled() {
            return Err(attempted(fail("execution", "diagnostic cancelled")));
        }
        let expires = context.expires;
        // Shorten only the final test deadline after genuine child execution;
        // no public context or execution authority can be fabricated here.
        #[cfg(test)]
        let expires = if self.expire_after_copy {
            Instant::now().min(expires)
        } else {
            expires
        };
        if Instant::now() >= expires {
            return Err(attempted(fail("context", "diagnostic context expired")));
        }
        Ok(result)
    }

    pub fn probe_registered(
        &mut self,
        id: &str,
        selection: DiagnosticSelection,
        cancellation: &Cancellation,
    ) -> Result<CandidateProbe, DiagnosticFailure> {
        if cancellation.cancelled() {
            return Err(fail("execution", "diagnostic cancelled"));
        }
        let context = self.prepare(id, selection)?;
        let candidate = self.probe_candidate(&context, cancellation)?;
        let result = CandidateProbe {
            adapter_id: id.to_owned(),
            candidate,
        };
        #[cfg(test)]
        if let Some(after_copy) = self.after_copy.take() {
            after_copy();
        }
        result.finish(cancellation, context.expires)
    }
}

#[cfg(test)]
#[path = "dispatch_tests.rs"]
mod late_controls;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_or_expiry_after_candidate_copy_prevents_return() {
        for cancelled in [true, false] {
            let cancellation = Cancellation::default();
            let candidate = CandidateProbe {
                adapter_id: "final_boundary_fixture".into(),
                candidate: json!({"capabilities":[]}),
            };
            // This terminal boundary runs after the protocol candidate copy;
            // it grants no registration/protocol authority to the fixture.
            let expires = if cancelled {
                cancellation.cancel();
                Instant::now() + Duration::from_secs(60)
            } else {
                Instant::now() - Duration::from_secs(1)
            };
            let failure = candidate.finish(&cancellation, expires).err().unwrap();
            assert_eq!(
                failure.stage,
                if cancelled { "execution" } else { "context" }
            );
            assert_eq!(
                failure.reason,
                if cancelled {
                    "diagnostic cancelled"
                } else {
                    "diagnostic context expired"
                }
            );
            assert!(failure.execution_attempted);
        }
    }
}
