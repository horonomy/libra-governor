//! Passive metadata and explicit local-code-trust CLI for host adapters.

use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use libra_governor_daemon::host_runtime::config_lifecycle::{ConfigLifecycle, LocalInstallation};
use libra_governor_daemon::host_runtime::config_profile::ConfigIntent;
use libra_governor_daemon::host_runtime::contract::{HostContract, ValidatedManifest};
use libra_governor_daemon::host_runtime::dispatch::{
    Cancellation, DiagnosticDispatcher, DiagnosticScope, DiagnosticSelection,
};
use libra_governor_daemon::host_runtime::state::{AdapterRegistry, RegistrySnapshot};
use libra_governor_daemon::host_runtime::{RegistryEffect, RegistryFailure};
use serde_json::{json, Map, Value};

const HELP: &str = "Usage:\n  libra-governor adapter list [--json]\n  libra-governor adapter inspect <id> [--review-code-trust] [--json]\n  libra-governor adapter capabilities <id> [--json]\n  libra-governor adapter status [id] [--json]\n  libra-governor adapter doctor [id] [--json]\n  libra-governor adapter doctor <id> --probe [--scope user|project] [--json]\n  libra-governor adapter explain <id> [--json]\n  libra-governor adapter register <manifest-path> [--confirm-code-digest <sha256:...>] [--dry-run] [--json]\n  libra-governor adapter unregister <id> [--dry-run] [--json]\n  libra-governor adapter install|uninstall|enable|disable <id> --profile <name> [--scope user|project] [--dry-run] [--json]\n\nRegistry metadata is passive. Code-trust review hashes declared files but does not execute them. `adapter doctor <id> --probe` executes explicitly trusted external code with ambient authority; the probe itself grants no host capability or native persistent-statusline support.";

#[derive(Clone, Copy, Debug)]
enum ProbeScope {
    User,
    Project,
}

#[derive(Debug)]
struct Command {
    operation: &'static str,
    id: Option<String>,
    path: Option<PathBuf>,
    json: bool,
    dry_run: bool,
    review: bool,
    confirmation: Option<String>,
    probe: bool,
    scope: Option<ProbeScope>,
    profile: Option<String>,
    unavailable: bool,
}

pub fn run(args: &[String]) -> i32 {
    if args.is_empty() || (args.len() == 1 && matches!(args[0].as_str(), "--help" | "-h" | "help"))
    {
        println!("{HELP}");
        return 0;
    }
    let parsed = match parse(args) {
        Ok(command) => command,
        Err((operation, json_output)) => {
            return emit_early(operation, None, json_output, "invalid_arguments", 2);
        }
    };
    if parsed.operation == "help" {
        println!("{HELP}");
        return 0;
    }
    execute(parsed)
}

fn parse(args: &[String]) -> Result<Command, (&'static str, bool)> {
    let operation = match args.first().map(String::as_str).unwrap_or("") {
        "list" => "list",
        "inspect" => "inspect",
        "capabilities" => "capabilities",
        "status" => "status",
        "doctor" => "doctor",
        "explain" => "explain",
        "register" => "register",
        "unregister" => "unregister",
        "install" => "install",
        "uninstall" => "uninstall",
        "enable" => "enable",
        "disable" => "disable",
        _ => return Err(("list", args.iter().any(|arg| arg == "--json"))),
    };
    let unavailable = matches!(operation, "install" | "uninstall" | "enable" | "disable");
    let mut command = Command {
        operation,
        id: None,
        path: None,
        json: false,
        dry_run: false,
        review: false,
        confirmation: None,
        probe: false,
        scope: None,
        profile: None,
        unavailable,
    };
    let mut index = 1;
    if matches!(operation, "list") {
        // No positional arguments are meaningful for list.
    } else if operation == "register" {
        let Some(path) = args.get(index) else {
            return Err((operation, args.iter().any(|arg| arg == "--json")));
        };
        if path.starts_with('-') {
            return Err((operation, args.iter().any(|arg| arg == "--json")));
        }
        command.path = Some(PathBuf::from(path));
        index += 1;
    } else if operation == "status" || operation == "doctor" {
        if let Some(value) = args.get(index).filter(|value| !value.starts_with('-')) {
            command.id = Some(value.clone());
            index += 1;
        }
    } else {
        let Some(value) = args.get(index) else {
            return Err((operation, args.iter().any(|arg| arg == "--json")));
        };
        if value.starts_with('-') {
            return Err((operation, args.iter().any(|arg| arg == "--json")));
        }
        if unavailable
            || matches!(
                operation,
                "unregister" | "inspect" | "capabilities" | "explain"
            )
        {
            command.id = Some(value.clone());
        } else {
            return Err((operation, args.iter().any(|arg| arg == "--json")));
        }
        index += 1;
    }

    while index < args.len() {
        match args[index].as_str() {
            "--json" if !command.json => command.json = true,
            "--dry-run"
                if !command.dry_run
                    && (matches!(operation, "register" | "unregister") || unavailable) =>
            {
                command.dry_run = true
            }
            "--review-code-trust" if !command.review && operation == "inspect" => {
                command.review = true
            }
            "--confirm-code-digest"
                if operation == "register" && command.confirmation.is_none() =>
            {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err((operation, args.iter().any(|arg| arg == "--json")));
                };
                command.confirmation = Some(value.clone());
            }
            "--probe" if operation == "doctor" && !command.probe => command.probe = true,
            "--scope" if (operation == "doctor" || unavailable) && command.scope.is_none() => {
                index += 1;
                command.scope = match args.get(index).map(String::as_str) {
                    Some("user") => Some(ProbeScope::User),
                    Some("project") => Some(ProbeScope::Project),
                    _ => return Err((operation, args.iter().any(|arg| arg == "--json"))),
                };
            }
            "--profile" if unavailable && command.profile.is_none() => {
                index += 1;
                let Some(value) = args
                    .get(index)
                    .filter(|value| !value.starts_with('-') && value.len() <= 128)
                else {
                    return Err((operation, args.iter().any(|arg| arg == "--json")));
                };
                command.profile = Some(value.clone());
            }
            _ => return Err((operation, args.iter().any(|arg| arg == "--json"))),
        }
        index += 1;
    }
    if command.scope.is_some() && !command.probe && !unavailable {
        return Err((operation, command.json));
    }
    if command.probe
        && (operation != "doctor" || command.id.as_deref().is_none_or(|id| !valid_id(id)))
    {
        return Err((operation, command.json));
    }
    Ok(command)
}

fn execute(command: Command) -> i32 {
    let contract = match HostContract::load() {
        Ok(contract) => contract,
        Err(_) => {
            eprintln!("libra-governor: adapter contract resources unavailable");
            return 2;
        }
    };
    if validate_sample(&contract, &command).is_err() {
        eprintln!("libra-governor: adapter contract resources unavailable");
        return 2;
    }
    let root = match libra_governor_daemon::paths::state_dir() {
        Ok(root) => root,
        Err(_) => {
            return emit(
                &contract,
                &command,
                "failed",
                "state_unavailable",
                json!({}),
                "failed",
                2,
            );
        }
    };
    let registry = AdapterRegistry::new(root.clone(), contract.clone());
    if command.probe {
        return execute_probe(&command, &contract, registry);
    }
    let result = if matches!(
        command.operation,
        "install" | "enable" | "disable" | "uninstall"
    ) && command.profile.is_some()
    {
        execute_install(&command, root, &contract)
    } else if command.unavailable {
        Err(RegistryFailure::new("operation", "operation unavailable"))
    } else {
        dispatch(&command, &contract, &registry, &root)
    };
    match result {
        Ok((outcome, reason, body, verification, status)) => emit(
            &contract,
            &command,
            outcome,
            reason,
            body,
            verification,
            status,
        ),
        Err(failure) => {
            let (outcome, reason, verification, status) = failure_output(&failure);
            emit(
                &contract,
                &command,
                outcome,
                reason,
                json!({"effect": effect_name(failure.effect), "effect_scope": if command.profile.is_some() {"installation_and_configuration"} else {"registry_and_trust"},
                    "filesystem_effect": if !command.dry_run && (matches!(command.operation,"register"|"unregister") || command.profile.is_some()) {"not_asserted"} else {"unchanged"}}),
                verification,
                status,
            )
        }
    }
}

type DispatchResult =
    Result<(&'static str, &'static str, Value, &'static str, i32), RegistryFailure>;

fn execute_install(command: &Command, root: PathBuf, contract: &HostContract) -> DispatchResult {
    let selection = LocalInstallation {
        scope: scope_name(command.scope.unwrap_or(ProbeScope::User)).into(),
        profile: command.profile.clone().unwrap(),
        target: crate::claude_settings::settings_path()
            .map_err(|_| RegistryFailure::new("lifecycle", "configuration_locator_unavailable"))?,
        binary: std::env::current_exe()
            .map_err(|_| RegistryFailure::new("lifecycle", "consumer_unavailable"))?,
    };
    let result = ConfigLifecycle::new(root, contract.clone()).change_connection(
        command.id.as_deref().unwrap_or(""),
        &selection,
        match command.operation {
            "install" => ConfigIntent::Install,
            "enable" => ConfigIntent::Enable,
            "disable" => ConfigIntent::Disable,
            "uninstall" => ConfigIntent::Uninstall,
            _ => unreachable!(),
        },
        command.dry_run,
    )?;
    let mut body = serde_json::to_value(result)
        .map_err(|_| RegistryFailure::new("lifecycle", "result_unavailable"))?;
    body["dry_run"] = json!(command.dry_run);
    Ok(("success", "", body, "unverified", 0))
}

fn dispatch(
    command: &Command,
    contract: &HostContract,
    registry: &AdapterRegistry,
    root: &Path,
) -> DispatchResult {
    match command.operation {
        "list" | "status" | "doctor" => {
            let snapshot = registry.read()?;
            if let Some(id) = command.id.as_deref() {
                if describe(&snapshot, id).is_none() {
                    return Err(RegistryFailure::new("registry", "unknown adapter"));
                }
            }
            let mut body = render_list(&snapshot, command.id.as_deref());
            if matches!(command.operation, "status" | "doctor")
                && command.id.as_deref().is_none_or(|id| id == "claude_code")
            {
                let local = ConfigLifecycle::new(root.to_owned(), contract.clone())
                    .inspect_installation("claude_code")?;
                body["local_installations"] = json!(local.into_iter().collect::<Vec<_>>());
            }
            Ok(("success", "", body, "unverified", 0))
        }
        "inspect" | "capabilities" | "explain" => {
            let snapshot = registry.read()?;
            let id = command.id.as_deref().unwrap_or("");
            if command.operation == "inspect" && command.review {
                return inspect_review(registry, &snapshot, id);
            }
            let Some(value) = describe(&snapshot, id) else {
                return Err(RegistryFailure::new("registry", "unknown adapter"));
            };
            let body = if command.operation == "capabilities" {
                json!({"adapter_id": safe(id), "capabilities": value["capabilities"], "effective_support": "unknown"})
            } else if command.operation == "explain" {
                json!({"adapter_id": id, "summary": "passive adapter metadata only", "native_lifecycle": "unavailable", "effective_support": "unknown"})
            } else {
                value
            };
            Ok(("success", "", body, "unverified", 0))
        }
        "register" => register(command, contract, registry),
        "unregister" => unregister(command, registry),
        _ => Err(RegistryFailure::new("operation", "operation unavailable")),
    }
}

fn execute_probe(command: &Command, contract: &HostContract, registry: AdapterRegistry) -> i32 {
    let mut signals = match crate::adapter_probe_signal::ProbeSignals::install() {
        Ok(signals) => signals,
        Err(()) => {
            return emit(
                contract,
                command,
                "refused",
                "signal_setup_refused",
                json!({"execution_attempted":false,"filesystem_effect":"unchanged"}),
                "failed",
                2,
            );
        }
    };
    let cancellation: Cancellation = signals.cancellation();
    let id = command.id.as_deref().unwrap_or("");
    let selection = DiagnosticSelection {
        scope: match command.scope.unwrap_or(ProbeScope::User) {
            ProbeScope::User => DiagnosticScope::User,
            ProbeScope::Project => DiagnosticScope::Project,
        },
        configuration: b"{}".to_vec(),
    };
    let result = DiagnosticDispatcher::new(registry, contract.clone()).probe_registered(
        id,
        selection,
        &cancellation,
    );
    let execution_attempted = result
        .as_ref()
        .map(|_| true)
        .unwrap_or_else(|failure| failure.execution_attempted);

    // probe_registered owns runner cleanup and returns only after the process
    // group is settled; restore handlers before emitting a CLI response.
    match signals.restore() {
        Ok(true) => {
            return emit(
                contract,
                command,
                "failed",
                "probe_cancelled",
                json!({"execution_attempted":execution_attempted,"filesystem_effect": if execution_attempted {"not_asserted"} else {"unchanged"}}),
                "failed",
                2,
            );
        }
        Ok(false) => {}
        Err(()) => {
            return emit(
                contract,
                command,
                "failed",
                "signal_restore_failed",
                json!({"execution_attempted":execution_attempted,"filesystem_effect":"not_asserted"}),
                "failed",
                2,
            );
        }
    }

    match result {
        Ok(candidate) => emit(
            contract,
            command,
            "partial",
            "candidate_protocol_validated",
            json!({
                "adapter_id": safe(candidate.adapter_id()),
                "capability_count": candidate.capability_count(),
                "candidate_protocol": "validated",
                "native_effect": "unverified",
                "host_trust": "unknown",
                "effective_support": "unknown",
            }),
            "unverified",
            0,
        ),
        Err(failure) => {
            let reason = diagnostic_reason(failure.stage, failure.reason);
            let (outcome, filesystem_effect, status) = if failure.execution_attempted {
                ("failed", "not_asserted", 2)
            } else {
                ("refused", "unchanged", 1)
            };
            emit(
                contract,
                command,
                outcome,
                reason,
                json!({"execution_attempted":failure.execution_attempted,"filesystem_effect":filesystem_effect,
                    "execution_failure": execution_failure_category(failure.stage, failure.reason)}),
                "failed",
                status,
            )
        }
    }
}

fn execution_failure_category(stage: &str, reason: &str) -> Option<&'static str> {
    if stage != "execution" {
        return None;
    }
    Some(match reason {
        "diagnostic cancelled" => "cancelled",
        "request deadline exceeded" => "timeout",
        "request limit exceeded" => "input_limit",
        "response limit exceeded" => "output_limit",
        "stderr limit exceeded" => "stderr_limit",
        "adapter exited unsuccessfully" => "nonzero_exit",
        "caller signal policy refused" => "signal_context",
        _ => "owned_execution_failed",
    })
}

fn diagnostic_reason(stage: &str, reason: &str) -> &'static str {
    match (stage, reason) {
        ("selection", "unknown adapter") => "unknown_adapter",
        ("selection", "builtin diagnostics unavailable") => "operation_refused",
        ("trust", _) => "code_trust_refused",
        ("registry", _) => "registry_unavailable",
        ("context", _) => "context_refused",
        ("configuration", _) => "configuration_refused",
        ("protocol", _) => "protocol_refused",
        ("execution", _) => "probe_execution_failed",
        _ => "probe_refused",
    }
}

fn register(
    command: &Command,
    contract: &HostContract,
    registry: &AdapterRegistry,
) -> DispatchResult {
    let path = command.path.as_deref().ok_or_else(|| invalid("manifest"))?;
    let raw = read_manifest(path).map_err(|_| invalid("manifest"))?;
    let manifest = contract.validate_manifest(&raw)?;
    let snapshot = registry.read()?;
    let existing = snapshot
        .document()
        .and_then(|document| document.adapters().get(manifest.adapter_id()));
    if let Some(confirmation) = command.confirmation.as_deref() {
        let Some(record) = existing else {
            return Err(RegistryFailure::new(
                "code_trust",
                "adapter must already be registered",
            ));
        };
        if record.manifest().raw() != raw {
            return Err(RegistryFailure::new(
                "code_trust",
                "registered manifest changed",
            ));
        }
        let review = registry.review_trust(manifest.adapter_id())?;
        if review.confirmation_digest() != confirmation {
            return Err(RegistryFailure::new(
                "code_trust",
                "confirmation digest mismatch",
            ));
        }
        registry.check_trust_confirmation(&snapshot, &review)?;
        if command.dry_run {
            return Ok((
                "success",
                "",
                json!({"action":"confirm_adapter_code_trust","registration_changed":false,"dry_run":true,"manifest_digest":review.manifest_digest(),"implementation_digest":review.implementation_digest(),"confirmation_digest":review.confirmation_digest(),"identity_scope":review.identity_scope(),"recorded_match":review.recorded_match()}),
                "unverified",
                0,
            ));
        }
        let effect = registry.confirm_trust(
            manifest.adapter_id(),
            manifest.digest(),
            confirmation,
            snapshot.stamp(),
        )?;
        return Ok((
            "success",
            "",
            json!({"action":"confirm_adapter_code_trust","registration_changed":false,"effect":effect_name(effect),"manifest_digest":review.manifest_digest(),"implementation_digest":review.implementation_digest(),"identity_scope":review.identity_scope()}),
            "verified",
            0,
        ));
    }
    registry.check_registration(&snapshot, &manifest)?;
    if command.dry_run {
        return Ok((
            "success",
            "",
            json!({"action":"register_adapter","registration_changed":true,"dry_run":true,"manifest_digest":manifest.digest(),"adapter_id":manifest.adapter_id()}),
            "not_applicable",
            0,
        ));
    }
    let effect = registry.register(manifest.clone(), snapshot.stamp())?;
    Ok((
        "success",
        "",
        json!({"action":"register_adapter","registration_changed":true,"effect":effect_name(effect),"manifest_digest":manifest.digest(),"adapter_id":manifest.adapter_id()}),
        "verified",
        0,
    ))
}

fn unregister(command: &Command, registry: &AdapterRegistry) -> DispatchResult {
    let id = command.id.as_deref().unwrap_or("");
    let snapshot = registry.read()?;
    let Some(record) = snapshot.document().and_then(|doc| doc.adapters().get(id)) else {
        return Err(RegistryFailure::new("registry", "unknown adapter"));
    };
    let manifest_digest = record.manifest().digest().to_owned();
    registry.check_unregistration(&snapshot, id)?;
    if command.dry_run {
        return Ok((
            "success",
            "",
            json!({"action":"unregister_adapter","registration_changed":true,"dry_run":true,"manifest_digest":manifest_digest,"adapter_id":id}),
            "not_applicable",
            0,
        ));
    }
    let effect = registry.unregister(id, snapshot.stamp())?;
    Ok((
        "success",
        "",
        json!({"action":"unregister_adapter","registration_changed":true,"effect":effect_name(effect),"manifest_digest":manifest_digest,"adapter_id":id}),
        "verified",
        0,
    ))
}

fn inspect_review(
    registry: &AdapterRegistry,
    snapshot: &RegistrySnapshot,
    id: &str,
) -> DispatchResult {
    if snapshot
        .document()
        .and_then(|doc| doc.adapters().get(id))
        .is_none()
    {
        return Err(RegistryFailure::new("registry", "unknown adapter"));
    }
    let review = registry.review_trust(id)?;
    Ok((
        "success",
        "",
        json!({"adapter_id":safe(id),"manifest_digest":review.manifest_digest(),"implementation_digest":review.implementation_digest(),"confirmation_digest":review.confirmation_digest(),"identity_scope":review.identity_scope(),"recorded_match":review.recorded_match()}),
        "unverified",
        0,
    ))
}

fn render_list(snapshot: &RegistrySnapshot, selected: Option<&str>) -> Value {
    let mut adapters = builtin_ids()
        .into_iter()
        .filter(|id| selected.is_none_or(|selected| selected == *id))
        .map(|id| {
            json!({"adapter_id":id,"origin":"builtin","registered":false,"adapter_version":"metadata-only","roles":[],"capabilities":[],"effective_support":"unknown"})
        })
        .collect::<Vec<_>>();
    if let Some(document) = snapshot.document() {
        adapters.extend(
            document
                .adapters()
                .iter()
                .filter(|(id, _)| selected.is_none_or(|selected| selected == id.as_str()))
                .map(|(id, record)| {
                    manifest_summary(id, record.manifest(), record.trust().is_some())
                }),
        );
    }
    json!({"adapters":adapters,"registry_initialized":snapshot.document().is_some()})
}

fn describe(snapshot: &RegistrySnapshot, id: &str) -> Option<Value> {
    if builtin_ids().contains(&id) {
        return Some(
            json!({"adapter_id":id,"origin":"builtin","registered":false,"adapter_version":"metadata-only","roles":[],"capabilities":[],"effective_support":"unknown","native_lifecycle":"unavailable","code_trust":"not_applicable"}),
        );
    }
    let document = snapshot.document()?;
    let record = document.adapters().get(id)?;
    Some(manifest_summary(
        id,
        record.manifest(),
        record.trust().is_some(),
    ))
}

fn manifest_summary(id: &str, manifest: &ValidatedManifest, trusted: bool) -> Value {
    let value = manifest.value();
    let roles = value["roles"].as_array().cloned().unwrap_or_default();
    let capabilities = value["capabilities"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    json!({
        "adapter_id": safe(id),
        "origin": "registered",
        "registered": true,
        "adapter_version": value["adapter_version"].as_str().map(safe),
        "manifest_digest": manifest.digest(),
        "contract_compatible": manifest.compatible(),
        "roles": roles.into_iter().filter_map(|v| v.as_str().map(safe)).collect::<Vec<_>>(),
        "capabilities": capabilities.into_iter().filter_map(|v| v.as_str().map(safe)).collect::<Vec<_>>(),
        "code_trust": if trusted { "recorded" } else { "not_reviewed" },
        "effective_support": "unknown",
        "native_lifecycle": "unavailable"
    })
}

fn read_manifest(path: &Path) -> std::io::Result<Vec<u8>> {
    let before_path = fs::symlink_metadata(path)?;
    if !before_path.file_type().is_file() || before_path.len() > 65_536 {
        return Err(std::io::Error::other("unsafe manifest"));
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(path)?;
    let before_fd = file.metadata()?;
    if !stable_file(&before_path, &before_fd)
        || !before_fd.file_type().is_file()
        || before_fd.len() > 65_536
    {
        return Err(std::io::Error::other("changed manifest"));
    }
    let mut raw = Vec::with_capacity(4096);
    (&mut file).take(65_537).read_to_end(&mut raw)?;
    if raw.len() > 65_536 {
        return Err(std::io::Error::other("manifest too large"));
    }
    let after_fd = file.metadata()?;
    let after_path = fs::symlink_metadata(path)?;
    if !stable_file(&before_fd, &after_fd)
        || !stable_file(&before_fd, &after_path)
        || before_fd.len() != raw.len() as u64
        || before_fd.mtime() != after_fd.mtime()
        || before_fd.mtime_nsec() != after_fd.mtime_nsec()
    {
        return Err(std::io::Error::other("manifest changed"));
    }
    Ok(raw)
}

fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino() && left.file_type().is_file()
}

fn stable_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    same_file(left, right)
        && left.len() == right.len()
        && left.mode() == right.mode()
        && left.uid() == right.uid()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

fn emit_early(
    operation: &str,
    id: Option<&str>,
    json_output: bool,
    reason: &'static str,
    status: i32,
) -> i32 {
    if !json_output {
        eprintln!("libra-governor: adapter request refused ({reason})");
        return status;
    }
    let Ok(contract) = HostContract::load() else {
        eprintln!("libra-governor: adapter contract resources unavailable");
        return 2;
    };
    let command = Command {
        operation: operation_for_schema(operation),
        id: id.map(str::to_owned),
        path: None,
        json: true,
        dry_run: false,
        review: false,
        confirmation: None,
        probe: false,
        scope: None,
        unavailable: false,
        profile: None,
    };
    emit(
        &contract,
        &command,
        "refused",
        reason,
        json!({}),
        "failed",
        status,
    )
}

fn validate_sample(contract: &HostContract, command: &Command) -> Result<(), RegistryFailure> {
    let sample = envelope(
        command.operation,
        command.id.as_deref(),
        response_scope(command),
        "success",
        "",
        json!({}),
        "unverified",
    );
    contract.validate_cli_envelope(&sample)
}

fn emit(
    contract: &HostContract,
    command: &Command,
    outcome: &'static str,
    reason: &'static str,
    body: Value,
    verification: &'static str,
    status: i32,
) -> i32 {
    let value = envelope(
        command.operation,
        command.id.as_deref(),
        response_scope(command),
        outcome,
        reason,
        body,
        verification,
    );
    if contract.validate_cli_envelope(&value).is_err() {
        eprintln!("libra-governor: adapter response unavailable");
        return 2;
    }
    if command.json {
        println!("{value}");
    } else {
        println!("{}: {}", command.operation, outcome);
        if !reason.is_empty() {
            println!("reason: {reason}");
        }
        println!("verification: {verification}");
        print_body(&value["result"], 0);
    }
    status
}

fn envelope(
    operation: &str,
    id: Option<&str>,
    scope: Option<&'static str>,
    outcome: &str,
    reason: &str,
    body: Value,
    verification: &str,
) -> Value {
    let mut value = Map::new();
    value.insert("schema_version".into(), json!(1));
    value.insert("operation".into(), json!(operation_for_schema(operation)));
    if let Some(id) = id.filter(|id| valid_id(id)) {
        value.insert("adapter_id".into(), json!(safe(id)));
    }
    if let Some(scope) = scope {
        value.insert("scope".into(), json!(scope));
    }
    value.insert("outcome".into(), json!(outcome));
    value.insert(
        "reasons".into(),
        json!(if reason.is_empty() {
            Vec::<String>::new()
        } else {
            vec![reason.to_owned()]
        }),
    );
    value.insert("result".into(), body);
    value.insert("verification_state".into(), json!(verification));
    Value::Object(value)
}

fn scope_name(scope: ProbeScope) -> &'static str {
    match scope {
        ProbeScope::User => "user",
        ProbeScope::Project => "project",
    }
}

fn response_scope(command: &Command) -> Option<&'static str> {
    (command.probe || command.profile.is_some())
        .then(|| scope_name(command.scope.unwrap_or(ProbeScope::User)))
}

fn print_body(value: &Value, depth: usize) {
    let indent = "  ".repeat(depth + 1);
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                match child {
                    Value::Array(_) | Value::Object(_) => {
                        println!("{indent}{}:", safe(key));
                        print_body(child, depth + 1);
                    }
                    _ => println!(
                        "{indent}{}: {}",
                        safe(key),
                        safe(child.to_string().trim_matches('"'))
                    ),
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                if item.is_object() {
                    println!("{indent}-");
                    print_body(item, depth + 1);
                } else {
                    println!("{indent}- {}", safe(item.to_string().trim_matches('"')));
                }
            }
        }
        _ => println!("{indent}{}", safe(value.to_string().trim_matches('"'))),
    }
}

fn safe(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        let code = ch as u32;
        if ch.is_control()
            || matches!(code, 0x061c | 0x200e..=0x200f | 0x202a..=0x202e | 0x2066..=0x2069)
        {
            out.push_str(&format!("\\u{{{code:04x}}}"));
        } else {
            out.push(ch);
        }
    }
    out
}

fn failure_output(failure: &RegistryFailure) -> (&'static str, &'static str, &'static str, i32) {
    if failure.effect == RegistryEffect::EffectUnconfirmed {
        return ("unknown", "effect_unconfirmed", "failed", 1);
    }
    let reason = match (failure.stage, failure.reason) {
        ("operation", _) => "operation_unavailable",
        ("lifecycle", "pending_operation") => "pending_operation",
        ("lifecycle", "disable_required") => "disable_required",
        ("lifecycle", "unsupported_installation_profile") => "unsupported_profile",
        ("lifecycle", "consumer_identity_changed") => "consumer_identity_changed",
        ("resource_integrity", _) => "resource_integrity",
        ("manifest", _) => "invalid_manifest",
        ("identity", "identity_limit") => "identity_limit",
        ("identity", _) => "identity_unavailable",
        ("registry", "unknown adapter") => "unknown_adapter",
        ("registry", "adapter already registered") => "adapter_conflict",
        ("registry", "registry lock unavailable") => "registry_busy",
        ("code_trust", _) => "code_trust_refused",
        _ => "operation_refused",
    };
    ("refused", reason, "failed", 1)
}

fn effect_name(effect: RegistryEffect) -> &'static str {
    match effect {
        RegistryEffect::NoChange => "no_change",
        RegistryEffect::AppliedVerified => "applied_verified",
        RegistryEffect::EffectUnconfirmed => "effect_unconfirmed",
    }
}

fn invalid(stage: &'static str) -> RegistryFailure {
    RegistryFailure::new(stage, "invalid input")
}

fn operation_for_schema(operation: &str) -> &'static str {
    match operation {
        "list" => "list",
        "inspect" => "inspect",
        "capabilities" => "capabilities",
        "register" => "register",
        "unregister" => "unregister",
        "install" => "install",
        "uninstall" => "uninstall",
        "enable" => "enable",
        "disable" => "disable",
        "status" => "status",
        "doctor" => "doctor",
        "explain" => "explain",
        _ => "list",
    }
}

fn valid_id(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some('a'..='z'))
        && chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '_' | '-'))
        && value.len() <= 64
}

fn builtin_ids() -> [&'static str; 2] {
    ["claude_code", "codex"]
}

#[cfg(test)]
mod execution_diagnostic_tests {
    use super::execution_failure_category;

    #[test]
    fn projects_only_fixed_execution_categories_without_source_messages() {
        for (reason, expected) in [
            ("diagnostic cancelled", "cancelled"),
            ("request deadline exceeded", "timeout"),
            ("request limit exceeded", "input_limit"),
            ("response limit exceeded", "output_limit"),
            ("stderr limit exceeded", "stderr_limit"),
            ("adapter exited unsuccessfully", "nonzero_exit"),
            ("caller signal policy refused", "signal_context"),
            ("PRIVATE_DRIVER_ERROR_CANARY", "owned_execution_failed"),
        ] {
            assert_eq!(
                execution_failure_category("execution", reason),
                Some(expected)
            );
            assert_eq!(execution_failure_category("protocol", reason), None);
        }
    }
}
