//! Confused deputy — the provider must never dereference a
//! filesystem path the agent names (ADR-0019 §3 "extra" row, PR-2's
//! design doc §4.4). Two sinks, both named explicitly by the ADR:
//! `Preflight.cwd` (reconnaissance) and `Finalize.transcript_path`
//! (measured-usage reading).
//!
//! # Deviation from the design doc's exact mechanism
//!
//! The design doc's harness plan places a FIFO at `transcript_path` and
//! watches for an opener. That mechanism is not used here: `File::open`
//! on a FIFO opened for reading blocks until a writer connects, and
//! getting a writer-timing-matched counterpart into this test reliably
//! added real flakiness risk for no extra proof value. The same claim —
//! "the provider actually reads whatever `transcript_path` names, in an
//! environment where that path is reachable" versus "trusted mode never
//! does" — is provable more simply and just as concretely with a real
//! (non-blocking) file and the daemon's own log line
//! (`usage.rs::reason_label`'s `"no_transcript_path"`, logged by
//! `server::handle_finalize`): the sensitivity control's daemon measures
//! real tokens from a file placed exactly where an agent could place one
//! next to its own sandbox, and the trusted run logs `no_transcript_path`
//! for the identical request shape, proving the field was blanked before
//! `dispatch` ever saw it, not merely that a particular file access
//! failed.
//!
//! The `cwd` sink is proven directly against the real response:
//! `recon_summary.files_scanned` is `0` in trusted mode (the empty-path
//! sentinel is never a directory) and nonzero in the sensitivity control
//! that scans the agent's own real directory.

use std::path::Path;

use libra_governor_two_principal_harness::{
    assert_disposable, reset_state_dirs, run_as, trusted_positive_control, ConfigSpec,
    DaemonProcess, TrustedConfigGuard, TrustedDaemon, LG_AGENT, PROVIDER_SOCKET,
    PROVIDER_STATE_DIR, SAMEUID_STATE_DIR, TRUSTED_CONFIG_PATH,
};

const PATH_ENV: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

fn raw_client() -> &'static str {
    env!("CARGO_BIN_EXE_lg-raw-client")
}

fn send(
    principal: &libra_governor_two_principal_harness::Principal,
    home: &str,
    socket: &str,
    json: &str,
) -> serde_json::Value {
    let out = run_as(
        principal,
        raw_client(),
        &[socket, json],
        &[("PATH", PATH_ENV), ("HOME", home)],
    );
    assert!(
        out.status.success(),
        "lg-raw-client failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "lg-raw-client printed non-JSON: {e}; stdout was {:?}",
            out.stdout
        )
    })
}

fn write_transcript(path: &str, home: &str) {
    // A single valid "assistant usage" record timestamped at the current
    // instant, matching `usage.rs::TranscriptRecord`'s real shape
    // (verified against its own `#[cfg(test)]` fixtures).
    let script = format!(
        "printf '{{\"type\":\"assistant\",\"timestamp\":\"%s\",\"message\":{{\"usage\":{{\
         \"input_tokens\":123,\"cache_creation_input_tokens\":0,\
         \"cache_read_input_tokens\":0,\"output_tokens\":45}}}}}}\\n' \"$(date -u +%Y-%m-%dT%H:%M:%SZ)\" > {path}"
    );
    let out = run_as(
        &LG_AGENT,
        "sh",
        &["-c", &script],
        &[("PATH", PATH_ENV), ("HOME", home)],
    );
    assert!(
        out.status.success(),
        "failed to write the sensitivity-control transcript fixture: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn read_log_as_root(state_dir: &str) -> String {
    std::fs::read_to_string(Path::new(state_dir).join("daemon.log")).unwrap_or_default()
}

#[test]
#[ignore = "runs only inside harness/two-principal/run.sh's disposable container"]
fn x_confused_deputy_provider_never_opens_agent_named_paths() {
    assert_disposable();
    reset_state_dirs();

    // --- Sensitivity control: same-UID default-mode daemon. ---
    let agent_transcript = format!("{SAMEUID_STATE_DIR}/fake_transcript.jsonl");
    write_transcript(&agent_transcript, "/home/lg-agent");
    // A couple of real files under the agent's own cwd, so a real recon
    // scan there has something nonzero to find.
    run_as(
        &LG_AGENT,
        "sh",
        &[
            "-c",
            &format!("echo fn main() {{}} > {SAMEUID_STATE_DIR}/main.rs"),
        ],
        &[("PATH", PATH_ENV), ("HOME", "/home/lg-agent")],
    );

    let sameuid_daemon = DaemonProcess::start(
        &LG_AGENT,
        SAMEUID_STATE_DIR,
        "/home/lg-agent",
        std::time::Duration::from_secs(10),
    );
    let sameuid_socket = format!("{SAMEUID_STATE_DIR}/daemon.sock");

    let preflight = serde_json::json!({
        "protocol_version": 14,
        "request": {
            "kind": "preflight",
            "task_hint": "confused deputy sensitivity control",
            "cwd": SAMEUID_STATE_DIR,
            "session_id": "x-confused-deputy-sameuid",
        }
    })
    .to_string();
    let response = send(&LG_AGENT, "/home/lg-agent", &sameuid_socket, &preflight);
    let files_scanned = response["response"]["recon_summary"]["files_scanned"]
        .as_u64()
        .expect("recon_summary.files_scanned must be present");
    assert!(
        files_scanned > 0,
        "sensitivity control must scan a nonzero number of files in the agent's own real \
         directory, got {files_scanned}"
    );

    let finalize = serde_json::json!({
        "protocol_version": 14,
        "request": {
            "kind": "finalize",
            "session_id": "x-confused-deputy-sameuid",
            "model": null,
            "provider": null,
            "transcript_path": agent_transcript,
        }
    })
    .to_string();
    let response = send(&LG_AGENT, "/home/lg-agent", &sameuid_socket, &finalize);
    assert_eq!(
        response["response"]["state"], "finalized",
        "sensitivity control's Finalize must actually reach the transcript read, not vacuously \
         return no_active_task: {response}"
    );
    // The log's own content is deliberately never interpolated into this
    // assertion's panic message, and the message is fully static with no
    // interpolation at all (CodeQL `rust/cleartext-logging`): this is the
    // daemon's own `daemon.log`, which by this repo's own privacy
    // invariant never carries a secret, but the static check cannot know
    // that from a variable merely named `*_log` — which test failed and
    // its assertion text are sufficient to diagnose a failure here,
    // exactly as `two-principal-harness::chown`'s doc comment already
    // establishes for the same query elsewhere in this crate.
    let sameuid_log = read_log_as_root(SAMEUID_STATE_DIR);
    assert!(
        !sameuid_log.contains("host usage no_transcript_path"),
        "sensitivity control: the daemon must have actually attempted to read the real \
         transcript file rather than treating the path as absent"
    );

    drop(sameuid_daemon);

    // --- Positive control + two-principal run: trusted mode. ---
    reset_state_dirs();
    let spec = ConfigSpec::default();
    let _guard = TrustedConfigGuard::install(TRUSTED_CONFIG_PATH, &spec);
    let mut daemon =
        TrustedDaemon::start(TRUSTED_CONFIG_PATH, &[], std::time::Duration::from_secs(10));
    let _ = trusted_positive_control(&daemon, raw_client());

    let trusted_preflight = serde_json::json!({
        "protocol_version": 14,
        "request": {
            "kind": "preflight",
            "task_hint": "confused deputy trusted run",
            "cwd": PROVIDER_STATE_DIR,
            "session_id": "x-confused-deputy-trusted",
        }
    })
    .to_string();
    let response = send(
        &LG_AGENT,
        "/home/lg-agent",
        PROVIDER_SOCKET,
        &trusted_preflight,
    );
    let files_scanned = response["response"]["recon_summary"]["files_scanned"]
        .as_u64()
        .expect("recon_summary.files_scanned must be present");
    assert_eq!(
        files_scanned, 0,
        "trusted mode must never scan the provider's own state directory, even though the \
         agent asked for exactly that path: {response}"
    );

    let evil_transcript = "/tmp/x-confused-deputy-trusted.jsonl".to_string();
    write_transcript(&evil_transcript, "/home/lg-agent");

    let trusted_finalize = serde_json::json!({
        "protocol_version": 14,
        "request": {
            "kind": "finalize",
            "session_id": "x-confused-deputy-trusted",
            "model": null,
            "provider": null,
            "transcript_path": evil_transcript,
        }
    })
    .to_string();
    let response = send(
        &LG_AGENT,
        "/home/lg-agent",
        PROVIDER_SOCKET,
        &trusted_finalize,
    );
    assert_eq!(
        response["response"]["state"], "finalized",
        "trusted mode's Finalize must still reach `finalized` (not vacuously no_active_task) \
         so the usage-reading guard actually means something: {response}"
    );
    // Same discipline as the sensitivity control's `sameuid_log` above:
    // the daemon log's own content is never interpolated, and the
    // message is fully static (CodeQL `rust/cleartext-logging`).
    let trusted_log = read_log_as_root(PROVIDER_STATE_DIR);
    assert!(
        trusted_log.contains("host usage no_transcript_path"),
        "trusted mode must log that the transcript path was treated as absent, proving \
         strip_agent_named_paths blanked it before dispatch"
    );

    daemon.kill9_and_reap();
}
