//! T4 — DB trigger removal, the **agent half** only (ADR-0019 §3, §6
//! PR-1). The other half — root drops a trigger offline and the
//! provider refuses to start — depends on a startup check that does not
//! exist until PR-2 (`trusted-principal` mode's pinned-config startup
//! walk), so it is explicitly out of scope here. What PR-1 proves is
//! narrower and purely OS-permission-based: `lg-agent` cannot open the
//! ledger file at all, so it cannot `DROP TRIGGER` on it either.

use std::path::Path;

use libra_governor_two_principal_harness::{
    assert_disposable, positive_control, reset_state_dirs, run_as, LedgerSnapshot, LG_AGENT,
    LG_PROVIDER, PROVIDER_STATE_DIR, SAMEUID_STATE_DIR,
};

const PATH_ENV: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

fn attack_drop_trigger(state_dir: &str, agent_home: &str) -> std::process::Output {
    run_as(
        &LG_AGENT,
        "sqlite3",
        &[
            &format!("{state_dir}/ledger.sqlite3"),
            "DROP TRIGGER trg_outcome_attestations_no_delete;",
        ],
        &[("PATH", PATH_ENV), ("HOME", agent_home)],
    )
}

#[test]
#[ignore = "runs only inside harness/two-principal/run.sh's disposable container"]
fn t04_agent_cannot_open_the_ledger_to_drop_a_trigger() {
    assert_disposable();
    reset_state_dirs();

    // Sensitivity control: lg-agent attacking its OWN ledger must
    // succeed — proves `sqlite3 ... DROP TRIGGER` really removes a
    // trigger in this environment when permitted to open the file.
    let (mut sameuid_daemon, _) = positive_control(&LG_AGENT, SAMEUID_STATE_DIR, "/home/lg-agent");
    sameuid_daemon.kill9_and_reap();
    let before_sameuid = LedgerSnapshot::capture(Path::new(SAMEUID_STATE_DIR));
    assert!(
        before_sameuid
            .triggers
            .iter()
            .any(|t| t == "trg_outcome_attestations_no_delete"),
        "sensitivity fixture must start with the trigger present"
    );
    let sameuid_attack = attack_drop_trigger(SAMEUID_STATE_DIR, "/home/lg-agent");
    assert!(
        sameuid_attack.status.success(),
        "sensitivity control: lg-agent must be able to DROP TRIGGER on its own ledger; stderr={}",
        String::from_utf8_lossy(&sameuid_attack.stderr)
    );
    let after_sameuid = LedgerSnapshot::capture(Path::new(SAMEUID_STATE_DIR));
    assert!(
        !after_sameuid
            .triggers
            .iter()
            .any(|t| t == "trg_outcome_attestations_no_delete"),
        "sensitivity control: the trigger must actually be gone afterward"
    );

    // Positive control + two-principal run: lg-agent against the real,
    // provider-owned ledger.
    let (mut daemon, _task_id) = positive_control(&LG_PROVIDER, PROVIDER_STATE_DIR, "/root");
    daemon.kill9_and_reap();
    let before = LedgerSnapshot::capture(Path::new(PROVIDER_STATE_DIR));
    assert!(
        before
            .triggers
            .iter()
            .any(|t| t == "trg_outcome_attestations_no_delete"),
        "provider ledger must start with the trigger present"
    );

    let attack = attack_drop_trigger(PROVIDER_STATE_DIR, "/home/lg-agent");
    assert!(
        !attack.status.success(),
        "lg-agent must NOT be able to open (let alone DROP TRIGGER on) the provider's ledger"
    );
    let stderr = String::from_utf8_lossy(&attack.stderr).to_lowercase();
    assert!(
        stderr.contains("unable to open database file") || stderr.contains("permission denied"),
        "expected an open-denial from sqlite3, got: {stderr}"
    );

    let after = LedgerSnapshot::capture(Path::new(PROVIDER_STATE_DIR));
    assert_eq!(
        before, after,
        "ledger (including its trigger set) must be unchanged after the failed attack"
    );
}
