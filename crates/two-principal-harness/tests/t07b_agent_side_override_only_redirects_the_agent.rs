//! T7(b) — an agent-side `LIBRA_GOVERNOR_STATE_DIR` override only
//! redirects the agent's OWN default-mode daemon; it must never touch
//! the trusted provider's ledger (ADR-0019 §3 T7, PR-2's design doc §7,
//! PR-2b scope).
//!
//! Companion to `t07a` (the provider itself ignores the override) and
//! `t05b` (the hook never reaches an env-named fake server once a
//! pinned config exists, proven via task counts). This test proves the
//! complementary direction with a stronger assertion: an agent running
//! its own, entirely separate default-mode daemon must leave not just
//! task counts, but the real provider's entire ledger —
//! [`LedgerSnapshot`]-equal (hash, inode, trigger set and both row
//! counts) — unchanged.
//!
//! # Deviation from the design doc's exact mechanism
//!
//! The design doc's own `trusted_positive_control` note (lib.rs)
//! explains that before PR-2c, no CLI client knew the pinned trusted
//! endpoint, so both halves went through `lg-raw-client`; from PR-2c
//! onward (this branch is stacked on PR-2c), `trusted_client.rs` makes
//! EVERY CLI client — `hook user-prompt-submit`, `outcome record`,
//! including one an agent points at its own fake daemon via
//! `LIBRA_GOVERNOR_STATE_DIR` — find the pinned config and connect only
//! to the real provider (t05b proves exactly this). So this test cannot
//! use the CLI subcommands to drive the agent's own fake daemon once the
//! pinned config exists: doing so would (correctly, per t05b) reach the
//! real provider instead, which is the opposite of what this test needs
//! to set up. It drives the agent's own fake daemon with `lg-raw-client`
//! directly instead — a raw socket client that connects to whatever path
//! it is given and has no pinned-endpoint logic to begin with, exactly
//! like `x_confused_deputy.rs`'s same-UID sensitivity control and
//! PR-2a/2b's `trusted_positive_control` before PR-2c.

use std::path::Path;
use std::time::Duration;

use libra_governor_two_principal_harness::{
    assert_disposable, reset_state_dirs, run_as, ConfigSpec, DaemonProcess, LedgerSnapshot,
    Principal, TrustedConfigGuard, TrustedDaemon, LG_AGENT, PROVIDER_STATE_DIR, SAMEUID_STATE_DIR,
    TRUSTED_CONFIG_PATH,
};

const PATH_ENV: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

fn raw_client() -> &'static str {
    env!("CARGO_BIN_EXE_lg-raw-client")
}

fn send(principal: &Principal, home: &str, socket: &str, json: &str) -> serde_json::Value {
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

/// Drives the agent's own fake daemon (never the real provider) directly
/// over its raw socket: a `Preflight` followed by a `RecordOutcome` for
/// the task it just created, tagged `tag` so repeated calls across
/// passes never collide on `session_id`/`idempotency_key`.
fn seed_task_and_record_outcome(socket: &str, tag: &str) {
    let preflight = serde_json::json!({
        "protocol_version": libra_governor_protocol::PROTOCOL_VERSION,
        "request": {
            "kind": "preflight",
            "task_hint": format!("t07b {tag} preflight"),
            "cwd": SAMEUID_STATE_DIR,
            "session_id": format!("t07b-{tag}"),
        }
    })
    .to_string();
    let response = send(&LG_AGENT, "/home/lg-agent", socket, &preflight);
    let task_id = response["response"]["task_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{tag}: Preflight response had no task_id: {response}"))
        .to_string();

    let outcome = serde_json::json!({
        "protocol_version": libra_governor_protocol::PROTOCOL_VERSION,
        "request": {
            "kind": "record_outcome",
            "task_id": task_id,
            "plan_id": null,
            "source_id": format!("t07b-{tag}"),
            "idempotency_key": format!("t07b-{tag}-1"),
            "outcome": {"kind": "completed", "evidence": [format!("harness://t07b-{tag}")]},
            "signed_claim": null,
        }
    })
    .to_string();
    let response = send(&LG_AGENT, "/home/lg-agent", socket, &outcome);
    assert_eq!(
        response["response"]["state"], "recorded",
        "{tag}: RecordOutcome must reach `recorded`, not some other state: {response}"
    );
}

#[test]
#[ignore = "runs only inside harness/two-principal/run.sh's disposable container"]
fn t07b_agent_side_override_only_redirects_the_agent() {
    assert_disposable();
    reset_state_dirs();

    // --- Sensitivity control: with no pinned config anywhere in this
    // pass, a raw client against the agent's own default-mode daemon
    // really does land both halves there — today's ordinary contract,
    // proven here directly against the wire protocol rather than through
    // task counts (t05b already proves the Preflight half via counts). ---
    {
        let mut fake = DaemonProcess::start(
            &LG_AGENT,
            SAMEUID_STATE_DIR,
            "/home/lg-agent",
            Duration::from_secs(10),
        );
        let socket = format!("{SAMEUID_STATE_DIR}/daemon.sock");
        let before = LedgerSnapshot::capture(Path::new(SAMEUID_STATE_DIR));
        seed_task_and_record_outcome(&socket, "sensitivity");
        let after = LedgerSnapshot::capture(Path::new(SAMEUID_STATE_DIR));
        assert_ne!(
            before.outcome_attestation_count, after.outcome_attestation_count,
            "sensitivity control: the agent's own outcome row must land in its own ledger"
        );
        fake.kill9_and_reap();
    }

    // --- Two-principal run: trusted mode is up and its ledger is
    // captured before. The agent then runs its OWN, entirely separate
    // default-mode daemon, pinned via env to its SAME-UID mirror
    // directory, and drives it directly over its raw socket — exactly
    // the layout a compromised or misconfigured agent host might have
    // running alongside the real pinned config. Only the agent's own
    // fake daemon ever receives anything; the real provider is never
    // contacted in between. ---
    reset_state_dirs();
    let spec = ConfigSpec::default();
    let _guard = TrustedConfigGuard::install(TRUSTED_CONFIG_PATH, &spec);
    let mut daemon = TrustedDaemon::start(TRUSTED_CONFIG_PATH, &[], Duration::from_secs(10));

    let before = LedgerSnapshot::capture(Path::new(PROVIDER_STATE_DIR));

    let mut agent_fake = DaemonProcess::start(
        &LG_AGENT,
        SAMEUID_STATE_DIR,
        "/home/lg-agent",
        Duration::from_secs(10),
    );
    let agent_socket = format!("{SAMEUID_STATE_DIR}/daemon.sock");
    seed_task_and_record_outcome(&agent_socket, "two-principal-agent-side");
    agent_fake.kill9_and_reap();

    let after = LedgerSnapshot::capture(Path::new(PROVIDER_STATE_DIR));
    assert_eq!(
        before, after,
        "an agent-side env override must never touch the provider's ledger \
         (before={before:?}, after={after:?})"
    );
    assert!(
        daemon.is_alive(),
        "the trusted provider must still be alive, untouched by the agent's own daemon"
    );

    daemon.kill9_and_reap();
}
