//! HORO-2196 (S5, also HORO-2190): shared-budget SETTLEMENT -- turn holds
//! settled at `Stop` from priced transcript usage, session binding, and the
//! subagent transcript charge -- driven through the real request dispatch
//! (`handle_connection` over a socket pair) with an injected clock.
//!
//! Evidence class: **TestOwnedLive** (synthetic clients and SYNTHETIC
//! transcripts that mirror the documented Claude Code JSONL shape; not a
//! real Claude Code host). The per-line session field name
//! (`sessionId`) and sidechain flag (`isSidechain`) are UNCONFIRMED against
//! a real host: the fixtures here prove the logic, not the host.

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
    wire, FinalizeOutcome, Request, RequestEnvelope, Response, ResponseEnvelope,
    SharedBudgetDenyReason, SharedBudgetOutcome, SharedBudgetRefusal, SharedBudgetRequest,
    SharedBudgetStatus, PROTOCOL_VERSION,
};

fn t(s: &str) -> OffsetDateTime {
    OffsetDateTime::parse(s, &Rfc3339).unwrap()
}

fn cfg_json(cap: u64, tz: &str, hold: u64) -> String {
    format!(
        r#"{{"schema_version":1,"monthly_cap_usd_cents":{cap},"time_zone":"{tz}","turn_hold_usd_cents":{hold}}}"#
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
    hold: u64,
}

fn grant(outcome: SharedBudgetOutcome) -> Grant {
    match outcome {
        SharedBudgetOutcome::Allowed {
            reservation_id,
            hold_usd_cents,
            ..
        } => Grant {
            id: reservation_id,
            hold: hold_usd_cents,
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

// ---------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------

const BASE: &str = "2027-01-15T12:00:00Z";
/// 1,000,000 input tokens of this model = 300 cents in the pinned table.
const SONNET: &str = "claude-sonnet-4-5";
const OWNER_PREFIX: &str = "metered:local-budget/";

fn at(secs: i64) -> OffsetDateTime {
    t(BASE) + time::Duration::seconds(secs)
}

fn cfg() -> String {
    cfg_json(100_000, "UTC", 1_000)
}

/// A synthetic assistant line mirroring the documented transcript shape.
/// `session` is the per-line session field (`None` omits it).
fn rec(
    secs: i64,
    id: &str,
    model: &str,
    tiers: [u64; 4],
    session: Option<&str>,
    sidechain: bool,
) -> String {
    use libra_governor_daemon::usage::priced::{
        TRANSCRIPT_SESSION_FIELD, TRANSCRIPT_SIDECHAIN_FIELD,
    };
    let session = session
        .map(|s| format!(r#""{TRANSCRIPT_SESSION_FIELD}":"{s}","#))
        .unwrap_or_default();
    let side = if sidechain {
        format!(r#""{TRANSCRIPT_SIDECHAIN_FIELD}":true,"#)
    } else {
        String::new()
    };
    format!(
        r#"{{"type":"assistant","timestamp":"{}",{session}{side}"message":{{"id":"{id}","model":"{model}","usage":{{"input_tokens":{},"cache_creation_input_tokens":{},"cache_read_input_tokens":{},"output_tokens":{}}}}}}}"#,
        at(secs).format(&Rfc3339).unwrap(),
        tiers[0],
        tiers[1],
        tiers[2],
        tiers[3],
    )
}

/// `n` million input tokens of [`SONNET`]: `300 * n` cents.
fn mi(n: u64) -> [u64; 4] {
    [n * 1_000_000, 0, 0, 0]
}

fn write_lines(path: &Path, lines: &[String]) {
    std::fs::write(path, lines.join("\n") + "\n").unwrap();
}

fn append_lines(path: &Path, lines: &[String]) {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    for l in lines {
        writeln!(f, "{l}").unwrap();
    }
}

fn s(p: &Path) -> String {
    p.to_str().unwrap().to_string()
}

fn principal() -> libra_governor_domain::PrincipalId {
    libra_governor_domain::PrincipalId(libra_governor_domain::LOCAL_BUDGET_OWNER.to_string())
}

impl Fx {
    fn finalize(&mut self, session: &str, transcript: Option<&str>) {
        match self.call(Request::Finalize {
            session_id: session.to_string(),
            model: None,
            provider: Some("claude-code".to_string()),
            transcript_path: transcript.map(String::from),
        }) {
            Response::Finalize(FinalizeOutcome::NoActiveTask) => {}
            other => panic!("expected Finalize(NoActiveTask), got {other:?}"),
        }
    }

    fn transcript_charge(
        &mut self,
        session: &str,
        agent: Option<&str>,
        path: &str,
    ) -> SharedBudgetOutcome {
        self.sb(SharedBudgetRequest::ChargeAgentTranscript {
            session_id: session.to_string(),
            agent_id: agent.map(String::from),
            agent_transcript_path: path.to_string(),
        })
    }

    /// Every main-executor hold of `session`, oldest first.
    fn lane(&self, session: &str) -> Vec<libra_governor_domain::SharedPoolReservation> {
        let mut v = self
            .ledger
            .shared_lane_holds(
                &principal(),
                session,
                None,
                ExecutorKind::AgentAbsent,
                OWNER_PREFIX,
            )
            .unwrap();
        v.sort_by_key(|h| h.created_at);
        v
    }

    fn admit_hold(&mut self, session: &str) -> Grant {
        grant(self.admit(session, None, None))
    }
}

/// `(used, reserved)` of the current period, from `Status`.
fn totals(st: &SharedBudgetStatus) -> (u64, u64) {
    (st.used_usd_cents, st.reserved_usd_cents)
}

fn transcribed(o: SharedBudgetOutcome) -> Tc {
    match o {
        SharedBudgetOutcome::TranscriptCharged {
            reservation_id,
            amount_charged_usd_cents,
            cumulative_usd_cents,
            already_charged,
            executor_kind,
            usage_known,
            unmeasured,
            unpriced,
            transcript_unverified,
            event_time_clamped,
            ..
        } => Tc {
            row: reservation_id,
            charged: amount_charged_usd_cents,
            cumulative: cumulative_usd_cents,
            already: already_charged,
            kind: executor_kind,
            usage_known,
            unmeasured,
            unpriced,
            unverified: transcript_unverified,
            clamped: event_time_clamped,
        },
        other => panic!("expected TranscriptCharged, got {other:?}"),
    }
}

struct Tc {
    row: Option<SharedPoolReservationId>,
    charged: u64,
    cumulative: u64,
    already: bool,
    kind: ExecutorKind,
    usage_known: bool,
    unmeasured: bool,
    unpriced: bool,
    unverified: bool,
    clamped: bool,
}

// ---------------------------------------------------------------------
// Settle at Stop: measured, once, never twice
// ---------------------------------------------------------------------

#[test]
fn a_stop_settles_the_turn_hold_at_the_priced_usd_and_a_retry_does_not_settle_again() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let hold = fx.admit_hold("s1");
    assert_eq!(hold.hold, 1_000);
    assert_eq!(totals(&fx.status()), (0, 1_000));

    let tx = fx.path().join("s1.jsonl");
    write_lines(
        &tx,
        &[
            rec(10, "m1", SONNET, mi(1), Some("s1"), false),
            // 100k output tokens at $15/Mtok = $1.50 = 150 cents.
            rec(20, "m2", SONNET, [0, 0, 0, 100_000], Some("s1"), false),
        ],
    );
    fx.clock.set(at(60));
    fx.finalize("s1", Some(&s(&tx)));

    let st = fx.status();
    assert_eq!(
        totals(&st),
        (450, 0),
        "300 + 150 cents; the hold is released"
    );
    assert_eq!(st.remaining_usd_cents, Some(100_000 - 450));
    let row = st
        .executors
        .iter()
        .find(|e| e.session_id == "s1")
        .expect("the lane row");
    assert_eq!(
        (
            row.executor_kind,
            row.agent_id.as_deref(),
            row.used_usd_cents
        ),
        (ExecutorKind::AgentAbsent, None, 450)
    );
    let lane = fx.lane("s1");
    assert_eq!(lane.len(), 1);
    assert_eq!(lane[0].id, hold.id);
    assert_eq!(lane[0].settled_amount, Some(450));
    assert_eq!(lane[0].usage_known, Some(true));
    assert_eq!(lane[0].charge_source, Some(ChargeSource::TurnHold));
    assert_eq!(lane[0].settled_at, Some(at(60)));

    // The same Stop delivered again: no hold to settle, nothing changes.
    let before = fx.fingerprint();
    fx.finalize("s1", Some(&s(&tx)));
    fx.finalize("s1", Some(&s(&tx)));
    assert_eq!(
        fx.fingerprint(),
        before,
        "a retried Finalize writes nothing"
    );
    assert_eq!(totals(&fx.status()), (450, 0));
}

#[test]
fn two_consecutive_stops_never_count_a_record_twice() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let tx = fx.path().join("s1.jsonl");
    fx.admit_hold("s1");
    write_lines(&tx, &[rec(10, "m1", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(60));
    fx.finalize("s1", Some(&s(&tx)));
    assert_eq!(totals(&fx.status()), (300, 0));

    // Turn 2 appends to the SAME file; turn 1's record is still in it.
    fx.clock.set(at(100));
    fx.admit_hold("s1");
    append_lines(&tx, &[rec(110, "m2", SONNET, mi(2), Some("s1"), false)]);
    fx.clock.set(at(150));
    fx.finalize("s1", Some(&s(&tx)));
    assert_eq!(
        totals(&fx.status()),
        (900, 0),
        "300 for turn 1 plus 600 for turn 2, not 300 + 900"
    );
}

#[test]
fn a_record_stamped_exactly_at_the_settle_instant_is_counted_by_one_window_only() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let tx = fx.path().join("s1.jsonl");
    fx.admit_hold("s1");
    write_lines(
        &tx,
        &[
            rec(10, "m1", SONNET, mi(1), Some("s1"), false),
            rec(60, "edge", SONNET, mi(1), Some("s1"), false),
        ],
    );
    fx.clock.set(at(60));
    fx.finalize("s1", Some(&s(&tx)));
    assert_eq!(totals(&fx.status()), (600, 0));

    fx.clock.set(at(70));
    fx.admit_hold("s1");
    fx.clock.set(at(80));
    fx.finalize("s1", Some(&s(&tx)));
    // Nothing new in the file: the window is empty (strictly after 60), so
    // it settles at the hold amount, not 600 again.
    assert_eq!(totals(&fx.status()), (600 + 1_000, 0));
    let lane = fx.lane("s1");
    assert_eq!(lane[1].usage_known, Some(false));
}

#[test]
fn an_actual_above_the_hold_is_recorded_unclamped_and_the_next_admit_is_refused() {
    let mut fx = Fx::new(Some(&cfg_json(400, "UTC", 300)), BASE);
    fx.admit_hold("s1");
    let tx = fx.path().join("s1.jsonl");
    // 2.5M input tokens = 750 cents against a 300-cent hold and 400 cap.
    write_lines(
        &tx,
        &[rec(
            10,
            "m1",
            SONNET,
            [2_500_000, 0, 0, 0],
            Some("s1"),
            false,
        )],
    );
    fx.clock.set(at(60));
    fx.finalize("s1", Some(&s(&tx)));

    let st = fx.status();
    assert_eq!(totals(&st), (750, 0), "never clamped down to the hold");
    assert_eq!(st.remaining_usd_cents, Some(400 - 750));
    assert_eq!(fx.lane("s1")[0].settled_amount, Some(750));
    expect_cap_exhausted(fx.admit("s2", None, None), -350);
    expect_cap_exhausted(fx.admit("s1", None, None), -350);
}

// ---------------------------------------------------------------------
// Session binding (HORO-2190)
// ---------------------------------------------------------------------

#[test]
fn another_sessions_transcript_charges_nothing_and_is_counted_then_its_own_stop_pays() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    fx.admit_hold("sB");
    fx.admit_hold("sA");
    let tx = fx.path().join("a.jsonl");
    write_lines(&tx, &[rec(10, "m1", SONNET, mi(1), Some("sA"), false)]);
    fx.clock.set(at(60));

    // Session B's hook was handed session A's transcript.
    fx.finalize("sB", Some(&s(&tx)));
    let st = fx.status();
    assert_eq!(
        totals(&st),
        (0, 2_000),
        "B charged nothing; both holds stand"
    );
    assert_eq!(st.transcript_binding_mismatches, 1);
    assert_eq!(
        fx.lane("sB")[0].state,
        libra_governor_domain::ReservationState::Active,
        "B's hold is left for its TTL"
    );

    // Spend is counted by its true session's own Stop.
    fx.finalize("sA", Some(&s(&tx)));
    assert_eq!(totals(&fx.status()), (300, 1_000));
    assert_eq!(fx.status().transcript_binding_mismatches, 1);
}

#[test]
fn a_transcript_whose_session_cannot_be_verified_settles_the_hold_unmeasured() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    fx.admit_hold("s1");
    let tx = fx.path().join("s1.jsonl");
    // No per-line session field at all.
    write_lines(&tx, &[rec(10, "m1", SONNET, mi(1), None, false)]);
    fx.clock.set(at(60));
    fx.finalize("s1", Some(&s(&tx)));

    let st = fx.status();
    assert_eq!(
        totals(&st),
        (1_000, 0),
        "the hold amount, never zero and never a guess"
    );
    assert_eq!(st.transcript_unverified, 1);
    assert_eq!(st.unmeasured_settlements, 1);
    let lane = fx.lane("s1");
    assert_eq!(lane[0].settled_amount, Some(1_000));
    assert_eq!(lane[0].usage_known, Some(false));
}

#[test]
fn a_window_mixing_equal_and_foreign_sessions_is_refused_whole() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    fx.admit_hold("s1");
    let tx = fx.path().join("s1.jsonl");
    write_lines(
        &tx,
        &[
            rec(10, "m1", SONNET, mi(1), Some("s1"), false),
            rec(20, "m2", SONNET, mi(1), Some("someone-else"), false),
        ],
    );
    fx.clock.set(at(60));
    fx.finalize("s1", Some(&s(&tx)));
    let st = fx.status();
    assert_eq!(totals(&st), (0, 1_000), "never charged partially");
    assert_eq!(st.transcript_binding_mismatches, 1);
}

// ---------------------------------------------------------------------
// Unpriced and unmeasurable windows settle conservatively
// ---------------------------------------------------------------------

#[test]
fn an_unpriced_model_settles_the_hold_unmeasured_and_is_named_in_status() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    fx.admit_hold("s1");
    let tx = fx.path().join("s1.jsonl");
    write_lines(
        &tx,
        &[
            rec(10, "m1", SONNET, mi(1), Some("s1"), false),
            rec(20, "m2", "mystery-model-9", mi(1), Some("s1"), false),
        ],
    );
    fx.clock.set(at(60));
    fx.finalize("s1", Some(&s(&tx)));
    let st = fx.status();
    assert_eq!(totals(&st), (1_000, 0));
    assert_eq!(st.unpriced_models, vec!["mystery-model-9".to_string()]);
    assert_eq!(fx.lane("s1")[0].usage_known, Some(false));
}

#[test]
fn pricing_overrides_in_the_file_price_a_model_the_pinned_table_lacks() {
    let json = r#"{"schema_version":1,"monthly_cap_usd_cents":100000,"time_zone":"UTC",
        "turn_hold_usd_cents":1000,
        "pricing_overrides":{"mystery-model-9":{"input_usd_per_mtok":2.0,
        "cache_creation_input_usd_per_mtok":2.0,"cache_read_input_usd_per_mtok":2.0,
        "output_usd_per_mtok":2.0}}}"#;
    let mut fx = Fx::new(Some(json), BASE);
    fx.admit_hold("s1");
    let tx = fx.path().join("s1.jsonl");
    write_lines(
        &tx,
        &[rec(10, "m1", "mystery-model-9", mi(1), Some("s1"), false)],
    );
    fx.clock.set(at(60));
    fx.finalize("s1", Some(&s(&tx)));
    let st = fx.status();
    assert_eq!(totals(&st), (200, 0), "1M tokens at the overridden $2/Mtok");
    assert!(st.unpriced_models.is_empty());
    assert_eq!(st.config.unwrap().pricing_override_count, 1);
}

#[test]
fn a_missing_transcript_and_no_transcript_path_settle_the_hold_unmeasured() {
    for path in [Some("/nonexistent/dir/t.jsonl"), None] {
        let mut fx = Fx::new(Some(&cfg()), BASE);
        fx.admit_hold("s1");
        fx.clock.set(at(60));
        fx.finalize("s1", path);
        let st = fx.status();
        assert_eq!(totals(&st), (1_000, 0), "{path:?}");
        assert_eq!(st.unmeasured_settlements, 1);
        assert_eq!(fx.lane("s1")[0].usage_known, Some(false));
    }
}

#[test]
fn a_truncated_or_rotated_transcript_is_unmeasured_not_recounted() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let tx = fx.path().join("s1.jsonl");
    fx.admit_hold("s1");
    write_lines(
        &tx,
        &[
            rec(10, "m1", SONNET, mi(1), Some("s1"), false),
            rec(20, "m2", SONNET, mi(1), Some("s1"), false),
        ],
    );
    fx.clock.set(at(60));
    fx.finalize("s1", Some(&s(&tx)));
    assert_eq!(totals(&fx.status()), (600, 0));

    // Truncated: the file is now SHORTER than what was already settled.
    fx.clock.set(at(100));
    fx.admit_hold("s1");
    write_lines(&tx, &[rec(10, "m1", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(110));
    fx.finalize("s1", Some(&s(&tx)));
    assert_eq!(
        totals(&fx.status()),
        (600 + 1_000, 0),
        "hold, not a recount"
    );
    assert_eq!(fx.lane("s1")[1].usage_known, Some(false));

    // Rotated: a brand-new file holding only older history.
    fx.clock.set(at(200));
    fx.admit_hold("s1");
    let rotated = fx.path().join("s1.rotated.jsonl");
    write_lines(
        &rotated,
        &[rec(15, "old", SONNET, mi(9), Some("s1"), false)],
    );
    fx.clock.set(at(210));
    fx.finalize("s1", Some(&s(&rotated)));
    assert_eq!(totals(&fx.status()), (600 + 1_000 + 1_000, 0));
    assert_eq!(fx.lane("s1")[2].usage_known, Some(false));
}

#[test]
fn a_torn_final_line_costs_only_that_line() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    fx.admit_hold("s1");
    let tx = fx.path().join("s1.jsonl");
    write_lines(&tx, &[rec(10, "m1", SONNET, mi(1), Some("s1"), false)]);
    let torn = rec(20, "m2", SONNET, mi(1), Some("s1"), false);
    std::fs::OpenOptions::new()
        .append(true)
        .open(&tx)
        .map(|mut f| std::io::Write::write_all(&mut f, &torn.as_bytes()[..40]))
        .unwrap()
        .unwrap();
    fx.clock.set(at(60));
    fx.finalize("s1", Some(&s(&tx)));
    assert_eq!(totals(&fx.status()), (300, 0));
}

// ---------------------------------------------------------------------
// Crashed turn, expiry, rollover
// ---------------------------------------------------------------------

#[test]
fn a_crashed_turn_is_measured_by_the_next_stop_and_its_hold_is_reclaimed_by_ttl() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let tx = fx.path().join("s1.jsonl");
    let h1 = fx.admit_hold("s1");
    // Turn 1 spends 300 cents and then crashes: no Stop ever fires.
    write_lines(&tx, &[rec(10, "m1", SONNET, mi(1), Some("s1"), false)]);

    // Two hours later (the TTL is one hour) the user prompts again.
    fx.clock.set(at(7_200));
    let h2 = fx.admit_hold("s1");
    assert_ne!(h1.id, h2.id);
    assert_eq!(
        fx.lane("s1")[0].state,
        libra_governor_domain::ReservationState::Expired,
        "the abandoned hold was reclaimed by the existing sweep"
    );
    assert_eq!(
        totals(&fx.status()),
        (0, 1_000),
        "only the live hold reserves"
    );

    append_lines(&tx, &[rec(7_210, "m2", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(7_300));
    fx.finalize("s1", Some(&s(&tx)));
    assert_eq!(
        totals(&fx.status()),
        (600, 0),
        "the next Stop also measures the crashed turn's 300 cents"
    );
    let lane = fx.lane("s1");
    assert_eq!(
        lane[0].settled_amount, None,
        "the abandoned hold stays unspent"
    );
    assert_eq!(lane[1].settled_amount, Some(600));
}

#[test]
fn a_late_stop_after_the_hold_expired_is_still_recorded() {
    for swept in [false, true] {
        let mut fx = Fx::new(Some(&cfg()), BASE);
        let tx = fx.path().join("s1.jsonl");
        fx.admit_hold("s1");
        write_lines(&tx, &[rec(10, "m1", SONNET, mi(1), Some("s1"), false)]);
        fx.clock.set(at(7_200));
        if swept {
            // Another session's Admit runs the expiry sweep.
            fx.admit_hold("s2");
            assert_eq!(
                fx.lane("s1")[0].state,
                libra_governor_domain::ReservationState::Expired
            );
        }
        fx.finalize("s1", Some(&s(&tx)));
        let lane = fx.lane("s1");
        assert_eq!(lane[0].settled_amount, Some(300), "swept={swept}");
        assert_eq!(lane[0].usage_known, Some(true));
    }
}

#[test]
fn a_stop_after_rollover_settles_into_the_period_the_hold_was_admitted_in() {
    let mut fx = Fx::new(Some(&cfg()), "2027-01-31T23:00:00Z");
    fx.admit_hold("s1");
    let tx = fx.path().join("s1.jsonl");
    let rec_at = |ts: &str| {
        rec(0, "m1", SONNET, mi(1), Some("s1"), false).replace(&at(0).format(&Rfc3339).unwrap(), ts)
    };
    write_lines(&tx, &[rec_at("2027-02-01T00:10:00Z")]);
    fx.clock.set(t("2027-02-01T00:30:00Z"));
    fx.finalize("s1", Some(&s(&tx)));

    let jan = fx
        .ledger
        .shared_pool_executor_usage(&libra_governor_domain::PoolId(
            "metered:local-budget/monthly-2027-01".into(),
        ))
        .unwrap();
    assert_eq!(
        (jan.total_used, jan.total_reserved),
        (300, 0),
        "booked to January"
    );
    assert_eq!(totals(&fx.status()), (0, 0), "February is untouched");
}

// ---------------------------------------------------------------------
// Configuration states at Stop
// ---------------------------------------------------------------------

#[test]
fn with_no_file_a_stop_touches_nothing_and_logs_nothing_about_the_budget() {
    let mut fx = Fx::new(None, BASE);
    let tx = fx.path().join("s1.jsonl");
    write_lines(&tx, &[rec(10, "m1", SONNET, mi(1), Some("s1"), false)]);
    let before = fx.fingerprint();
    fx.finalize("s1", Some(&s(&tx)));
    assert_eq!(fx.fingerprint(), before, "ledger rows byte-identical");
    assert!(fx.pools().is_empty());
    let log = std::fs::read_to_string(&fx.config.log_path).unwrap_or_default();
    assert!(!log.contains("shared budget"), "{log}");
}

#[test]
fn an_invalid_file_at_stop_settles_the_existing_hold_at_its_amount() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    fx.admit_hold("s1");
    let tx = fx.path().join("s1.jsonl");
    write_lines(&tx, &[rec(10, "m1", SONNET, mi(1), Some("s1"), false)]);
    std::fs::write(fx.path().join(SHARED_BUDGET_FILE_NAME), "{ half written").unwrap();
    fx.clock.set(at(60));
    fx.finalize("s1", Some(&s(&tx)));
    // The hold's own pool takes the hold amount; overrides from a file that
    // cannot be trusted are not used to price anything.
    let lane = fx.lane("s1");
    assert_eq!(lane[0].settled_amount, Some(1_000));
    assert_eq!(lane[0].usage_known, Some(false));

    // No hold and an invalid file: nothing is written.
    let before = fx.fingerprint();
    fx.finalize("s9", Some(&s(&tx)));
    assert_eq!(fx.fingerprint(), before);
}

// ---------------------------------------------------------------------
// Subagent transcript charge
// ---------------------------------------------------------------------

#[test]
fn a_subagent_transcript_is_charged_by_delta_and_never_twice() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let sub = fx.path().join("agent-x.jsonl");
    write_lines(&sub, &[rec(10, "a1", SONNET, mi(2), Some("s1"), true)]);

    let first = transcribed(fx.transcript_charge("s1", Some("x"), &s(&sub)));
    assert_eq!((first.charged, first.cumulative), (600, 600));
    assert_eq!(first.kind, ExecutorKind::Agent);
    assert!(first.usage_known && !first.unmeasured && !first.already && !first.clamped);
    assert!(first.row.is_some());
    assert_eq!(totals(&fx.status()), (600, 0));

    // A repeated SubagentStop: nothing is written.
    let before = fx.fingerprint();
    let again = transcribed(fx.transcript_charge("s1", Some("x"), &s(&sub)));
    assert_eq!(
        (again.charged, again.cumulative, again.already),
        (0, 600, true)
    );
    assert!(again.row.is_none());
    assert_eq!(fx.fingerprint(), before);

    // The file grew: only the difference is charged.
    append_lines(
        &sub,
        &[rec(20, "a2", SONNET, [0, 0, 0, 100_000], Some("s1"), true)],
    );
    let grown = transcribed(fx.transcript_charge("s1", Some("x"), &s(&sub)));
    assert_eq!((grown.charged, grown.cumulative), (150, 750));
    assert_eq!(totals(&fx.status()), (750, 0));

    // A shorter file never refunds.
    write_lines(&sub, &[rec(10, "a1", SONNET, mi(1), Some("s1"), true)]);
    let shrunk = transcribed(fx.transcript_charge("s1", Some("x"), &s(&sub)));
    assert_eq!(
        (shrunk.charged, shrunk.cumulative, shrunk.already),
        (0, 750, true)
    );
    assert_eq!(totals(&fx.status()), (750, 0));
}

#[test]
fn an_unattributed_subagent_is_never_given_an_invented_id() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let a = fx.path().join("a.jsonl");
    let b = fx.path().join("b.jsonl");
    write_lines(&a, &[rec(10, "a1", SONNET, mi(1), Some("s1"), true)]);
    write_lines(&b, &[rec(10, "b1", SONNET, mi(2), Some("s1"), true)]);
    let ta = transcribed(fx.transcript_charge("s1", None, &s(&a)));
    let tb = transcribed(fx.transcript_charge("s1", None, &s(&b)));
    assert_eq!((ta.kind, ta.charged), (ExecutorKind::Unattributed, 300));
    assert_eq!(
        (tb.kind, tb.charged),
        (ExecutorKind::Unattributed, 600),
        "two unknown subagents are two transcripts, not one cumulative"
    );
    let st = fx.status();
    assert_eq!(totals(&st), (900, 0));
    assert!(st
        .executors
        .iter()
        .all(|e| e.agent_id.is_none() && e.executor_kind == ExecutorKind::Unattributed));
    // Replaying the first is still idempotent.
    assert!(transcribed(fx.transcript_charge("s1", None, &s(&a))).already);
}

#[test]
fn an_unmeasurable_subagent_is_charged_the_configured_fallback_once_never_omitted() {
    let json = r#"{"schema_version":1,"monthly_cap_usd_cents":100000,"time_zone":"UTC",
        "turn_hold_usd_cents":1000,"subagent_unmeasured_charge_usd_cents":25}"#;
    let mut fx = Fx::new(Some(json), BASE);
    let missing = s(&fx.path().join("never-written.jsonl"));

    let first = transcribed(fx.transcript_charge("s1", Some("x"), &missing));
    assert_eq!((first.charged, first.cumulative), (25, 25));
    assert!(first.unmeasured && !first.usage_known && !first.already);
    let st = fx.status();
    assert_eq!(totals(&st), (25, 0));
    assert_eq!(st.unmeasured_charges, 1);
    let row = fx
        .ledger
        .get_shared_reservation(first.row.unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(row.usage_known, Some(false));
    assert_eq!(row.charge_source, Some(ChargeSource::SubagentTranscript));

    // The next SubagentStop for the same agent does not charge it again.
    let again = transcribed(fx.transcript_charge("s1", Some("x"), &missing));
    assert_eq!(
        (again.charged, again.cumulative, again.already),
        (0, 25, true)
    );
    assert_eq!(fx.status().unmeasured_charges, 1);

    // When the file appears the fallback counts as already charged.
    let sub = fx.path().join("never-written.jsonl");
    write_lines(&sub, &[rec(10, "a1", SONNET, mi(1), Some("s1"), true)]);
    let measured = transcribed(fx.transcript_charge("s1", Some("x"), &s(&sub)));
    assert_eq!((measured.charged, measured.cumulative), (275, 300));
    assert!(measured.usage_known);
    assert_eq!(totals(&fx.status()), (300, 0));
}

#[test]
fn the_unmeasured_fallback_defaults_to_the_turn_hold() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let o = transcribed(fx.transcript_charge("s1", Some("x"), "/nonexistent/x.jsonl"));
    assert_eq!(o.charged, 1_000);
    assert!(o.unmeasured);
    // An empty and a relative path are unreadable too, not silently free.
    let e = transcribed(fx.transcript_charge("s1", Some("y"), ""));
    let r = transcribed(fx.transcript_charge("s1", Some("z"), "relative/path.jsonl"));
    assert_eq!((e.charged, r.charged), (1_000, 1_000));
}

#[test]
fn an_unpriced_or_unverifiable_subagent_is_charged_the_fallback_with_its_flag() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let unpriced = fx.path().join("u.jsonl");
    write_lines(
        &unpriced,
        &[rec(10, "a1", "mystery-model-9", mi(1), Some("s1"), true)],
    );
    let u = transcribed(fx.transcript_charge("s1", Some("u"), &s(&unpriced)));
    assert_eq!(u.charged, 1_000);
    assert!(u.unmeasured && u.unpriced && !u.unverified);

    let nofield = fx.path().join("n.jsonl");
    write_lines(&nofield, &[rec(10, "a1", SONNET, mi(1), None, true)]);
    let n = transcribed(fx.transcript_charge("s1", Some("n"), &s(&nofield)));
    assert_eq!(n.charged, 1_000);
    assert!(n.unmeasured && n.unverified && !n.unpriced);

    let st = fx.status();
    assert_eq!(st.unpriced_models, vec!["mystery-model-9".to_string()]);
    assert_eq!(st.transcript_unverified, 1);
    assert_eq!(st.unmeasured_charges, 2);
}

#[test]
fn another_sessions_subagent_transcript_is_refused_and_charges_nothing() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let sub = fx.path().join("agent.jsonl");
    write_lines(&sub, &[rec(10, "a1", SONNET, mi(5), Some("other"), true)]);
    let before = fx.fingerprint();
    expect_refused(
        fx.transcript_charge("s1", Some("x"), &s(&sub)),
        SharedBudgetRefusal::TranscriptSessionMismatch,
    );
    assert_eq!(fx.fingerprint(), before, "not even the fallback is charged");
    assert_eq!(fx.status().transcript_binding_mismatches, 1);
}

#[test]
fn a_subagent_transcript_over_the_size_cap_is_unmeasured() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let big = fx.path().join("big.jsonl");
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&big).unwrap();
        let line = format!("{}\n", "x".repeat(1023));
        for _ in 0..(33 * 1024) {
            f.write_all(line.as_bytes()).unwrap();
        }
    }
    assert!(std::fs::metadata(&big).unwrap().len() > 32 * 1024 * 1024);
    let o = transcribed(fx.transcript_charge("s1", Some("x"), &s(&big)));
    assert_eq!(o.charged, 1_000);
    assert!(o.unmeasured && !o.usage_known);
}

#[test]
fn transcript_charge_refuses_malformed_identity_and_respects_config_state() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    expect_refused(
        fx.transcript_charge("", Some("x"), "/tmp/x.jsonl"),
        SharedBudgetRefusal::EmptySessionId,
    );
    expect_refused(
        fx.transcript_charge("  ", None, "/tmp/x.jsonl"),
        SharedBudgetRefusal::EmptySessionId,
    );
    expect_refused(
        fx.transcript_charge("execution-owner-v1:abc", Some("x"), "/tmp/x.jsonl"),
        SharedBudgetRefusal::ReservedSessionScope,
    );
    expect_refused(
        fx.transcript_charge("s1", Some(""), "/tmp/x.jsonl"),
        SharedBudgetRefusal::InvalidAgentId,
    );
    assert!(fx.pools().is_empty(), "refusals create nothing");

    std::fs::write(fx.path().join(SHARED_BUDGET_FILE_NAME), "not json").unwrap();
    expect_denied(
        fx.transcript_charge("s1", Some("x"), "/tmp/x.jsonl"),
        SharedBudgetDenyReason::ConfigInvalid,
    );

    let mut none = Fx::new(None, BASE);
    assert_eq!(
        none.transcript_charge("s1", Some("x"), "/tmp/x.jsonl"),
        SharedBudgetOutcome::NotConfigured
    );
    assert!(none.pools().is_empty());
}

#[test]
fn a_subagent_event_time_in_a_later_month_is_booked_now_and_flagged() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let sub = fx.path().join("agent.jsonl");
    let line = rec(0, "a1", SONNET, mi(1), Some("s1"), true)
        .replace(&at(0).format(&Rfc3339).unwrap(), "2027-03-01T00:00:00Z");
    write_lines(&sub, &[line]);
    let o = transcribed(fx.transcript_charge("s1", Some("x"), &s(&sub)));
    assert_eq!(o.charged, 300);
    assert!(o.clamped);
    assert_eq!(totals(&fx.status()), (300, 0));
}

// ---------------------------------------------------------------------
// Parent + subagent never double count
// ---------------------------------------------------------------------

#[test]
fn parent_and_subagent_reconcile_with_no_duplicated_spend() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    fx.admit_hold("s1");

    // The parent transcript carries the main turn AND the subagent's own
    // lines (flagged sidechain); the subagent also has its own file.
    let sub_lines = [
        rec(12, "a1", SONNET, mi(2), Some("s1"), true),
        rec(14, "a2", SONNET, [0, 0, 0, 100_000], Some("s1"), true),
    ];
    let parent = fx.path().join("parent.jsonl");
    let mut parent_lines = vec![rec(10, "m1", SONNET, mi(1), Some("s1"), false)];
    parent_lines.extend(sub_lines.clone());
    parent_lines.push(rec(20, "m2", SONNET, [0, 0, 0, 100_000], Some("s1"), false));
    write_lines(&parent, &parent_lines);
    let sub = fx.path().join("subagents").join("agent-x.jsonl");
    std::fs::create_dir_all(sub.parent().unwrap()).unwrap();
    write_lines(&sub, &sub_lines);

    fx.clock.set(at(60));
    fx.finalize("s1", Some(&s(&parent)));
    let st = fx.status();
    assert_eq!(
        totals(&st),
        (450, 0),
        "parent: 300 + 150, sidechain excluded"
    );

    let o = transcribed(fx.transcript_charge("s1", Some("x"), &s(&sub)));
    assert_eq!(o.charged, 750, "subagent: 600 + 150");

    let st = fx.status();
    assert_eq!(totals(&st), (450 + 750, 0));
    let by_executor: u64 = st.executors.iter().map(|e| e.used_usd_cents).sum();
    assert_eq!(by_executor, st.used_usd_cents, "sum of executors == pool");
    let main = st.executors.iter().find(|e| e.agent_id.is_none()).unwrap();
    let agent = st
        .executors
        .iter()
        .find(|e| e.agent_id.as_deref() == Some("x"))
        .unwrap();
    assert_eq!((main.used_usd_cents, agent.used_usd_cents), (450, 750));
}
