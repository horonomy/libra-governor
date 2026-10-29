//! HORO-2195 (S4): the local monthly shared-budget daemon core, driven
//! through the real request dispatch (`handle_connection` over a socket
//! pair) with an injected clock so rollover and timezone boundaries are
//! deterministic. No wall-clock sleeps decide any result.
//!
//! Evidence class: **TestOwnedLive** (synthetic clients, not a real Claude
//! Code host). The real-daemon, multi-process cases live in
//! `crates/cli/tests/shared_budget_live.rs`.

use std::io::BufReader;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use libra_governor_daemon::recon::ReconBudget;
use libra_governor_daemon::shared_budget::{ManualClock, SharedBudgetRuntime};
use libra_governor_daemon::shared_budget_file::SHARED_BUDGET_FILE_NAME;
use libra_governor_daemon::DaemonConfig;
use libra_governor_domain::{ChargeSource, ExecutorKind, SharedPoolReservationId};
use libra_governor_ledger::LedgerStore;
use libra_governor_protocol::{
    wire, MeteredPoolRequest, Request, RequestEnvelope, Response, ResponseEnvelope,
    SharedBudgetCheckState, SharedBudgetConfigDrift, SharedBudgetConfigState,
    SharedBudgetDenyReason, SharedBudgetOutcome, SharedBudgetRefusal, SharedBudgetRequest,
    SharedBudgetStatus, SoftLimitScope, SoftLimitWarning, PROTOCOL_VERSION,
};

fn t(s: &str) -> OffsetDateTime {
    OffsetDateTime::parse(s, &Rfc3339).unwrap()
}

fn cfg_json(cap: u64, tz: &str, hold: u64) -> String {
    format!(
        r#"{{"schema_version":1,"monthly_cap_usd_cents":{cap},"time_zone":"{tz}","turn_hold_usd_cents":{hold}}}"#
    )
}

fn cfg_json_soft(cap: u64, tz: &str, hold: u64, per_session: u64, per_agent: u64) -> String {
    format!(
        r#"{{"schema_version":1,"monthly_cap_usd_cents":{cap},"time_zone":"{tz}","turn_hold_usd_cents":{hold},
        "soft_limits":{{"per_session_usd_cents":{per_session},"per_agent_usd_cents":{per_agent}}}}}"#
    )
}

fn daemon_config(dir: &Path, shared_budget: SharedBudgetRuntime) -> DaemonConfig {
    DaemonConfig {
        socket_path: dir.join("d.sock"),
        ledger_path: dir.join("ledger.sqlite3"),
        log_path: dir.join("daemon.log"),
        recon_budget: ReconBudget::default(),
        replan_hysteresis: libra_governor_domain::ReplanHysteresisConfig::default(),
        policy: libra_governor_daemon::default_admission_policy(),
        lifetime_scope: libra_governor_domain::LifetimeScope::LifetimeBounded,
        reservation_ttl_secs: 900,
        gateway: None,
        gateway_stats: Arc::new(Default::default()),
        gateway_session_header: libra_governor_gateway::proxy::DEFAULT_SESSION_HEADER.to_string(),
        extensions: None,
        extension_runtime: std::sync::OnceLock::new(),
        outcome_authority: None,
        progressive_interval_secs: libra_governor_daemon::DEFAULT_PROGRESSIVE_INTERVAL_SECS,
        launch_gate: None,
        shared_budget,
    }
}

struct Fx {
    dir: tempfile::TempDir,
    clock: Arc<ManualClock>,
    config: DaemonConfig,
    ledger: LedgerStore,
}

impl Fx {
    /// `file` is written as `shared-budget.json` (or no file when `None`)
    /// before the runtime loads it, exactly as the daemon does at start.
    fn new(file: Option<&str>, start: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        if let Some(json) = file {
            std::fs::write(dir.path().join(SHARED_BUDGET_FILE_NAME), json).unwrap();
        }
        let clock = Arc::new(ManualClock::new(t(start)));
        let runtime = SharedBudgetRuntime::load(dir.path()).with_clock(clock.clone());
        let config = daemon_config(dir.path(), runtime);
        let ledger = LedgerStore::open(&config.ledger_path).unwrap();
        Self {
            dir,
            clock,
            config,
            ledger,
        }
    }

    fn call(&mut self, request: Request) -> Response {
        let (client, server) = UnixStream::pair().unwrap();
        wire::write_message(
            &client,
            &RequestEnvelope {
                protocol_version: PROTOCOL_VERSION,
                request,
            },
        )
        .unwrap();
        let mut current_task = None;
        libra_governor_daemon::handle_connection(
            server,
            &mut self.ledger,
            &mut current_task,
            &self.config,
        )
        .unwrap();
        let envelope: ResponseEnvelope = wire::read_message(BufReader::new(client)).unwrap();
        envelope.response
    }

    fn sb(&mut self, request: SharedBudgetRequest) -> SharedBudgetOutcome {
        match self.call(Request::SharedBudget {
            shared_budget: request,
        }) {
            Response::SharedBudget(outcome) => *outcome,
            other => panic!("expected a SharedBudget response, got {other:?}"),
        }
    }

    fn admit(
        &mut self,
        session: &str,
        agent: Option<&str>,
        prompt: Option<&str>,
    ) -> SharedBudgetOutcome {
        self.sb(SharedBudgetRequest::Admit {
            session_id: session.to_string(),
            agent_id: agent.map(String::from),
            prompt_id: prompt.map(String::from),
            provider: None,
        })
    }

    fn check(&mut self, session: &str, agent: Option<&str>) -> SharedBudgetOutcome {
        self.sb(SharedBudgetRequest::Check {
            session_id: session.to_string(),
            agent_id: agent.map(String::from),
            provider: None,
        })
    }

    fn charge(
        &mut self,
        session: &str,
        agent: Option<&str>,
        amount: u64,
        key: &str,
        source: ChargeSource,
        event_time: Option<&str>,
    ) -> SharedBudgetOutcome {
        self.sb(SharedBudgetRequest::ChargeAgent {
            session_id: session.to_string(),
            agent_id: agent.map(String::from),
            amount_usd_cents: amount,
            idempotency_key: key.to_string(),
            charge_source: source,
            event_time: event_time.map(String::from),
        })
    }

    fn status(&mut self) -> SharedBudgetStatus {
        match self.sb(SharedBudgetRequest::Status {}) {
            SharedBudgetOutcome::Status(s) => *s,
            other => panic!("expected Status, got {other:?}"),
        }
    }

    fn pools(&self) -> Vec<String> {
        self.ledger
            .shared_pool_ids_with_prefix("metered:")
            .unwrap()
            .into_iter()
            .map(|p| p.0)
            .collect()
    }

    /// Every table's rows as text, read through a separate connection.
    fn fingerprint(&self) -> String {
        let conn = rusqlite::Connection::open_with_flags(
            &self.config.ledger_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let mut out = String::new();
        let tables: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
                .unwrap();
            let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
            rows.map(Result::unwrap).collect()
        };
        for table in tables {
            let mut stmt = conn.prepare(&format!("SELECT * FROM \"{table}\"")).unwrap();
            let columns = stmt.column_count();
            let mut rows = stmt.query([]).unwrap();
            out.push_str(&format!("[{table}]\n"));
            while let Some(row) = rows.next().unwrap() {
                for i in 0..columns {
                    out.push_str(&format!("{:?}|", row.get_ref(i).unwrap()));
                }
                out.push('\n');
            }
        }
        out
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }
}

struct Grant {
    id: SharedPoolReservationId,
    pool_id: String,
    hold: u64,
    reused: bool,
    kind: ExecutorKind,
    remaining: i64,
    period_end: String,
    warnings: Vec<SoftLimitWarning>,
}

fn grant(outcome: SharedBudgetOutcome) -> Grant {
    match outcome {
        SharedBudgetOutcome::Allowed {
            reservation_id,
            pool_id,
            hold_usd_cents,
            hold_reused,
            executor_kind,
            remaining_usd_cents,
            period_end,
            warnings,
            ..
        } => Grant {
            id: reservation_id,
            pool_id,
            hold: hold_usd_cents,
            reused: hold_reused,
            kind: executor_kind,
            remaining: remaining_usd_cents,
            period_end,
            warnings,
        },
        other => panic!("expected Allowed, got {other:?}"),
    }
}

fn expect_cap_exhausted(outcome: SharedBudgetOutcome, remaining: i64) {
    match outcome {
        SharedBudgetOutcome::Denied {
            reason: SharedBudgetDenyReason::CapExhausted,
            remaining_usd_cents,
            period_end,
            ..
        } => {
            assert_eq!(remaining_usd_cents, Some(remaining));
            assert!(period_end.is_some(), "a denial names the period end");
        }
        other => panic!("expected Denied cap_exhausted, got {other:?}"),
    }
}

fn expect_denied(outcome: SharedBudgetOutcome, reason: SharedBudgetDenyReason) {
    match outcome {
        SharedBudgetOutcome::Denied { reason: got, .. } => assert_eq!(got, reason),
        other => panic!("expected Denied {reason:?}, got {other:?}"),
    }
}

fn expect_refused(outcome: SharedBudgetOutcome, reason: SharedBudgetRefusal) {
    match outcome {
        SharedBudgetOutcome::Refused { reason: got } => assert_eq!(got, reason),
        other => panic!("expected Refused {reason:?}, got {other:?}"),
    }
}

const JAN: &str = "2027-01-15T12:00:00Z";

// ---------------------------------------------------------------------
// Safe-failure table (ADR-0026)
// ---------------------------------------------------------------------

#[test]
fn no_file_means_the_budget_path_is_not_consulted_and_nothing_is_written() {
    let mut fx = Fx::new(None, JAN);
    let before = fx.fingerprint();

    assert_eq!(
        fx.admit("s1", None, None),
        SharedBudgetOutcome::NotConfigured
    );
    assert_eq!(fx.check("s1", None), SharedBudgetOutcome::NotConfigured);
    assert_eq!(
        fx.charge("s1", None, 5, "k", ChargeSource::Gateway, None),
        SharedBudgetOutcome::NotConfigured
    );
    let status = fx.status();
    assert_eq!(status.config_state, SharedBudgetConfigState::Absent);
    assert!(!status.capacity_known);
    assert_eq!(status.remaining_usd_cents, None);
    // A budget request against no file neither validated identity nor
    // touched the ledger: byte-identical rows, no pool of any kind.
    assert_eq!(
        fx.admit("", None, None),
        SharedBudgetOutcome::NotConfigured,
        "even a blank session is not consulted without a file"
    );
    assert_eq!(fx.fingerprint(), before, "ledger rows unchanged");
    assert!(fx.pools().is_empty());
    assert_eq!(
        libra_governor_daemon::shared_budget::heartbeat(
            &mut fx.ledger,
            &fx.config.shared_budget,
            "s1"
        )
        .unwrap(),
        0
    );
    assert_eq!(
        fx.fingerprint(),
        before,
        "heartbeat is a no-op without a file"
    );
}

#[test]
fn a_tool_event_with_no_file_leaves_every_shared_pool_table_untouched() {
    let mut fx = Fx::new(None, JAN);
    let before = fx.fingerprint();
    match fx.call(Request::ToolInvoked {
        agent_id: None,
        provider: None,
        session_id: "s1".into(),
        tool_name: "Bash".into(),
    }) {
        Response::Ack => {}
        other => panic!("expected Ack, got {other:?}"),
    }
    assert!(fx.pools().is_empty());
    // The tool event itself writes (call counters); the shared-pool tables
    // must be exactly as they were.
    let after = fx.fingerprint();
    let section = |s: &str, table: &str| {
        s.split('[')
            .find(|part| part.starts_with(&format!("{table}]")))
            .map(str::to_string)
    };
    for table in ["quota_pools", "shared_pool_reservations"] {
        assert_eq!(section(&before, table), section(&after, table), "{table}");
    }
}

#[test]
fn every_invalid_file_denies_config_invalid_and_creates_nothing() {
    let bad = [
        "not json".to_string(),
        r#"{"schema_version":1,"monthly_cap_usd_cents":100,"time_zone":"UTC","extra":true}"#.into(),
        r#"{"schema_version":1,"monthly_cap_usd_cents":100}"#.into(),
        r#"{"schema_version":1,"monthly_cap_usd_cents":100,"time_zone":"Not/AZone"}"#.into(),
        r#"{"schema_version":1,"monthly_cap_usd_cents":0,"time_zone":"UTC"}"#.into(),
        r#"{"schema_version":1,"monthly_cap_usd_cents":-1,"time_zone":"UTC"}"#.into(),
        cfg_json(100, "UTC", 101),
        r#"{"schema_version":1,"monthly_cap_usd_cents":100,"time_zone":"UTC","soft_limits":{"per_agent_usd_cents":0}}"#.into(),
        r#"{"schema_version":1,"monthly_cap_usd_cents":100,"time_zone":"UTC","soft_limits":{"per_session_usd_cents":-4}}"#.into(),
        r#"{"schema_version":9,"monthly_cap_usd_cents":100,"time_zone":"UTC"}"#.into(),
    ];
    for json in bad {
        let mut fx = Fx::new(Some(&json), JAN);
        match fx.admit("s1", None, None) {
            SharedBudgetOutcome::Denied {
                reason: SharedBudgetDenyReason::ConfigInvalid,
                detail,
                ..
            } => assert!(detail.is_some(), "{json}: the reason is reported"),
            other => panic!("{json}: expected config_invalid, got {other:?}"),
        }
        expect_denied(fx.check("s1", None), SharedBudgetDenyReason::ConfigInvalid);
        expect_denied(
            fx.charge("s1", None, 5, "k", ChargeSource::Gateway, None),
            SharedBudgetDenyReason::ConfigInvalid,
        );
        assert!(fx.pools().is_empty(), "{json}: nothing charged or created");
        let status = fx.status();
        assert_eq!(status.config_state, SharedBudgetConfigState::Invalid);
        assert!(status.config_invalid_reason.is_some());
        assert!(!status.capacity_known);
    }
}

#[test]
fn a_later_period_pool_is_a_clock_anomaly_that_denies_and_charges_nothing() {
    let mut fx = Fx::new(Some(&cfg_json(5_000, "UTC", 1_000)), JAN);
    // A pool two months ahead of the clock (not just next month).
    fx.ledger
        .ensure_pool(
            &libra_governor_domain::PoolId("metered:local-budget/monthly-2027-03".into()),
            &libra_governor_domain::QuotaUnit::UsdCents,
            5_000,
            t(JAN),
        )
        .unwrap();
    let before = fx.fingerprint();
    expect_denied(
        fx.admit("s1", None, None),
        SharedBudgetDenyReason::ClockAnomaly,
    );
    expect_denied(fx.check("s1", None), SharedBudgetDenyReason::ClockAnomaly);
    assert_eq!(fx.fingerprint(), before, "a denied Admit writes nothing");
    assert!(fx.status().clock_anomaly);
    // Spend that already happened is never refused for a plausible
    // timestamp: a later-period event time is booked to the current period.
    match fx.charge(
        "s1",
        None,
        5,
        "k",
        ChargeSource::Gateway,
        Some("2027-02-10T00:00:00Z"),
    ) {
        SharedBudgetOutcome::Charged {
            pool_id,
            event_time_clamped,
            ..
        } => {
            assert_eq!(pool_id, "metered:local-budget/monthly-2027-01");
            assert!(event_time_clamped);
        }
        other => panic!("{other:?}"),
    }

    // Once the clock catches up to the newest pool the anomaly is gone.
    fx.clock.set(t("2027-03-02T00:00:00Z"));
    assert!(!fx.status().clock_anomaly);
    assert_eq!(
        grant(fx.admit("s1", None, None)).pool_id,
        "metered:local-budget/monthly-2027-03"
    );
}

// ---------------------------------------------------------------------
// Admission boundary and identity
// ---------------------------------------------------------------------

#[test]
fn admit_at_exactly_the_hold_is_allowed_and_one_cent_short_is_denied() {
    // remaining == hold -> allowed.
    let mut fx = Fx::new(Some(&cfg_json(2_000, "UTC", 1_000)), JAN);
    let first = grant(fx.admit("a", None, None));
    assert_eq!((first.remaining, first.hold), (1_000, 1_000));
    let second = grant(fx.admit("b", None, None));
    assert_eq!(second.remaining, 0, "remaining was exactly the hold");
    expect_cap_exhausted(fx.admit("c", None, None), 0);

    // remaining == hold - 1 -> denied, and nothing was reserved by the denial.
    let mut fx = Fx::new(Some(&cfg_json(1_999, "UTC", 1_000)), JAN);
    assert_eq!(grant(fx.admit("a", None, None)).remaining, 999);
    expect_cap_exhausted(fx.admit("b", None, None), 999);
    let status = fx.status();
    assert_eq!(status.reserved_usd_cents, 1_000);
    assert_eq!(status.remaining_usd_cents, Some(999));
}

#[test]
fn a_denial_reports_remaining_and_period_end() {
    let mut fx = Fx::new(Some(&cfg_json(1_000, "UTC", 1_000)), JAN);
    grant(fx.admit("a", None, None));
    match fx.admit("b", None, None) {
        SharedBudgetOutcome::Denied {
            remaining_usd_cents,
            period_end,
            ..
        } => {
            assert_eq!(remaining_usd_cents, Some(0));
            assert_eq!(period_end.as_deref(), Some("2027-02-01T00:00:00Z"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn identity_refusals_charge_nothing_and_never_fabricate_a_session() {
    let mut fx = Fx::new(Some(&cfg_json(5_000, "UTC", 1_000)), JAN);
    for blank in ["", "   ", "\t\n"] {
        expect_refused(
            fx.admit(blank, None, None),
            SharedBudgetRefusal::EmptySessionId,
        );
        expect_refused(fx.check(blank, None), SharedBudgetRefusal::EmptySessionId);
    }
    expect_refused(
        fx.admit("execution-owner-v1:abc", None, None),
        SharedBudgetRefusal::ReservedSessionScope,
    );
    expect_refused(
        fx.admit("  execution-owner-v1:abc", None, None),
        SharedBudgetRefusal::ReservedSessionScope,
    );
    expect_refused(
        fx.admit("s1", Some(""), None),
        SharedBudgetRefusal::InvalidAgentId,
    );
    expect_refused(
        fx.admit("s1", Some("  "), None),
        SharedBudgetRefusal::InvalidAgentId,
    );
    assert!(fx.pools().is_empty(), "refusals create no pool");
    assert_eq!(fx.status().reserved_usd_cents, 0);
}

#[test]
fn a_session_never_seen_by_preflight_is_admitted_and_a_missing_agent_is_agent_absent() {
    let mut fx = Fx::new(Some(&cfg_json(5_000, "UTC", 1_000)), JAN);
    let main = grant(fx.admit("brand-new-session", None, None));
    assert_eq!(main.kind, ExecutorKind::AgentAbsent);
    let agent = grant(fx.admit("brand-new-session", Some("sub-1"), None));
    assert_eq!(agent.kind, ExecutorKind::Agent);
    assert_ne!(main.id, agent.id, "distinct lanes hold separately");
    let rows = fx.status().executors;
    assert_eq!(rows.len(), 2);
    assert!(rows
        .iter()
        .any(|r| r.agent_id.is_none() && r.executor_kind == ExecutorKind::AgentAbsent));
    assert!(rows
        .iter()
        .any(|r| r.agent_id.as_deref() == Some("sub-1") && r.executor_kind == ExecutorKind::Agent));
    assert!(
        rows.iter()
            .all(|r| r.executor_kind != ExecutorKind::Unattributed),
        "a missing agent is never promoted to a root/unknown bucket by Admit"
    );
}

// ---------------------------------------------------------------------
// Lane hold reuse
// ---------------------------------------------------------------------

#[test]
fn a_reprompt_on_the_same_lane_reuses_the_hold_instead_of_reserving_again() {
    let mut fx = Fx::new(Some(&cfg_json(5_000, "UTC", 1_000)), JAN);
    let first = grant(fx.admit("s1", None, Some("p1")));
    assert!(!first.reused);
    let before = fx.ledger.get_shared_reservation(first.id).unwrap().unwrap();

    fx.clock.set(t("2027-01-15T12:10:00Z"));
    // A different prompt id, as after an interrupt (Stop never fired).
    let second = grant(fx.admit("s1", None, Some("p2")));
    assert!(second.reused);
    assert_eq!(second.id, first.id, "same hold, renewed");
    assert_eq!(
        fx.status().reserved_usd_cents,
        1_000,
        "no double reservation"
    );
    let after = fx.ledger.get_shared_reservation(first.id).unwrap().unwrap();
    assert!(
        after.expires_at > before.expires_at,
        "reuse renews the expiry"
    );

    // The same prompt id replayed is also the same hold.
    let replay = grant(fx.admit("s1", None, Some("p1")));
    assert_eq!(replay.id, first.id);
    // Another lane of the same session is a separate hold.
    let other_lane = grant(fx.admit("s1", Some("agent-x"), Some("p1")));
    assert_ne!(other_lane.id, first.id);
    assert_eq!(fx.status().reserved_usd_cents, 2_000);
}

#[test]
fn a_finished_hold_is_not_reused_and_a_resent_prompt_id_is_refused() {
    let mut fx = Fx::new(Some(&cfg_json(5_000, "UTC", 1_000)), JAN);
    let first = grant(fx.admit("s1", None, Some("p1")));
    fx.ledger
        .settle_shared(first.id, Some(300), t(JAN))
        .unwrap();
    // A prompt id names one turn: re-sending p1 after its hold finished is a
    // replay, not new work (HORO-2250).
    match fx.admit("s1", None, Some("p1")) {
        SharedBudgetOutcome::Refused { reason } => {
            assert_eq!(reason, SharedBudgetRefusal::KeyConflict)
        }
        other => panic!("expected Refused KeyConflict, got {other:?}"),
    }
    assert_eq!(fx.status().reserved_usd_cents, 0, "nothing was reserved");
    // The next real prompt gets a fresh hold; the finished one is not revived.
    let next = grant(fx.admit("s1", None, Some("p2")));
    assert!(!next.reused);
    assert_ne!(next.id, first.id);
    let status = fx.status();
    assert_eq!(status.used_usd_cents, 300);
    assert_eq!(status.reserved_usd_cents, 1_000);
}

#[test]
fn an_abandoned_hold_expires_and_frees_its_capacity() {
    let mut fx = Fx::new(Some(&cfg_json(1_000, "UTC", 1_000)), JAN);
    let dead = grant(fx.admit("crashed", None, None));
    expect_cap_exhausted(fx.admit("other", None, None), 0);
    // Just before expiry the hold still counts; at expiry it is reclaimed.
    fx.clock.set(t(JAN) + time::Duration::seconds(3_599));
    expect_cap_exhausted(fx.admit("other", None, None), 0);
    fx.clock.set(t(JAN) + time::Duration::seconds(3_600));
    let fresh = grant(fx.admit("other", None, None));
    assert_ne!(fresh.id, dead.id);
    assert_eq!(
        fx.status().reserved_usd_cents,
        1_000,
        "only the live hold counts"
    );
}

// ---------------------------------------------------------------------
// Heartbeat
// ---------------------------------------------------------------------

#[test]
fn a_tool_event_renews_the_sessions_hold_and_only_that_sessions() {
    let mut fx = Fx::new(Some(&cfg_json(5_000, "UTC", 1_000)), JAN);
    let mine = grant(fx.admit("mine", None, None));
    let sub = grant(fx.admit("mine", Some("sub"), None));
    let theirs = grant(fx.admit("theirs", None, None));
    let expiry = |fx: &Fx, id| {
        fx.ledger
            .get_shared_reservation(id)
            .unwrap()
            .unwrap()
            .expires_at
    };
    let (m0, s0, o0) = (
        expiry(&fx, mine.id),
        expiry(&fx, sub.id),
        expiry(&fx, theirs.id),
    );

    fx.clock.set(t(JAN) + time::Duration::seconds(3_000));
    match fx.call(Request::ToolInvoked {
        agent_id: None,
        provider: None,
        session_id: "mine".into(),
        tool_name: "Bash".into(),
    }) {
        Response::Ack => {}
        other => panic!("a tool event must be acked, got {other:?}"),
    }
    let renewed_to = t(JAN) + time::Duration::seconds(3_000 + 3_600);
    assert_eq!(expiry(&fx, mine.id), renewed_to);
    assert_eq!(expiry(&fx, sub.id), renewed_to, "every lane of the session");
    assert_eq!(expiry(&fx, theirs.id), o0, "another session is untouched");
    assert!(renewed_to > m0 && renewed_to > s0);

    // The renewed hold survives past the original expiry.
    fx.clock.set(t(JAN) + time::Duration::seconds(3_700));
    let now = fx.clock_now();
    assert_eq!(
        fx.ledger
            .expire_stale_shared_pool_reservations(now)
            .unwrap()
            .len(),
        1
    );
}

impl Fx {
    fn clock_now(&self) -> OffsetDateTime {
        use libra_governor_daemon::shared_budget::Clock;
        self.clock.now()
    }
}

#[test]
fn a_tool_event_for_an_invalid_config_is_acked_and_touches_no_hold() {
    let mut fx = Fx::new(Some("{bad"), JAN);
    match fx.call(Request::ToolInvoked {
        agent_id: None,
        provider: None,
        session_id: "s".into(),
        tool_name: "Bash".into(),
    }) {
        Response::Ack => {}
        other => panic!("{other:?}"),
    }
    assert!(fx.pools().is_empty());
}

// ---------------------------------------------------------------------
// Rollover and timezone
// ---------------------------------------------------------------------

#[test]
fn month_rollover_creates_a_new_pool_and_late_settles_and_charges_land_in_the_old_one() {
    let mut fx = Fx::new(Some(&cfg_json(2_000, "UTC", 1_000)), "2027-01-31T23:30:00Z");
    let a = grant(fx.admit("a", None, None));
    let b = grant(fx.admit("b", None, None));
    assert_eq!(a.pool_id, "metered:local-budget/monthly-2027-01");
    expect_cap_exhausted(fx.admit("c", None, None), 0);

    // One nanosecond before the boundary is still January...
    fx.clock
        .set(t("2027-02-01T00:00:00Z") - time::Duration::nanoseconds(1));
    expect_cap_exhausted(fx.admit("c", None, None), 0);
    // ...and the boundary instant itself belongs to February.
    fx.clock.set(t("2027-02-01T00:00:00Z"));
    let c = grant(fx.admit("c", None, None));
    assert_eq!(c.pool_id, "metered:local-budget/monthly-2027-02");
    assert_eq!(
        c.remaining, 1_000,
        "February's cap is untouched by January's holds"
    );
    assert_eq!(c.period_end, "2027-03-01T00:00:00Z");
    assert_eq!(fx.pools().len(), 2);

    // Lane b's January hold is not reused for a February turn: the new turn
    // must be charged against February.
    let b2 = grant(fx.admit("b", None, None));
    assert!(!b2.reused);
    assert_eq!(b2.pool_id, "metered:local-budget/monthly-2027-02");
    assert_ne!(b2.id, b.id);

    // A settle after the boundary lands in the hold's own (January) row.
    let now = fx.clock_now();
    fx.ledger.settle_shared(a.id, Some(400), now).unwrap();
    // A charge with a January event time lands in January...
    match fx.charge(
        "a",
        Some("sub"),
        150,
        "late-1",
        ChargeSource::SubagentTranscript,
        Some("2027-01-31T23:30:00Z"),
    ) {
        SharedBudgetOutcome::Charged { pool_id, .. } => {
            assert_eq!(pool_id, "metered:local-budget/monthly-2027-01");
        }
        other => panic!("{other:?}"),
    }
    // ...one with no event time follows the session's latest hold (a: January).
    match fx.charge(
        "a",
        Some("sub"),
        50,
        "late-2",
        ChargeSource::SubagentTranscript,
        None,
    ) {
        SharedBudgetOutcome::Charged { pool_id, .. } => {
            assert_eq!(pool_id, "metered:local-budget/monthly-2027-01");
        }
        other => panic!("{other:?}"),
    }
    // A session with no hold at all falls back to the current period.
    match fx.charge("fresh", None, 25, "f1", ChargeSource::Gateway, None) {
        SharedBudgetOutcome::Charged { pool_id, .. } => {
            assert_eq!(pool_id, "metered:local-budget/monthly-2027-02");
        }
        other => panic!("{other:?}"),
    }

    let usage = |fx: &Fx, pool: &str| {
        let r = fx
            .ledger
            .shared_pool_executor_usage(&libra_governor_domain::PoolId(pool.into()))
            .unwrap();
        (r.total_used, r.total_reserved)
    };
    // January: a settled 400 + two charges (150 + 50); b's hold still active.
    assert_eq!(
        usage(&fx, "metered:local-budget/monthly-2027-01"),
        (600, 1_000)
    );
    // February: only the fresh charge is settled; c and b2 hold.
    assert_eq!(
        usage(&fx, "metered:local-budget/monthly-2027-02"),
        (25, 2_000)
    );
}

#[test]
fn the_configured_zone_not_utc_decides_where_the_month_edge_falls() {
    let edge = "2027-02-01T03:00:00Z"; // Jan 31 22:00 in New York (UTC-5)
    let mut ny = Fx::new(Some(&cfg_json(5_000, "America/New_York", 1_000)), edge);
    let g = grant(ny.admit("s", None, None));
    assert_eq!(g.pool_id, "metered:local-budget/monthly-2027-01");
    assert_eq!(
        g.period_end, "2027-02-01T05:00:00Z",
        "February starts at midnight local"
    );

    let mut utc = Fx::new(Some(&cfg_json(5_000, "UTC", 1_000)), edge);
    let g = grant(utc.admit("s", None, None));
    assert_eq!(g.pool_id, "metered:local-budget/monthly-2027-02");

    // New York's own boundary: 05:00Z is February there.
    ny.clock.set(t("2027-02-01T05:00:00Z"));
    assert_eq!(
        grant(ny.admit("s2", None, None)).pool_id,
        "metered:local-budget/monthly-2027-02"
    );
    let status = ny.status();
    assert_eq!(status.config.unwrap().time_zone, "America/New_York");
    assert_eq!(status.period.unwrap().start, "2027-02-01T05:00:00Z");
}

// ---------------------------------------------------------------------
// Soft limits are warn-only
// ---------------------------------------------------------------------

fn decisions(fx: &mut Fx) -> Vec<String> {
    // Same script for every run; only the soft limits differ.
    let mut out = Vec::new();
    for (s, a) in [
        ("s1", None),
        ("s1", Some("a")),
        ("s2", None),
        ("s3", None),
        ("s4", None),
    ] {
        out.push(match fx.admit(s, a, None) {
            SharedBudgetOutcome::Allowed {
                remaining_usd_cents,
                executor_kind,
                ..
            } => {
                format!("allowed remaining={remaining_usd_cents} kind={executor_kind:?}")
            }
            SharedBudgetOutcome::Denied {
                reason,
                remaining_usd_cents,
                ..
            } => {
                format!("denied {reason:?} remaining={remaining_usd_cents:?}")
            }
            other => panic!("{other:?}"),
        });
    }
    out
}

#[test]
fn soft_limits_warn_but_never_change_a_decision_or_mint_capacity() {
    let mut plain = Fx::new(Some(&cfg_json(3_000, "UTC", 1_000)), JAN);
    // Thresholds of 1 cent: every admission is over both soft limits.
    let mut soft = Fx::new(Some(&cfg_json_soft(3_000, "UTC", 1_000, 1, 1)), JAN);
    // Thresholds far above the hard cap must not grant a single extra hold.
    let mut huge = Fx::new(
        Some(&cfg_json_soft(3_000, "UTC", 1_000, 1_000_000, 1_000_000)),
        JAN,
    );

    let plain_decisions = decisions(&mut plain);
    assert_eq!(
        decisions(&mut soft),
        plain_decisions,
        "identical with soft limits"
    );
    assert_eq!(
        decisions(&mut huge),
        plain_decisions,
        "soft limits mint nothing"
    );
    assert_eq!(
        plain_decisions
            .iter()
            .filter(|d| d.starts_with("allowed"))
            .count(),
        3,
        "exactly cap/hold grants: {plain_decisions:?}"
    );
    let status = soft.status();
    assert_eq!(
        status.used_usd_cents + status.reserved_usd_cents,
        3_000,
        "the hard cap is reached"
    );
    assert_eq!(huge.status().reserved_usd_cents, 3_000);

    // Warnings: present only where configured and at/over the threshold.
    let mut fx = Fx::new(Some(&cfg_json_soft(9_000, "UTC", 1_000, 2_000, 1_500)), JAN);
    assert!(
        grant(fx.admit("s1", None, None)).warnings.is_empty(),
        "1000 < both thresholds"
    );
    let second = grant(fx.admit("s1", Some("a"), None));
    // s1 now holds 2000 across two lanes: per-session (>= 2000) warns; the
    // agent lane holds 1000 < 1500.
    assert_eq!(second.warnings.len(), 1);
    assert_eq!(second.warnings[0].scope, SoftLimitScope::PerSession);
    assert_eq!(second.warnings[0].used_plus_reserved_usd_cents, 2_000);
    assert_eq!(second.warnings[0].threshold_usd_cents, 2_000);
    fx.charge(
        "s1",
        Some("a"),
        500,
        "c1",
        ChargeSource::SubagentTranscript,
        None,
    );
    let third = grant(fx.admit("s1", Some("a"), None));
    assert!(third.reused);
    assert!(
        third
            .warnings
            .iter()
            .any(|w| w.scope == SoftLimitScope::PerAgent && w.agent_id.as_deref() == Some("a")),
        "agent a: 1000 held + 500 charged >= 1500"
    );
    let mut none = Fx::new(Some(&cfg_json(3_000, "UTC", 1_000)), JAN);
    assert!(
        grant(none.admit("s1", None, None)).warnings.is_empty(),
        "no soft limits configured -> no warnings"
    );
    // And the status reports the same warnings it would for an admit.
    assert!(fx.status().warnings.len() >= 2);
}

#[test]
fn per_agent_warnings_exclude_main_lanes_without_hiding_session_usage() {
    let mut fx = Fx::new(Some(&cfg_json_soft(9_000, "UTC", 1_000, 1, 1)), JAN);
    let main = grant(fx.admit("main", None, None));
    assert!(main
        .warnings
        .iter()
        .any(|w| w.scope == SoftLimitScope::PerSession));
    assert!(main
        .warnings
        .iter()
        .all(|w| w.scope != SoftLimitScope::PerAgent));
    let agent = grant(fx.admit("main", Some("worker"), None));
    assert!(agent
        .warnings
        .iter()
        .any(|w| w.scope == SoftLimitScope::PerAgent && w.agent_id.as_deref() == Some("worker")));
    let renewed = grant(fx.admit("main", None, None));
    assert!(renewed.reused);
    assert!(renewed
        .warnings
        .iter()
        .all(|w| w.scope != SoftLimitScope::PerAgent));
    let status = fx.status();
    assert_eq!(status.used_usd_cents + status.reserved_usd_cents, 2_000);
    let agent_warnings: Vec<_> = status
        .warnings
        .iter()
        .filter(|w| w.scope == SoftLimitScope::PerAgent)
        .collect();
    assert_eq!(agent_warnings.len(), 1);
    assert_eq!(agent_warnings[0].agent_id.as_deref(), Some("worker"));
    assert!(status
        .warnings
        .iter()
        .any(|w| w.scope == SoftLimitScope::PerSession && w.used_plus_reserved_usd_cents == 2_000));
}

// ---------------------------------------------------------------------
// Executor table and charges
// ---------------------------------------------------------------------

#[test]
fn the_executor_table_sums_to_the_pool_totals_including_unattributed() {
    let mut fx = Fx::new(Some(&cfg_json(50_000, "UTC", 1_000)), JAN);
    let main = grant(fx.admit("s1", None, None));
    grant(fx.admit("s1", Some("a1"), None));
    grant(fx.admit("s2", None, None));
    fx.ledger.settle_shared(main.id, Some(700), t(JAN)).unwrap();
    fx.charge(
        "s1",
        Some("a1"),
        300,
        "c-a1",
        ChargeSource::SubagentTranscript,
        None,
    );
    // No agent id on a subagent charge: explicit UNKNOWN, not the main executor.
    match fx.charge(
        "s1",
        None,
        200,
        "c-unk",
        ChargeSource::SubagentTranscript,
        None,
    ) {
        SharedBudgetOutcome::Charged { executor_kind, .. } => {
            assert_eq!(executor_kind, ExecutorKind::Unattributed);
        }
        other => panic!("{other:?}"),
    }
    // No agent id on a gateway charge: the session's main executor.
    match fx.charge("s3", None, 50, "c-gw", ChargeSource::Gateway, None) {
        SharedBudgetOutcome::Charged { executor_kind, .. } => {
            assert_eq!(executor_kind, ExecutorKind::AgentAbsent);
        }
        other => panic!("{other:?}"),
    }

    let status = fx.status();
    let used: u64 = status.executors.iter().map(|e| e.used_usd_cents).sum();
    let reserved: u64 = status.executors.iter().map(|e| e.reserved_usd_cents).sum();
    assert_eq!(used, status.used_usd_cents);
    assert_eq!(reserved, status.reserved_usd_cents);
    assert_eq!(status.used_usd_cents, 700 + 300 + 200 + 50);
    assert_eq!(status.reserved_usd_cents, 2_000, "a1 and s2 still hold");
    assert_eq!(
        status.remaining_usd_cents,
        Some(50_000 - 1_250 - 2_000),
        "cap - used - reserved"
    );
    assert!(status
        .executors
        .iter()
        .any(|e| e.executor_kind == ExecutorKind::Unattributed && e.used_usd_cents == 200));
    assert!(status
        .executors
        .iter()
        .any(|e| e.session_id == "s3" && e.executor_kind == ExecutorKind::AgentAbsent));
    assert!(status.capacity_known);
}

#[test]
fn charges_are_idempotent_validated_and_never_refused_for_capacity() {
    let mut fx = Fx::new(Some(&cfg_json(1_000, "UTC", 1_000)), JAN);
    let first = fx.charge(
        "s1",
        Some("a"),
        400,
        "k1",
        ChargeSource::SubagentTranscript,
        None,
    );
    let SharedBudgetOutcome::Charged {
        already_charged,
        remaining_usd_cents,
        reservation_id,
        ..
    } = first
    else {
        panic!("{first:?}");
    };
    assert!(!already_charged);
    assert_eq!(remaining_usd_cents, 600);
    match fx.charge(
        "s1",
        Some("a"),
        400,
        "k1",
        ChargeSource::SubagentTranscript,
        None,
    ) {
        SharedBudgetOutcome::Charged {
            already_charged,
            reservation_id: again,
            ..
        } => {
            assert!(already_charged);
            assert_eq!(again, reservation_id);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(fx.status().used_usd_cents, 400, "a replay adds nothing");
    // Same key, different amount: refused, not silently dropped.
    expect_refused(
        fx.charge(
            "s1",
            Some("a"),
            401,
            "k1",
            ChargeSource::SubagentTranscript,
            None,
        ),
        SharedBudgetRefusal::KeyConflict,
    );
    // Two sessions may reuse the same literal key.
    assert!(matches!(
        fx.charge(
            "s2",
            Some("a"),
            10,
            "k1",
            ChargeSource::SubagentTranscript,
            None
        ),
        SharedBudgetOutcome::Charged {
            already_charged: false,
            ..
        }
    ));
    expect_refused(
        fx.charge("s1", None, 0, "z", ChargeSource::Gateway, None),
        SharedBudgetRefusal::InvalidAmount,
    );
    expect_refused(
        fx.charge("s1", None, u64::MAX, "z", ChargeSource::Gateway, None),
        SharedBudgetRefusal::InvalidAmount,
    );
    expect_refused(
        fx.charge("s1", None, 1, "  ", ChargeSource::Gateway, None),
        SharedBudgetRefusal::InvalidIdempotencyKey,
    );
    for bad in ["yesterday", "1999-06-01T00:00:00Z", "9999-01-01T00:00:00Z"] {
        expect_refused(
            fx.charge("s1", None, 1, "z", ChargeSource::Gateway, Some(bad)),
            SharedBudgetRefusal::InvalidEventTime,
        );
    }
    expect_refused(
        fx.charge("", None, 1, "z", ChargeSource::Gateway, None),
        SharedBudgetRefusal::EmptySessionId,
    );

    // Spend past the cap is recorded as overrun (the money is gone), and
    // every later Admit is refused with the negative remaining.
    match fx.charge(
        "s1",
        Some("a"),
        900,
        "big",
        ChargeSource::SubagentTranscript,
        None,
    ) {
        SharedBudgetOutcome::Charged {
            overrun_usd_cents,
            remaining_usd_cents,
            capacity_known,
            ..
        } => {
            assert_eq!(overrun_usd_cents, 310);
            assert_eq!(remaining_usd_cents, -310);
            assert!(capacity_known);
        }
        other => panic!("{other:?}"),
    }
    expect_cap_exhausted(fx.admit("late", None, None), -310);
}

#[test]
fn a_hold_key_and_a_charge_key_never_collide() {
    let mut fx = Fx::new(Some(&cfg_json(5_000, "UTC", 1_000)), JAN);
    grant(fx.admit("s1", None, Some("same")));
    // A charge whose caller key equals a prompt id is a different namespace.
    assert!(matches!(
        fx.charge("s1", None, 5, "same", ChargeSource::Gateway, None),
        SharedBudgetOutcome::Charged {
            already_charged: false,
            ..
        }
    ));
}

// ---------------------------------------------------------------------
// Check
// ---------------------------------------------------------------------

#[test]
fn check_is_a_read_that_reports_remaining_and_lane_state() {
    let mut fx = Fx::new(Some(&cfg_json(2_000, "UTC", 1_000)), JAN);
    let before = fx.fingerprint();
    match fx.check("s1", None) {
        SharedBudgetOutcome::Checked {
            state,
            remaining_usd_cents,
            period_end,
        } => {
            assert_eq!(state, SharedBudgetCheckState::Available);
            assert_eq!(remaining_usd_cents, 2_000, "cap, since no pool exists yet");
            assert_eq!(period_end, "2027-02-01T00:00:00Z");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        fx.fingerprint(),
        before,
        "Check never creates a pool or writes"
    );

    grant(fx.admit("s1", None, None));
    let state = |fx: &mut Fx, s: &str, a| match fx.check(s, a) {
        SharedBudgetOutcome::Checked {
            state,
            remaining_usd_cents,
            ..
        } => (state, remaining_usd_cents),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        state(&mut fx, "s1", None),
        (SharedBudgetCheckState::Admitted, 1_000)
    );
    // A different lane has no hold; 1000 remain = one hold.
    assert_eq!(
        state(&mut fx, "s1", Some("a")),
        (SharedBudgetCheckState::Available, 1_000)
    );
    grant(fx.admit("s2", None, None));
    // Exhausted for a lane with no hold; the admitted lane stays admitted.
    assert_eq!(
        state(&mut fx, "s3", None),
        (SharedBudgetCheckState::Exhausted, 0)
    );
    assert_eq!(
        state(&mut fx, "s1", None),
        (SharedBudgetCheckState::Admitted, 0)
    );
    // Below one hold (999) is already exhausted for a new lane.
    let mut fx2 = Fx::new(Some(&cfg_json(1_999, "UTC", 1_000)), JAN);
    grant(fx2.admit("a", None, None));
    assert_eq!(
        state(&mut fx2, "b", None),
        (SharedBudgetCheckState::Exhausted, 999)
    );
}

// ---------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------

#[test]
fn status_carries_the_honest_labels_and_the_running_configuration() {
    let mut fx = Fx::new(
        Some(&cfg_json_soft(7_000, "Europe/Paris", 700, 3_000, 2_000)),
        JAN,
    );
    let s = fx.status();
    assert_eq!(s.basis, "libra-accounted-usd-list-price-equivalent");
    assert_eq!(s.guarantee, "admission-bounded-cooperative");
    assert_eq!(s.scope, "this-host-this-os-user");
    assert_eq!(s.config_state, SharedBudgetConfigState::Valid);
    assert!(s.capacity_known);
    assert!(
        !s.pool_exists,
        "the pool is created lazily by the first Admit"
    );
    assert_eq!(s.remaining_usd_cents, Some(7_000));
    let c = s.config.unwrap();
    assert_eq!(
        (
            c.monthly_cap_usd_cents,
            c.turn_hold_usd_cents,
            c.time_zone.as_str()
        ),
        (7_000, 700, "Europe/Paris")
    );
    assert_eq!(c.soft_limit_per_session_usd_cents, Some(3_000));
    assert_eq!(c.soft_limit_per_agent_usd_cents, Some(2_000));
    let p = s.period.unwrap();
    assert_eq!(p.pool_id, "metered:local-budget/monthly-2027-01");
    assert_eq!(p.start, "2026-12-31T23:00:00Z", "Paris is UTC+1 in January");
    assert!(fx.pools().is_empty(), "Status is read-only");
    grant(fx.admit("s", None, None));
    assert!(fx.status().pool_exists);
}

#[test]
fn status_reports_the_file_in_force_and_its_drift_from_the_start_config() {
    let mut fx = Fx::new(Some(&cfg_json(5_000, "UTC", 1_000)), JAN);
    let file = fx.path().join(SHARED_BUDGET_FILE_NAME);
    let s = fx.status();
    assert_eq!(s.config_drift, SharedBudgetConfigDrift::None);
    assert!(!s.config_changed_since_start);

    std::fs::write(&file, cfg_json(9_000, "UTC", 1_000)).unwrap();
    let s = fx.status();
    assert_eq!(s.config_drift, SharedBudgetConfigDrift::Changed);
    assert!(s.config_changed_since_start);
    assert_eq!(
        s.config.unwrap().monthly_cap_usd_cents,
        9_000,
        "the daemon enforces the file in force, not the one read at start"
    );
    std::fs::remove_file(&file).unwrap();
    let s = fx.status();
    assert_eq!(s.config_drift, SharedBudgetConfigDrift::Removed);
    assert_eq!(s.config_state, SharedBudgetConfigState::Absent);
    assert!(s.config_changed_since_start);

    let mut absent = Fx::new(None, JAN);
    assert_eq!(absent.status().config_drift, SharedBudgetConfigDrift::None);
    std::fs::write(
        absent.path().join(SHARED_BUDGET_FILE_NAME),
        cfg_json(1_000, "UTC", 1_000),
    )
    .unwrap();
    let s = absent.status();
    assert_eq!(s.config_drift, SharedBudgetConfigDrift::Appeared);
    assert_eq!(s.config_state, SharedBudgetConfigState::Valid);
    assert!(s.config_changed_since_start);

    // A loose mode is noted, never refused.
    use std::os::unix::fs::PermissionsExt;
    let mut loose = Fx::new(Some(&cfg_json(5_000, "UTC", 1_000)), JAN);
    std::fs::set_permissions(
        loose.path().join(SHARED_BUDGET_FILE_NAME),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let loose_rt = SharedBudgetRuntime::load(loose.path());
    assert!(loose_rt
        .loaded()
        .permissions_note()
        .unwrap()
        .contains("0600"));
    let _ = loose.status();
}

// ---------------------------------------------------------------------
// Charge: cross-period idempotency, clamping, bounds
// ---------------------------------------------------------------------

#[test]
fn a_retried_charge_after_a_month_change_is_a_replay_not_a_second_booking() {
    let mut fx = Fx::new(Some(&cfg_json(5_000, "UTC", 1_000)), "2027-01-31T23:00:00Z");
    let first = fx.charge("s1", None, 100, "retry-me", ChargeSource::Gateway, None);
    let SharedBudgetOutcome::Charged {
        pool_id: first_pool,
        reservation_id,
        ..
    } = first
    else {
        panic!("{first:?}");
    };
    assert_eq!(first_pool, "metered:local-budget/monthly-2027-01");
    // The month turns and the session is admitted again: its latest hold
    // now lives in February, which is where a fresh fallback would point.
    fx.clock.set(t("2027-02-01T00:05:00Z"));
    grant(fx.admit("s1", None, None));
    match fx.charge("s1", None, 100, "retry-me", ChargeSource::Gateway, None) {
        SharedBudgetOutcome::Charged {
            already_charged,
            pool_id,
            reservation_id: again,
            ..
        } => {
            assert!(already_charged, "the retry is a replay");
            assert_eq!(pool_id, first_pool);
            assert_eq!(again, reservation_id);
        }
        other => panic!("{other:?}"),
    }
    let used = |fx: &Fx, pool: &str| {
        fx.ledger
            .shared_pool_executor_usage(&libra_governor_domain::PoolId(pool.into()))
            .unwrap()
            .total_used
    };
    assert_eq!(used(&fx, "metered:local-budget/monthly-2027-01"), 100);
    assert_eq!(used(&fx, "metered:local-budget/monthly-2027-02"), 0);
}

#[test]
fn a_plausible_future_event_time_is_clamped_and_recorded_but_an_absurd_one_is_refused() {
    let mut fx = Fx::new(Some(&cfg_json(5_000, "UTC", 1_000)), JAN);
    match fx.charge(
        "s1",
        None,
        40,
        "k",
        ChargeSource::Gateway,
        Some("2027-02-03T00:00:00Z"),
    ) {
        SharedBudgetOutcome::Charged {
            pool_id,
            event_time_clamped,
            ..
        } => {
            assert_eq!(pool_id, "metered:local-budget/monthly-2027-01");
            assert!(event_time_clamped);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(fx.status().used_usd_cents, 40, "the spend is on the books");
    match fx.charge(
        "s1",
        None,
        1,
        "k2",
        ChargeSource::Gateway,
        Some("2027-01-02T00:00:00Z"),
    ) {
        SharedBudgetOutcome::Charged {
            event_time_clamped, ..
        } => assert!(!event_time_clamped),
        other => panic!("{other:?}"),
    }
    expect_refused(
        fx.charge(
            "s1",
            None,
            1,
            "k3",
            ChargeSource::Gateway,
            Some("9999-01-01T00:00:00Z"),
        ),
        SharedBudgetRefusal::InvalidEventTime,
    );
}

#[test]
fn one_charge_above_ten_times_the_cap_is_refused_and_pool_totals_stay_summable() {
    let mut fx = Fx::new(Some(&cfg_json(1_000, "UTC", 1_000)), JAN);
    expect_refused(
        fx.charge("s1", None, 10_001, "big", ChargeSource::Gateway, None),
        SharedBudgetRefusal::AmountTooLarge,
    );
    assert!(matches!(
        fx.charge("s1", None, 10_000, "edge", ChargeSource::Gateway, None),
        SharedBudgetOutcome::Charged { .. }
    ));
    assert_eq!(fx.status().used_usd_cents, 10_000);

    // A huge cap: the second near-maximal charge would push the pool total
    // past the range SQLite can add, so it is refused before it is written.
    let huge = 4_000_000_000_000_000_000u64;
    let mut fx = Fx::new(Some(&cfg_json(huge, "UTC", 1_000)), JAN);
    assert!(matches!(
        fx.charge("s1", None, huge, "one", ChargeSource::Gateway, None),
        SharedBudgetOutcome::Charged { .. }
    ));
    expect_refused(
        fx.charge("s1", None, huge, "two", ChargeSource::Gateway, None),
        SharedBudgetRefusal::AmountTooLarge,
    );
    // The pool still answers and a small later charge still works.
    assert!(matches!(
        fx.charge("s1", None, 5, "three", ChargeSource::Gateway, None),
        SharedBudgetOutcome::Charged { .. }
    ));
    assert!(fx.status().capacity_known);
}

#[test]
fn a_tool_event_renews_only_the_current_periods_holds() {
    let mut fx = Fx::new(Some(&cfg_json(5_000, "UTC", 1_000)), "2027-01-31T23:50:00Z");
    let old = grant(fx.admit("s", None, None));
    fx.clock.set(t("2027-02-01T00:10:00Z"));
    let new = grant(fx.admit("s", None, None));
    assert_ne!(old.id, new.id);
    let expiry = |fx: &Fx, id| {
        fx.ledger
            .get_shared_reservation(id)
            .unwrap()
            .unwrap()
            .expires_at
    };
    let (old_before, new_before) = (expiry(&fx, old.id), expiry(&fx, new.id));
    fx.clock.set(t("2027-02-01T00:30:00Z"));
    fx.call(Request::ToolInvoked {
        agent_id: None,
        provider: None,
        session_id: "s".into(),
        tool_name: "Bash".into(),
    });
    assert_eq!(
        expiry(&fx, old.id),
        old_before,
        "last month's hold is not kept alive"
    );
    assert!(
        expiry(&fx, new.id) > new_before,
        "this month's hold is renewed"
    );
}

#[test]
fn status_names_the_future_pool_behind_a_clock_anomaly() {
    let mut fx = Fx::new(Some(&cfg_json(5_000, "UTC", 1_000)), JAN);
    assert_eq!(fx.status().future_pool_detected, None);
    fx.ledger
        .ensure_pool(
            &libra_governor_domain::PoolId("metered:local-budget/monthly-2027-04".into()),
            &libra_governor_domain::QuotaUnit::UsdCents,
            5_000,
            t(JAN),
        )
        .unwrap();
    let s = fx.status();
    assert!(s.clock_anomaly);
    assert_eq!(s.future_pool_detected.as_deref(), Some("monthly-2027-04"));
}

// ---------------------------------------------------------------------
// Hot reload: the file in force is consulted on every decision (fail closed)
// ---------------------------------------------------------------------

#[test]
fn a_file_created_after_start_is_enforced_from_the_next_decision() {
    let mut fx = Fx::new(None, JAN);
    assert_eq!(
        fx.admit("a", None, None),
        SharedBudgetOutcome::NotConfigured
    );
    std::fs::write(
        fx.path().join(SHARED_BUDGET_FILE_NAME),
        cfg_json(2_000, "UTC", 1_000),
    )
    .unwrap();
    grant(fx.admit("a", None, None));
    grant(fx.admit("b", None, None));
    expect_cap_exhausted(fx.admit("c", None, None), 0);
    let s = fx.status();
    assert_eq!(s.config_state, SharedBudgetConfigState::Valid);
    assert!(s.config_changed_since_start);
}

#[test]
fn a_changed_cap_applies_next_and_a_lower_cap_never_revokes_existing_holds() {
    let mut fx = Fx::new(Some(&cfg_json(3_000, "UTC", 1_000)), JAN);
    let file = fx.path().join(SHARED_BUDGET_FILE_NAME);
    let a = grant(fx.admit("a", None, None));
    grant(fx.admit("b", None, None));
    // Same byte length as before ("3000" -> "1500"): the change is still seen.
    std::fs::write(&file, cfg_json(1_500, "UTC", 1_000)).unwrap();
    expect_cap_exhausted(fx.admit("c", None, None), -500);
    let again = grant(fx.admit("a", None, None));
    assert!(
        again.reused,
        "an existing hold is never revoked by a lower cap"
    );
    assert_eq!(again.id, a.id);
    std::fs::write(&file, cfg_json(5_000, "UTC", 1_000)).unwrap();
    assert_eq!(grant(fx.admit("c", None, None)).remaining, 2_000);
}

#[test]
fn a_file_that_becomes_invalid_denies_and_one_that_becomes_valid_recovers() {
    let mut fx = Fx::new(Some(&cfg_json(5_000, "UTC", 1_000)), JAN);
    let file = fx.path().join(SHARED_BUDGET_FILE_NAME);
    let held = grant(fx.admit("a", None, None));
    // A partially written file is invalid, never "no budget".
    std::fs::write(&file, "{\"schema_version\":1,\"monthly_ca").unwrap();
    expect_denied(
        fx.admit("b", None, None),
        SharedBudgetDenyReason::ConfigInvalid,
    );
    expect_denied(fx.check("a", None), SharedBudgetDenyReason::ConfigInvalid);
    expect_denied(
        fx.charge("a", None, 5, "k", ChargeSource::Gateway, None),
        SharedBudgetDenyReason::ConfigInvalid,
    );
    assert_eq!(fx.status().config_state, SharedBudgetConfigState::Invalid);
    std::fs::write(&file, cfg_json(5_000, "UTC", 1_000)).unwrap();
    let back = grant(fx.admit("a", None, None));
    assert!(back.reused);
    assert_eq!(back.id, held.id, "recovery keeps the existing hold");
    grant(fx.admit("b", None, None));
}

#[test]
fn a_file_deleted_after_start_means_not_consulted_and_status_says_so() {
    let mut fx = Fx::new(Some(&cfg_json(2_000, "UTC", 1_000)), JAN);
    let file = fx.path().join(SHARED_BUDGET_FILE_NAME);
    grant(fx.admit("a", None, None));
    std::fs::remove_file(&file).unwrap();
    let before = fx.fingerprint();
    assert_eq!(
        fx.admit("b", None, None),
        SharedBudgetOutcome::NotConfigured
    );
    assert_eq!(fx.check("b", None), SharedBudgetOutcome::NotConfigured);
    assert_eq!(fx.fingerprint(), before, "nothing is consulted or written");
    let s = fx.status();
    assert_eq!(s.config_state, SharedBudgetConfigState::Absent);
    assert!(s.config_changed_since_start);
    // Putting the file back re-enforces it, counting the earlier hold.
    std::fs::write(&file, cfg_json(2_000, "UTC", 1_000)).unwrap();
    grant(fx.admit("b", None, None));
    expect_cap_exhausted(fx.admit("c", None, None), 0);
}

// ---------------------------------------------------------------------
// The generic MeteredPool RPC cannot touch the local budget
// ---------------------------------------------------------------------

#[test]
fn the_generic_metered_pool_rpc_refuses_the_local_budget_owner_on_every_op() {
    use libra_governor_domain::QuotaUnit;
    use libra_governor_protocol::{MeteredPoolOutcome, MeteredRefusal};
    let mut fx = Fx::new(Some(&cfg_json(5_000, "UTC", 1_000)), JAN);
    let hold = grant(fx.admit("s1", None, None));
    let before = fx.fingerprint();

    let refused = |response: Response| match response {
        Response::MeteredPool(outcome) => match *outcome {
            MeteredPoolOutcome::Refused {
                reason: MeteredRefusal::InvalidPoolKey,
            } => {}
            other => panic!("expected InvalidPoolKey, got {other:?}"),
        },
        other => panic!("{other:?}"),
    };
    let owner = "local-budget".to_string();
    refused(fx.call(Request::MeteredPool {
        metered_pool: MeteredPoolRequest::Reserve {
            owner: owner.clone(),
            alias: "monthly-2027-01".into(),
            unit: QuotaUnit::UsdCents,
            amount: 100,
            session_id: "s1".into(),
            idempotency_key: "k".into(),
            session_cap: None,
        },
    }));
    // The right session and the real reservation id: still refused, and the
    // hold is not settled or released.
    refused(fx.call(Request::MeteredPool {
        metered_pool: MeteredPoolRequest::Settle {
            reservation_id: hold.id,
            owner: owner.clone(),
            session_id: "s1".into(),
            actual: Some(1),
        },
    }));
    refused(fx.call(Request::MeteredPool {
        metered_pool: MeteredPoolRequest::Release {
            reservation_id: hold.id,
            owner: owner.clone(),
            session_id: "s1".into(),
        },
    }));
    refused(fx.call(Request::MeteredPool {
        metered_pool: MeteredPoolRequest::Status {
            owner,
            alias: "monthly-2027-01".into(),
        },
    }));
    assert_eq!(fx.fingerprint(), before, "nothing moved");
    assert_eq!(fx.status().reserved_usd_cents, 1_000);
}

#[test]
fn protocol_version_is_bumped_past_the_shared_budget_shape_change() {
    assert_eq!(PROTOCOL_VERSION, 24);
}

#[test]
fn a_client_session_in_the_provider_lane_namespace_is_refused_with_zero_writes() {
    let mut fx = Fx::new(Some(&cfg_json(2_000, "UTC", 1_000)), JAN);
    let before = fx.fingerprint();
    expect_refused(
        fx.admit("shared-budget-provider-v1:[\"codex\",\"s\"]", None, None),
        SharedBudgetRefusal::ReservedSessionScope,
    );
    assert!(fx.pools().is_empty(), "a refusal creates no pool");
    assert_eq!(fx.fingerprint(), before, "nothing moved");
}
