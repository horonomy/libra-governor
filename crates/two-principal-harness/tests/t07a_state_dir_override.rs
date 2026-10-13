//! T7(a) — state-directory env overrides (ADR-0019 §3 T7, PR-2's design
//! doc §4.5/§7). Trusted mode never reads `LIBRA_GOVERNOR_STATE_DIR`,
//! `XDG_STATE_HOME` or `HOME` — every path comes from the pinned config.
//! T7(b) (agent-side override only redirects the agent) is PR-2b scope.

use std::path::Path;

use libra_governor_two_principal_harness::{
    assert_disposable, reset_state_dirs, ConfigSpec, TrustedConfigGuard, TrustedDaemon, LG_AGENT,
    PROVIDER_STATE_DIR, TRUSTED_CONFIG_PATH,
};

const AGENT_FAKE_STATE_DIR: &str = "/tmp/t07a-lg-agent-state";

#[test]
#[ignore = "runs only inside harness/two-principal/run.sh's disposable container"]
fn t07a_provider_ignores_state_dir_env() {
    assert_disposable();
    reset_state_dirs();
    let _ = std::fs::remove_dir_all(AGENT_FAKE_STATE_DIR);

    // Sensitivity control: today's default-mode daemon DOES honor
    // LIBRA_GOVERNOR_STATE_DIR, proving this is a real env knob in this
    // environment, not a no-op the container disables generally.
    {
        let mut daemon = libra_governor_two_principal_harness::DaemonProcess::start(
            &LG_AGENT,
            AGENT_FAKE_STATE_DIR,
            "/home/lg-agent",
            std::time::Duration::from_secs(10),
        );
        assert!(
            Path::new(AGENT_FAKE_STATE_DIR)
                .join("ledger.sqlite3")
                .exists(),
            "sensitivity control: default-mode daemon must create its ledger at the \
             env-overridden state dir"
        );
        daemon.kill9_and_reap();
    }
    let _ = std::fs::remove_dir_all(AGENT_FAKE_STATE_DIR);

    // Two-principal run: trusted mode, with the agent's env override
    // AND XDG_STATE_HOME AND HOME all pointed at the same fake
    // directory. None of them may move the ledger.
    reset_state_dirs();
    let spec = ConfigSpec::default();
    let _guard = TrustedConfigGuard::install(TRUSTED_CONFIG_PATH, &spec);
    let mut daemon = TrustedDaemon::start(
        TRUSTED_CONFIG_PATH,
        &[
            ("LIBRA_GOVERNOR_STATE_DIR", AGENT_FAKE_STATE_DIR),
            ("XDG_STATE_HOME", AGENT_FAKE_STATE_DIR),
            ("HOME", AGENT_FAKE_STATE_DIR),
        ],
        std::time::Duration::from_secs(10),
    );

    assert!(
        Path::new(PROVIDER_STATE_DIR)
            .join("ledger.sqlite3")
            .exists(),
        "trusted mode's ledger must still land at the pinned state_dir"
    );
    assert!(
        !Path::new(AGENT_FAKE_STATE_DIR)
            .join("ledger.sqlite3")
            .exists(),
        "trusted mode must never create a ledger at the env-named fake state dir, even with \
         LIBRA_GOVERNOR_STATE_DIR, XDG_STATE_HOME and HOME all pointed at it"
    );

    daemon.kill9_and_reap();
    let _ = std::fs::remove_dir_all(AGENT_FAKE_STATE_DIR);
}
