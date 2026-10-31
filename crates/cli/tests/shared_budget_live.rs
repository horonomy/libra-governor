//! HORO-2195 (S4): the local monthly shared budget against a REAL
//! `libra-governor daemon run` process, over its real Unix socket, with
//! real separate client processes and real kill/restart. Evidence class:
//! **TestOwnedLive** -- synthetic clients speaking the daemon protocol, not
//! a real Claude Code host (the hook runtime is a later slice). The
//! deterministic rollover/boundary/identity matrix is in
//! `crates/daemon/tests/shared_budget_core.rs`.
//!
//! Every child process is owned by a guard that kills and reaps it on drop,
//! and every wait is a bounded loop, so a panic or a hang cannot leave
//! processes behind.

use std::io::BufReader;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use libra_governor_domain::ChargeSource;
use libra_governor_ledger::LedgerStore;
use libra_governor_protocol::{
    wire, Request, RequestEnvelope, Response, ResponseEnvelope, SharedBudgetConfigDrift,
    SharedBudgetConfigState, SharedBudgetDenyReason, SharedBudgetOutcome, SharedBudgetRequest,
    SharedBudgetStatus, PROTOCOL_VERSION,
};

mod support;

const HOLD: u64 = 1_000;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_libra-governor")
}

fn config_json(cap: u64) -> String {
    format!(
        r#"{{"schema_version":1,"monthly_cap_usd_cents":{cap},"time_zone":"UTC","turn_hold_usd_cents":{HOLD}}}"#
    )
}

/// Kills and reaps every child on drop.
struct Reaper(Vec<Child>);

impl Drop for Reaper {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct Live {
    state: support::DaemonState,
    home: tempfile::TempDir,
    daemon: Option<Child>,
}

impl Live {
    /// `file` becomes `<state>/shared-budget.json` (mode 0600) before the
    /// daemon starts, or no file when `None`.
    fn new(file: Option<&str>) -> Self {
        let live = Self {
            state: support::DaemonState::new(),
            home: tempfile::tempdir().unwrap(),
            daemon: None,
        };
        if let Some(json) = file {
            live.write_config(json);
        }
        live
    }

    fn config_path(&self) -> PathBuf {
        self.state.path().join("shared-budget.json")
    }

    fn write_config(&self, json: &str) {
        let path = self.config_path();
        std::fs::write(&path, json).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    fn sock(&self) -> PathBuf {
        self.state.path().join("daemon.sock")
    }

    fn ledger_path(&self) -> PathBuf {
        self.state.path().join("ledger.sqlite3")
    }

    fn pid(&self) -> u32 {
        let record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(self.state.path().join("daemon.pid")).unwrap())
                .unwrap();
        record["pid"].as_u64().unwrap() as u32
    }

    /// Spawns `daemon run` in the foreground and waits (bounded) until it
    /// answers a protocol request.
    fn start(&mut self) {
        assert!(self.daemon.is_none(), "daemon already started");
        let child = Command::new(bin())
            .args(["daemon", "run"])
            .env("LIBRA_GOVERNOR_STATE_DIR", self.state.path())
            .env("HOME", self.home.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn daemon run");
        self.daemon = Some(child);
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if try_rpc(&self.sock(), SharedBudgetRequest::Status {}).is_some() {
                return;
            }
            let exited = self
                .daemon
                .as_mut()
                .and_then(|c| c.try_wait().ok().flatten());
            assert!(exited.is_none(), "daemon exited during startup: {exited:?}");
            assert!(Instant::now() < deadline, "daemon never answered");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// SIGKILL: no shutdown path runs.
    fn kill9(&mut self) {
        let mut child = self.daemon.take().expect("daemon running");
        child.kill().unwrap();
        child.wait().unwrap();
    }

    /// The identity-checked `daemon stop`, then wait (bounded) for exit.
    fn stop(&mut self) {
        let status = Command::new(bin())
            .args(["daemon", "stop"])
            .env("LIBRA_GOVERNOR_STATE_DIR", self.state.path())
            .env("HOME", self.home.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "daemon stop refused");
        let mut child = self.daemon.take().expect("daemon running");
        let deadline = Instant::now() + Duration::from_secs(30);
        while child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "daemon did not exit after stop");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn rpc(&self, request: SharedBudgetRequest) -> SharedBudgetOutcome {
        try_rpc(&self.sock(), request).expect("daemon answers")
    }

    fn admit(&self, session: &str) -> SharedBudgetOutcome {
        self.rpc(SharedBudgetRequest::Admit {
            session_id: session.to_string(),
            agent_id: None,
            prompt_id: None,
            provider: None,
        })
    }

    fn status(&self) -> SharedBudgetStatus {
        match self.rpc(SharedBudgetRequest::Status {}) {
            SharedBudgetOutcome::Status(s) => *s,
            other => panic!("expected Status, got {other:?}"),
        }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        if let Some(mut child) = self.daemon.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn try_rpc(socket: &Path, request: SharedBudgetRequest) -> Option<SharedBudgetOutcome> {
    let stream = UnixStream::connect(socket).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(30)))
        .ok()?;
    wire::write_message(
        &stream,
        &RequestEnvelope {
            protocol_version: PROTOCOL_VERSION,
            request: Request::SharedBudget {
                shared_budget: request,
            },
        },
    )
    .ok()?;
    let envelope: ResponseEnvelope = wire::read_message(BufReader::new(stream)).ok()?;
    match envelope.response {
        Response::SharedBudget(outcome) => Some(*outcome),
        other => panic!("expected a SharedBudget response, got {other:?}"),
    }
}

fn reservation_id(outcome: &SharedBudgetOutcome) -> libra_governor_domain::SharedPoolReservationId {
    match outcome {
        SharedBudgetOutcome::Allowed { reservation_id, .. } => *reservation_id,
        other => panic!("expected Allowed, got {other:?}"),
    }
}

fn assert_denied_cap(outcome: &SharedBudgetOutcome, remaining: i64) {
    match outcome {
        SharedBudgetOutcome::Denied {
            reason: SharedBudgetDenyReason::CapExhausted,
            remaining_usd_cents,
            ..
        } => assert_eq!(*remaining_usd_cents, Some(remaining)),
        other => panic!("expected cap_exhausted, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// Multi-process race against one real daemon.
// ---------------------------------------------------------------------

const WORKER_ENV: &str = "HORO2195_WORKER";
const RESULT_PREFIX: &str = "HORO2195_RESULT ";

/// Not a real test: the body of one admitting client process. A no-op
/// unless the parent re-executes this test binary with `HORO2195_WORKER`
/// set. spec = "<socket>|<ready file>|<gate file>|<session id>".
#[test]
fn shared_budget_admit_worker_entrypoint() {
    let Ok(spec) = std::env::var(WORKER_ENV) else {
        return;
    };
    let parts: Vec<&str> = spec.split('|').collect();
    let (socket, ready, gate, session) = (parts[0], parts[1], parts[2], parts[3]);
    std::fs::write(ready, b"").unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !Path::new(gate).exists() {
        assert!(Instant::now() < deadline, "gate never opened");
        std::thread::sleep(Duration::from_millis(1));
    }
    let outcome = try_rpc(
        Path::new(socket),
        SharedBudgetRequest::Admit {
            session_id: session.to_string(),
            agent_id: None,
            prompt_id: None,
            provider: None,
        },
    )
    .expect("daemon answered");
    println!(
        "{RESULT_PREFIX}{}",
        serde_json::to_string(&outcome).unwrap()
    );
}

#[test]
fn twelve_concurrent_client_processes_are_granted_exactly_what_the_cap_fits() {
    const CLIENTS: usize = 12;
    const CAP: u64 = 5_500; // fits exactly five 1_000-cent holds
    let mut live = Live::new(Some(&config_json(CAP)));
    live.start();

    let gate_dir = tempfile::tempdir().unwrap();
    let gate = gate_dir.path().join("go");
    let readies: Vec<PathBuf> = (0..CLIENTS)
        .map(|i| gate_dir.path().join(format!("ready-{i}")))
        .collect();
    let mut reaper = Reaper(Vec::new());
    for (i, ready) in readies.iter().enumerate() {
        let spec = format!(
            "{}|{}|{}|racer-{i}",
            live.sock().display(),
            ready.display(),
            gate.display()
        );
        reaper.0.push(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "shared_budget_admit_worker_entrypoint",
                    "--nocapture",
                ])
                .env(WORKER_ENV, spec)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("spawn client process"),
        );
    }
    let deadline = Instant::now() + Duration::from_secs(60);
    while !readies.iter().all(|p| p.exists()) {
        assert!(Instant::now() < deadline, "clients never became ready");
        std::thread::sleep(Duration::from_millis(5));
    }
    // One file creation releases every client at once.
    std::fs::write(&gate, b"").unwrap();

    let (mut granted, mut denied) = (Vec::new(), 0usize);
    while !reaper.0.is_empty() {
        // One at a time, so unconsumed children stay in the guard.
        let child = reaper.0.remove(0);
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "client failed: {output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let line = stdout
            .lines()
            .find_map(|l| l.strip_prefix(RESULT_PREFIX))
            .unwrap_or_else(|| panic!("no result line in {stdout:?}"));
        match serde_json::from_str::<SharedBudgetOutcome>(line).unwrap() {
            SharedBudgetOutcome::Allowed { reservation_id, .. } => granted.push(reservation_id),
            denial @ SharedBudgetOutcome::Denied { .. } => {
                assert_denied_cap(&denial, 500);
                denied += 1;
            }
            other => panic!("unexpected outcome {other:?}"),
        }
    }

    assert_eq!(
        granted.len(),
        5,
        "exactly floor(cap / hold) clients are granted"
    );
    assert_eq!(
        denied,
        CLIENTS - 5,
        "every other client is denied cap_exhausted"
    );
    let mut unique = granted.clone();
    unique.sort_by_key(|id| id.0);
    unique.dedup();
    assert_eq!(unique.len(), 5, "five distinct holds");

    let status = live.status();
    assert_eq!(status.reserved_usd_cents, 5 * HOLD);
    assert_eq!(status.used_usd_cents, 0);
    assert!(
        status.used_usd_cents + status.reserved_usd_cents <= CAP,
        "never over-allocated"
    );
    assert_eq!(status.remaining_usd_cents, Some(500));
    assert_eq!(status.executors.len(), 5);
    assert_eq!(
        status
            .executors
            .iter()
            .map(|e| e.reserved_usd_cents)
            .sum::<u64>(),
        status.reserved_usd_cents
    );
}

// ---------------------------------------------------------------------
// Restart and crash recovery.
// ---------------------------------------------------------------------

#[test]
fn a_restarted_daemon_keeps_settled_totals_and_active_holds() {
    let mut live = Live::new(Some(&config_json(10_000)));
    live.start();
    let h1 = reservation_id(&live.admit("s1"));
    let h2 = reservation_id(&live.admit("s2"));
    match live.rpc(SharedBudgetRequest::ChargeAgent {
        session_id: "s1".into(),
        agent_id: Some("sub-1".into()),
        amount_usd_cents: 700,
        idempotency_key: "c1".into(),
        charge_source: ChargeSource::SubagentTranscript,
        event_time: None,
    }) {
        SharedBudgetOutcome::Charged {
            already_charged: false,
            ..
        } => {}
        other => panic!("{other:?}"),
    }
    let before = live.status();
    assert_eq!(
        (before.used_usd_cents, before.reserved_usd_cents),
        (700, 2_000)
    );
    let first_pid = live.pid();

    live.stop();
    live.start();
    assert_ne!(live.pid(), first_pid, "a new daemon process is serving");

    let after = live.status();
    assert_eq!(after.used_usd_cents, 700, "settled totals survive");
    assert_eq!(after.reserved_usd_cents, 2_000, "active holds survive");
    assert_eq!(after.executors, before.executors);
    assert_eq!(after.remaining_usd_cents, before.remaining_usd_cents);
    assert_eq!(after.period, before.period);

    // The surviving hold is the lane's hold: a re-prompt renews it.
    match live.admit("s1") {
        SharedBudgetOutcome::Allowed {
            reservation_id,
            hold_reused,
            ..
        } => {
            assert!(hold_reused);
            assert_eq!(reservation_id, h1);
        }
        other => panic!("{other:?}"),
    }
    assert_ne!(h1, h2);
    // The retried charge is a replay, not new spend.
    match live.rpc(SharedBudgetRequest::ChargeAgent {
        session_id: "s1".into(),
        agent_id: Some("sub-1".into()),
        amount_usd_cents: 700,
        idempotency_key: "c1".into(),
        charge_source: ChargeSource::SubagentTranscript,
        event_time: None,
    }) {
        SharedBudgetOutcome::Charged {
            already_charged: true,
            ..
        } => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(live.status().used_usd_cents, 700);
}

#[test]
fn a_sigkilled_daemon_leaves_no_phantom_capacity_and_dead_holds_free_exactly_their_share() {
    let mut live = Live::new(Some(&config_json(3_000)));
    live.start();
    let ids: Vec<_> = ["s1", "s2", "s3"]
        .iter()
        .map(|s| reservation_id(&live.admit(s)))
        .collect();
    assert_denied_cap(&live.admit("s4"), 0);
    let dead_pid = live.pid();

    live.kill9();
    live.start();
    assert_ne!(live.pid(), dead_pid, "a new daemon is serving");

    // The crash freed nothing: every hold still counts.
    assert_denied_cap(&live.admit("s4"), 0);
    let status = live.status();
    assert_eq!(status.reserved_usd_cents, 3_000);
    assert_eq!(status.remaining_usd_cents, Some(0));
    match live.admit("s1") {
        SharedBudgetOutcome::Allowed {
            reservation_id,
            hold_reused: true,
            ..
        } => {
            assert_eq!(reservation_id, ids[0]);
        }
        other => panic!("{other:?}"),
    }

    // Reclaiming the dead holds is bounded by their own expiry: just
    // before it nothing is freed, just after it exactly their share is.
    live.kill9();
    let mut store = LedgerStore::open(live.ledger_path()).unwrap();
    let expiries: Vec<_> = ids
        .iter()
        .map(|id| {
            store
                .get_shared_reservation(*id)
                .unwrap()
                .unwrap()
                .expires_at
        })
        .collect();
    let (earliest, latest) = (
        *expiries.iter().min().unwrap(),
        *expiries.iter().max().unwrap(),
    );
    assert!(
        store
            .expire_stale_shared_pool_reservations(earliest - time::Duration::seconds(1))
            .unwrap()
            .is_empty(),
        "nothing is reclaimed before the earliest hold's own expiry"
    );
    assert_eq!(
        store
            .expire_stale_shared_pool_reservations(latest + time::Duration::seconds(1))
            .unwrap()
            .len(),
        3,
        "exactly the three dead holds are reclaimed after their expiry"
    );
    drop(store);

    live.start();
    let status = live.status();
    assert_eq!(status.reserved_usd_cents, 0, "dead holds are reclaimed");
    for s in ["n1", "n2", "n3"] {
        reservation_id(&live.admit(s));
    }
    assert_denied_cap(&live.admit("n4"), 0);
    assert_eq!(
        live.status().reserved_usd_cents,
        3_000,
        "not one cent over the cap"
    );
}

// ---------------------------------------------------------------------
// Configuration states over the wire.
// ---------------------------------------------------------------------

#[test]
fn a_real_daemon_reports_not_configured_invalid_and_drift_honestly() {
    let mut none = Live::new(None);
    none.start();
    assert_eq!(none.admit("s1"), SharedBudgetOutcome::NotConfigured);
    let status = none.status();
    assert_eq!(status.config_state, SharedBudgetConfigState::Absent);
    assert!(!status.capacity_known);
    assert_eq!(status.config_drift, SharedBudgetConfigDrift::None);
    // A file that appears after start is enforced from the next decision.
    none.write_config(&config_json(1_000));
    assert_eq!(
        none.status().config_drift,
        SharedBudgetConfigDrift::Appeared
    );
    reservation_id(&none.admit("s1"));
    assert_denied_cap(&none.admit("s2"), 0);

    let mut bad = Live::new(Some("{ not json"));
    bad.start();
    match bad.admit("s1") {
        SharedBudgetOutcome::Denied {
            reason: SharedBudgetDenyReason::ConfigInvalid,
            ..
        } => {}
        other => panic!("{other:?}"),
    }
    let status = bad.status();
    assert_eq!(status.config_state, SharedBudgetConfigState::Invalid);
    // Fixing the file takes effect without a restart.
    bad.write_config(&config_json(1_000));
    assert_eq!(bad.status().config_drift, SharedBudgetConfigDrift::Changed);
    assert_eq!(bad.status().config_state, SharedBudgetConfigState::Valid);
    assert!(bad.status().config_changed_since_start);
    bad.stop();
    bad.start();
    assert_eq!(bad.status().config_state, SharedBudgetConfigState::Valid);
    assert!(!bad.status().config_changed_since_start);
    reservation_id(&bad.admit("s1"));
}
