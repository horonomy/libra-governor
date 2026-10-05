//! Explicit, freshly authorized external diagnostics. Probe claims confer no
//! host, policy, installation, identity or economic authority.

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use uuid::Uuid;

use super::catalog::RegistryRecord;
use super::contract::{HostContract, ValidatedManifest};
use super::exec::{self, ExecFailure, VerifiedLaunch};
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
pub struct DiagnosticDispatcher {
    registry: AdapterRegistry,
    contract: HostContract,
    #[cfg(test)]
    after_copy: Option<Box<dyn FnOnce()>>,
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
        let max_request_bytes = context.manifest.value()["input_limits"]["max_bytes"]
            .as_f64()
            // The validated integer range is at most 2^20: every accepted
            // value has an exact f64/usize representation, including 1024.0.
            .filter(|limit| (1.0..=1_048_576.0).contains(limit) && limit.fract() == 0.0)
            .map(|limit| limit as usize)
            .ok_or_else(|| fail("selection", "input limit refused"))?;
        // Recheck after request/FD preparation, immediately before admission.
        self.recheck(context)?;
        let response = exec::run(
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
            reason: match error {
                ExecFailure::Cancelled => "diagnostic cancelled",
                ExecFailure::Timeout => "request deadline exceeded",
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
        let handshake = json!({"protocol":"horonom.host-adapter", "offered_versions":[1], "request_id":Uuid::new_v4().to_string(), "operation":"handshake"});
        let hello = self.invoke(&context, handshake, cancellation)?;
        if hello.get("error").is_some() || hello["selected_version"].as_f64() != Some(1.0) {
            return Err(DiagnosticFailure {
                stage: "protocol",
                reason: "handshake refused",
                execution_attempted: true,
            });
        }
        let request = json!({"protocol":"horonom.host-adapter", "protocol_version":1, "host_contract_version":1, "request_id":Uuid::new_v4().to_string(), "operation":"probe", "input":{"context":context.wire_context}, "configuration":context.configuration});
        let response = self
            .invoke(&context, request, cancellation)
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
        self.recheck(&context).map_err(|mut error| {
            error.execution_attempted = true;
            error
        })?;
        let result = CandidateProbe {
            adapter_id: id.to_owned(),
            candidate: candidate.clone(),
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
