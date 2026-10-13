//! `libra-governor quota-observation <register|approve|enable|disable|uninstall|doctor|inspect>`
//! (HORO-1870 PR2) — the operator-facing CLI for the
//! `libra-governor-quota-observation` crate's registration lifecycle and
//! read/probe surface. PR1 (already merged) shipped the crate's own
//! library API (`identity::{measure_identity, approve}`,
//! `registration::check_pool_uniqueness`, `wire::{BOOTSTRAP_VERSION,
//! PROTOCOL_VERSION}`, `compat::check_describe`); this module is the
//! first and only caller of that API from an operator-invoked command —
//! nothing here runs automatically on daemon startup or any other
//! trigger.
//!
//! # Three escalating tiers (AC4)
//!
//! - `doctor` — fully passive. Lists registrations from disk; no
//!   extension process is ever started.
//! - `doctor --probe` — opt-in. A `describe` bootstrap call only (no
//!   `snapshot`), for each enabled+approved registration (or a named
//!   one). Reports identity/capabilities and protocol-version
//!   compatibility; never quota availability/coverage.
//! - `inspect` — the one command that performs a live, explicitly
//!   consented `snapshot` round trip. This is the only command in the
//!   whole surface that can report quota availability/partial/stale.
//!
//! # What this module never does
//!
//! - Never prompts for, accepts, or stores a credential value for an
//!   extension (E1) — nothing in this module's argument surface even has
//!   a place to put one.
//! - Never prints a raw protocol/stderr frame; every live-call command
//!   formats only parsed, typed fields (see `render.rs`).
//! - Never invents an auth-gating step — `LiveEndpointAuthorization`
//!   stays unwired (E2).
//! - Never multiplies a shared pool: `register` runs the same
//!   `check_pool_uniqueness` gate the crate's own loader uses, against
//!   the directory's current contents plus the candidate, before writing
//!   anything.

mod render;
mod store;

use std::sync::atomic::AtomicBool;

use libra_governor_quota_observation::compat::check_describe;
use libra_governor_quota_observation::freshness;
use libra_governor_quota_observation::registration::Registration;
use libra_governor_quota_observation::runner::{self, parse_single_frame};
use libra_governor_quota_observation::wire::{
    BootstrapOperation, BootstrapRequest, BootstrapResponse, SnapshotOperation, SnapshotRequest,
    SnapshotResponse, BOOTSTRAP_VERSION, PROTOCOL_VERSION,
};

const HELP: &str = "Usage:\n  \
    libra-governor quota-observation register <registration-file>\n  \
    libra-governor quota-observation approve <registration-id>\n  \
    libra-governor quota-observation enable <registration-id>\n  \
    libra-governor quota-observation disable <registration-id>\n  \
    libra-governor quota-observation uninstall <registration-id> [--note-executable]\n  \
    libra-governor quota-observation doctor [<registration-id>] [--probe]\n  \
    libra-governor quota-observation inspect <registration-id> [--constraint-ids <id1,id2,...>]\n\n\
    doctor is fully passive (no extension process is ever started). doctor --probe performs a \
    describe-only bootstrap round trip for each enabled+approved registration (never a snapshot \
    call). inspect performs the one live, explicitly consented snapshot round trip in this \
    surface and is the only command that reports quota availability/coverage.";

fn new_request_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Entry point for `["quota-observation", ...]`. Returns the process exit
/// code; `main.rs` is responsible for calling `std::process::exit` with
/// it.
pub fn run(args: &[String]) -> i32 {
    if args.is_empty() || matches!(args[0].as_str(), "--help" | "-h" | "help") {
        println!("{HELP}");
        return 0;
    }

    let (operation, rest) = (args[0].as_str(), &args[1..]);
    match operation {
        "register" => run_register(rest),
        "approve" => run_approve(rest),
        "enable" => run_set_enabled(rest, true),
        "disable" => run_set_enabled(rest, false),
        "uninstall" => run_uninstall(rest),
        "doctor" => run_doctor(rest),
        "inspect" => run_inspect(rest),
        other => {
            eprintln!("libra-governor quota-observation: unknown operation {other:?}\n\n{HELP}");
            2
        }
    }
}

/// A single required positional argument (a registration-id or a file
/// path), with no other arguments accepted. Shared by the four
/// single-argument lifecycle commands.
fn single_arg<'a>(args: &'a [String], usage: &str) -> Result<&'a str, i32> {
    match args {
        [only] if !only.starts_with('-') => Ok(only.as_str()),
        _ => {
            eprintln!("libra-governor quota-observation: {usage}");
            Err(2)
        }
    }
}

fn run_register(args: &[String]) -> i32 {
    let path = match single_arg(args, "register <registration-file>") {
        Ok(p) => p,
        Err(code) => return code,
    };

    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(e) => {
            eprintln!("libra-governor quota-observation register: could not read {path:?}: {e}");
            return 1;
        }
    };
    let candidate: Registration = match serde_json::from_str(&raw) {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "libra-governor quota-observation register: {path:?} is not a valid \
                 registration: {e}"
            );
            return 1;
        }
    };

    match store::register(candidate) {
        Ok(written_path) => {
            println!("registered: wrote {}", written_path.display());
            0
        }
        Err(e) => {
            eprintln!("libra-governor quota-observation register: {e}");
            1
        }
    }
}

fn run_approve(args: &[String]) -> i32 {
    let id = match single_arg(args, "approve <registration-id>") {
        Ok(id) => id,
        Err(code) => return code,
    };
    match store::approve(id) {
        Ok(registration) => {
            println!(
                "approved {:?} (extension {:?} {:?})",
                registration.registration_id,
                registration.extension_id,
                registration.extension_version
            );
            0
        }
        Err(e) => {
            eprintln!("libra-governor quota-observation approve: {e}");
            1
        }
    }
}

fn run_set_enabled(args: &[String], enabled: bool) -> i32 {
    let usage = if enabled {
        "enable <registration-id>"
    } else {
        "disable <registration-id>"
    };
    let id = match single_arg(args, usage) {
        Ok(id) => id,
        Err(code) => return code,
    };
    match store::set_enabled(id, enabled) {
        Ok(registration) => {
            let word = if enabled { "enabled" } else { "disabled" };
            println!("{word} {:?}", registration.registration_id);
            if enabled && registration.approval.approved {
                println!(
                    "note: re-enabling does not require re-approval — the manifest digest is \
                     unchanged from the last approval (trust-and-lifecycle.md)."
                );
            }
            0
        }
        Err(e) => {
            eprintln!(
                "libra-governor quota-observation {}: {e}",
                if enabled { "enable" } else { "disable" }
            );
            1
        }
    }
}

fn run_uninstall(args: &[String]) -> i32 {
    let (id, note_executable) = match args {
        [id, flag] if flag == "--note-executable" && !id.starts_with('-') => (id.as_str(), true),
        [id] if !id.starts_with('-') => (id.as_str(), false),
        _ => {
            eprintln!(
                "libra-governor quota-observation: uninstall <registration-id> [--note-executable]"
            );
            return 2;
        }
    };
    match store::uninstall(id) {
        Ok((path, registration)) => {
            println!("uninstalled {:?} (removed {})", id, path.display());
            if note_executable {
                println!(
                    "note: the extension's executable was left in place at {:?} — delete it \
                     yourself if it is no longer needed.",
                    registration.transport.executable
                );
            }
            0
        }
        Err(e) => {
            eprintln!("libra-governor quota-observation uninstall: {e}");
            1
        }
    }
}

fn run_doctor(args: &[String]) -> i32 {
    let mut probe = false;
    let mut id: Option<String> = None;
    for arg in args {
        match arg.as_str() {
            "--probe" => probe = true,
            other if !other.starts_with('-') && id.is_none() => id = Some(other.to_owned()),
            other => {
                eprintln!(
                    "libra-governor quota-observation doctor: unrecognized argument {other:?}"
                );
                return 2;
            }
        }
    }

    let dir = match store::registrations_dir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("libra-governor quota-observation doctor: {e}");
            return 1;
        }
    };
    let loaded = match store::load_with_paths(&dir) {
        Ok(loaded) => loaded,
        Err(e) => {
            eprintln!("libra-governor quota-observation doctor: {e}");
            return 1;
        }
    };

    let selected: Vec<&Registration> = loaded
        .iter()
        .map(|(_, r)| r)
        .filter(|r| id.as_deref().is_none_or(|id| r.registration_id == id))
        .collect();

    if let Some(id) = &id {
        if selected.is_empty() {
            eprintln!(
                "libra-governor quota-observation doctor: no registration {id:?} found in {dir:?}"
            );
            return 1;
        }
    }

    if selected.is_empty() {
        println!(
            "no quota-observation registrations found in {}",
            dir.display()
        );
        return 0;
    }

    for registration in &selected {
        println!("{}", render::doctor_passive_line(registration));
        if probe {
            if !registration.enabled {
                println!("  skipped --probe: registration is disabled");
                continue;
            }
            if !registration.approval.approved {
                println!("  skipped --probe: registration is not approved");
                continue;
            }
            match run_describe_probe(registration) {
                Ok(summary) => println!("{summary}"),
                Err(message) => println!("  --probe failed: {message}"),
            }
        }
    }
    0
}

/// `doctor --probe`'s one live call: a `describe` bootstrap round trip
/// only. Never a `snapshot` call, never touches quota availability or
/// coverage — see this module's own docs.
fn run_describe_probe(registration: &Registration) -> Result<String, String> {
    let request = BootstrapRequest {
        bootstrap_version: BOOTSTRAP_VERSION.to_string(),
        request_id: new_request_id(),
        operation: BootstrapOperation::Describe,
        supported_versions: vec![PROTOCOL_VERSION.to_string()],
    };
    let bytes = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
    let cancel = AtomicBool::new(false);
    let frame = runner::invoke(registration, &bytes, &cancel).map_err(|e| e.to_string())?;
    let response: BootstrapResponse = parse_single_frame(&frame).map_err(|e| e.to_string())?;
    response.validate().map_err(|e| e.to_string())?;
    Ok(render::describe_probe_summary(
        &response,
        check_describe(&response),
    ))
}

fn run_inspect(args: &[String]) -> i32 {
    let mut id: Option<String> = None;
    let mut constraint_ids: Option<Vec<String>> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--constraint-ids" => {
                let value = match args.get(i + 1) {
                    Some(v) => v,
                    None => {
                        eprintln!(
                            "libra-governor quota-observation inspect: --constraint-ids requires \
                             a comma-separated value"
                        );
                        return 2;
                    }
                };
                constraint_ids = Some(value.split(',').map(|s| s.trim().to_string()).collect());
                i += 2;
            }
            other if id.is_none() && !other.starts_with('-') => {
                id = Some(other.to_owned());
                i += 1;
            }
            other => {
                eprintln!(
                    "libra-governor quota-observation inspect: unrecognized argument {other:?}"
                );
                return 2;
            }
        }
    }
    let Some(id) = id else {
        eprintln!("libra-governor quota-observation: inspect <registration-id> [--constraint-ids <id1,id2,...>]");
        return 2;
    };

    let dir = match store::registrations_dir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("libra-governor quota-observation inspect: {e}");
            return 1;
        }
    };
    let (_, registration) = match store::find(&dir, &id) {
        Ok(found) => found,
        Err(e) => {
            eprintln!("libra-governor quota-observation inspect: {e}");
            return 1;
        }
    };
    if !registration.enabled {
        eprintln!("libra-governor quota-observation inspect: registration {id:?} is disabled");
        return 1;
    }
    if !registration.approval.approved {
        eprintln!("libra-governor quota-observation inspect: registration {id:?} is not approved");
        return 1;
    }

    let requested: Vec<String> = constraint_ids
        .unwrap_or_else(|| registration.expected.iter().map(|e| e.id.clone()).collect());

    match run_snapshot(&registration, &requested) {
        Ok(summary) => {
            println!("{summary}");
            0
        }
        Err(message) => {
            eprintln!("libra-governor quota-observation inspect: {message}");
            1
        }
    }
}

/// `inspect`'s one live call: a `snapshot` round trip, the only operation
/// in this entire CLI surface that reports quota availability/partial/
/// stale. Never persists anything — display-only, this invocation only.
fn run_snapshot(registration: &Registration, requested: &[String]) -> Result<String, String> {
    let max_age_seconds =
        freshness::compute_max_age_seconds(registration, requested).map_err(|e| e.to_string())?;
    let budget_ms = registration.timeout_ms.min(10_000);
    let request = SnapshotRequest {
        protocol_version: PROTOCOL_VERSION.to_string(),
        request_id: new_request_id(),
        operation: SnapshotOperation::Snapshot,
        context_selector: registration.context.selector.clone(),
        constraint_ids: requested.to_vec(),
        max_age_seconds,
        budget_ms,
    };
    let bytes = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
    let cancel = AtomicBool::new(false);
    let frame = runner::invoke(registration, &bytes, &cancel).map_err(|e| e.to_string())?;
    // "Received" is when this CLI invocation got the frame back — there is
    // no caching layer in this one-shot process, so this is also the
    // moment freshness is evaluated against (`now`, just below).
    let received_at = time::OffsetDateTime::now_utc();
    let response: SnapshotResponse = parse_single_frame(&frame).map_err(|e| e.to_string())?;
    response.validate(requested).map_err(|e| e.to_string())?;
    freshness::verify_response_identifiers(&request, &response, registration)
        .map_err(|e| e.to_string())?;

    if let Some(error) = &response.error {
        return Ok(render::snapshot_error_summary(error));
    }
    let result = response
        .result
        .as_ref()
        .expect("validate() guarantees exactly one of result/error is present");
    let bound = freshness::apply_account_binding(result, requested, registration)
        .map_err(|e| e.to_string())?;
    let now = time::OffsetDateTime::now_utc();
    Ok(render::snapshot_result_summary(
        registration,
        &bound,
        received_at,
        now,
    ))
}
