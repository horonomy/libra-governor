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
        self.finalize_for_prompt(session, transcript, None);
    }

    fn finalize_for_prompt(
        &mut self,
        session: &str,
        transcript: Option<&str>,
        prompt: Option<&str>,
    ) {
        match self.call(Request::Finalize {
            agent_id: None,
            prompt_id: prompt.map(String::from),
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
            binding_mismatch,
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
            mismatch: binding_mismatch,
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
    mismatch: bool,
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
fn another_sessions_transcript_is_never_imported_but_the_turn_settles_at_the_hold() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    fx.admit_hold("sB");
    fx.admit_hold("sA");
    let tx = fx.path().join("a.jsonl");
    write_lines(&tx, &[rec(10, "m1", SONNET, mi(5), Some("sA"), false)]);
    fx.clock.set(at(60));

    // Session B's hook was handed session A's transcript: A's 1500 cents
    // are NOT imported into B, but B's turn is not free either.
    fx.finalize("sB", Some(&s(&tx)));
    let st = fx.status();
    assert_eq!(
        totals(&st),
        (1_000, 1_000),
        "B settled at its hold; A's hold stands"
    );
    assert_eq!(st.transcript_binding_mismatches, 1);
    assert_eq!(st.unmeasured_settlements, 1);
    let b = fx.lane("sB");
    assert_eq!(b[0].settled_amount, Some(1_000));
    assert_eq!(b[0].usage_known, Some(false));

    // Spend is counted by its true session's own Stop.
    fx.finalize("sA", Some(&s(&tx)));
    assert_eq!(totals(&fx.status()), (1_000 + 1_500, 0));

    // The watermark moved, so the same foreign file is not judged again by
    // B's next turn: no repeated mismatch for the same stale content.
    fx.clock.set(at(100));
    fx.admit_hold("sB");
    fx.clock.set(at(120));
    fx.finalize("sB", Some(&s(&tx)));
    let st = fx.status();
    assert_eq!(st.transcript_binding_mismatches, 1, "not repeated forever");
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
fn a_window_mixing_own_and_foreign_lines_prices_only_the_own_lines() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    fx.admit_hold("s1");
    let tx = fx.path().join("s1.jsonl");
    write_lines(
        &tx,
        &[
            rec(10, "m1", SONNET, mi(1), Some("s1"), false),
            rec(20, "m2", SONNET, mi(9), Some("someone-else"), false),
        ],
    );
    fx.clock.set(at(60));
    fx.finalize("s1", Some(&s(&tx)));
    let st = fx.status();
    assert_eq!(
        totals(&st),
        (300, 0),
        "own 300 only; the foreign 2700 is not imported"
    );
    assert_eq!(
        st.transcript_binding_mismatches, 0,
        "not a mismatch: there are own lines"
    );
    assert_eq!(st.foreign_lines, 1);
    assert_eq!(fx.lane("s1")[0].usage_known, Some(true));
    // The mark is the newest OWN record, not the foreign one at 20.
    let mark = fx
        .ledger
        .shared_lane_watermark(
            &principal(),
            "s1",
            None,
            ExecutorKind::AgentAbsent,
            OWNER_PREFIX,
        )
        .unwrap()
        .unwrap();
    assert_eq!(mark.at, Some(at(10)));
}

#[test]
fn an_all_foreign_window_books_the_flagged_hold_and_does_not_move_the_mark() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let mine = fx.path().join("b.jsonl");
    fx.admit_hold("sB");
    write_lines(&mine, &[rec(10, "b1", SONNET, mi(1), Some("sB"), false)]);
    fx.clock.set(at(60));
    fx.finalize("sB", Some(&s(&mine)));
    let mark_of = |fx: &Fx| {
        fx.ledger
            .shared_lane_watermark(
                &principal(),
                "sB",
                None,
                ExecutorKind::AgentAbsent,
                OWNER_PREFIX,
            )
            .unwrap()
            .unwrap()
            .at
    };
    assert_eq!(mark_of(&fx), Some(at(10)));

    // B's next hook is handed A's transcript.
    let theirs = fx.path().join("a.jsonl");
    write_lines(&theirs, &[rec(70, "a1", SONNET, mi(5), Some("sA"), false)]);
    fx.clock.set(at(65));
    fx.admit_hold("sB");
    fx.clock.set(at(80));
    fx.finalize("sB", Some(&s(&theirs)));
    let st = fx.status();
    assert_eq!(
        totals(&st),
        (300 + 1_000, 0),
        "A's 1500 is not imported; B pays the flat hold"
    );
    assert_eq!(st.transcript_binding_mismatches, 1);
    assert_eq!(mark_of(&fx), Some(at(10)), "the mark did not jump to now");

    // B's own records stamped between the old mark and now are NOT lost.
    append_lines(&mine, &[rec(75, "b2", SONNET, mi(2), Some("sB"), false)]);
    fx.clock.set(at(100));
    fx.admit_hold("sB");
    fx.clock.set(at(120));
    fx.finalize("sB", Some(&s(&mine)));
    assert_eq!(totals(&fx.status()), (300 + 1_000 + 600, 0));
}

// ---------------------------------------------------------------------
// Future-stamped records (S1)
// ---------------------------------------------------------------------

#[test]
fn a_future_stamped_record_books_the_flagged_hold_and_never_poisons_the_watermark() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    fx.admit_hold("s1");
    let tx = fx.path().join("s1.jsonl");
    // Year ~2090: far past now + skew, inside the accepted year range.
    write_lines(
        &tx,
        &[
            rec(10, "m1", SONNET, mi(1), Some("s1"), false),
            rec(2_000_000_000, "future", SONNET, mi(1), Some("s1"), false),
        ],
    );
    fx.clock.set(at(60));
    fx.finalize("s1", Some(&s(&tx)));
    let st = fx.status();
    assert_eq!(totals(&st), (1_000, 0), "max(300, hold): flagged");
    assert_eq!(st.unmeasured_settlements, 1);
    assert_eq!(
        st.future_timestamp_settlements, 1,
        "the future-stamped window is counted in status"
    );
    assert_eq!(fx.lane("s1")[0].usage_known, Some(false));
    let mark = fx
        .ledger
        .shared_lane_watermark(
            &principal(),
            "s1",
            None,
            ExecutorKind::AgentAbsent,
            OWNER_PREFIX,
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        mark.at,
        Some(at(10)),
        "advanced only to the newest in-bound record"
    );

    // The future line is not remembered as counted: it stays after the
    // mark and flags every window it appears in (conservatively, never
    // below the hold), while the in-bound records beside it are still
    // measured and the lane keeps working.
    fx.clock.set(at(100));
    fx.admit_hold("s1");
    append_lines(&tx, &[rec(110, "m2", SONNET, mi(2), Some("s1"), false)]);
    fx.clock.set(at(150));
    fx.finalize("s1", Some(&s(&tx)));
    assert_eq!(totals(&fx.status()), (1_000 + 1_000, 0), "max(600, hold)");
    let mark = fx
        .ledger
        .shared_lane_watermark(
            &principal(),
            "s1",
            None,
            ExecutorKind::AgentAbsent,
            OWNER_PREFIX,
        )
        .unwrap()
        .unwrap();
    assert!(mark.message_ids.iter().all(|id| id != "future"));
}

#[test]
fn settle_tail_does_not_charge_a_future_stamped_record() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let tx = fx.path().join("s1.jsonl");
    fx.admit_hold("s1");
    write_lines(&tx, &[rec(10, "m1", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(60));
    fx.finalize("s1", Some(&s(&tx)));
    append_lines(
        &tx,
        &[rec(
            2_000_000_000,
            "future",
            SONNET,
            mi(5),
            Some("s1"),
            false,
        )],
    );
    let used = fx.status().used_usd_cents;
    match tail(&mut fx, "s1", &s(&tx)) {
        SharedBudgetOutcome::TailSettled {
            unmeasured,
            amount_charged_usd_cents,
            ..
        } => assert!(unmeasured && amount_charged_usd_cents == 0),
        other => panic!("{other:?}"),
    }
    assert_eq!(fx.status().used_usd_cents, used);
}

// ---------------------------------------------------------------------
// SettleTail hardening (S3) and sweep budget (S5)
// ---------------------------------------------------------------------

#[test]
fn settle_tail_refuses_a_session_with_no_settled_lane() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let tx = fx.path().join("s1.jsonl");
    write_lines(&tx, &[rec(10, "m1", SONNET, mi(9), Some("s1"), false)]);
    // Never admitted.
    expect_refused(
        tail(&mut fx, "s1", &s(&tx)),
        SharedBudgetRefusal::NoSettledLane,
    );
    // Admitted but the hold never settled: still nothing to continue from.
    fx.admit_hold("s1");
    expect_refused(
        tail(&mut fx, "s1", &s(&tx)),
        SharedBudgetRefusal::NoSettledLane,
    );
    assert_eq!(
        totals(&fx.status()),
        (0, 1_000),
        "the whole history was not billed"
    );
}

#[test]
fn a_tail_charge_and_its_watermark_are_written_atomically_and_a_replay_heals() {
    use libra_governor_ledger::{
        CapacitySource, ExecutorAttribution, SettleWatermark, SharedPoolChargeOutcome,
        SharedPoolChargeRequest,
    };
    let mut fx = Fx::new(Some(&cfg()), BASE);
    fx.admit_hold("s1");
    let pool = libra_governor_domain::PoolId("metered:local-budget/monthly-2027-01".into());
    let mark = SettleWatermark {
        at: Some(at(50)),
        message_ids: vec!["m".into()],
    };
    let owner = principal();
    let req = || SharedPoolChargeRequest {
        pool_id: &pool,
        principal_id: &owner,
        session_id: "s1",
        amount: 40,
        unit: &libra_governor_domain::QuotaUnit::UsdCents,
        idempotency_key: "tail:k",
        now: at(60),
        attribution: ExecutorAttribution::main_executor(Some(ChargeSource::TurnHold)),
        capacity_source: CapacitySource::LocalConfigured { cap: 100_000 },
        usage_known: true,
    };
    let lane = |fx: &Fx| {
        fx.ledger
            .shared_lane_watermark(
                &principal(),
                "s1",
                None,
                ExecutorKind::AgentAbsent,
                OWNER_PREFIX,
            )
            .unwrap()
    };
    assert_eq!(lane(&fx), None);
    let out = fx
        .ledger
        .charge_shared_with_watermark(req(), Some(&mark))
        .unwrap();
    assert!(matches!(out, SharedPoolChargeOutcome::Charged { .. }));
    assert_eq!(
        lane(&fx).unwrap().at,
        Some(at(50)),
        "row and mark landed together"
    );
    // An identical replay is a no-op and keeps the mark.
    assert!(matches!(
        fx.ledger
            .charge_shared_with_watermark(req(), Some(&mark))
            .unwrap(),
        SharedPoolChargeOutcome::AlreadyCharged(_)
    ));

    // A row written WITHOUT a mark (an older build / crash window) adopts
    // it on the replay.
    let mut req2 = req();
    req2.idempotency_key = "tail:k2";
    fx.ledger.charge_shared(req2).unwrap();
    let mut req3 = req();
    req3.idempotency_key = "tail:k2";
    let later = SettleWatermark {
        at: Some(at(58)),
        message_ids: vec![],
    };
    fx.ledger
        .charge_shared_with_watermark(req3, Some(&later))
        .unwrap();
    assert_eq!(lane(&fx).unwrap().at, Some(at(58)));
}

#[test]
fn a_poisoned_stored_watermark_far_past_its_row_is_ignored() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    fx.admit_hold("s1");
    let tx = fx.path().join("s1.jsonl");
    write_lines(&tx, &[rec(10, "m1", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(60));
    fx.finalize("s1", Some(&s(&tx)));
    let id = fx.lane("s1")[0].id;
    // An already-poisoned row (older build / hand edit): a mark years ahead.
    fx.ledger
        .record_shared_watermark(
            id,
            &libra_governor_ledger::SettleWatermark {
                at: Some(at(2_000_000_000)),
                message_ids: vec![],
            },
        )
        .unwrap();
    let lane = fx
        .ledger
        .shared_lane_watermark(
            &principal(),
            "s1",
            None,
            ExecutorKind::AgentAbsent,
            OWNER_PREFIX,
        )
        .unwrap();
    assert_eq!(lane, None, "the poisoned mark is not trusted");
    // So the next window still counts real records.
    fx.clock.set(at(100));
    fx.admit_hold("s1");
    append_lines(&tx, &[rec(110, "m2", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(150));
    fx.finalize("s1", Some(&s(&tx)));
    assert_eq!(fx.status().used_usd_cents, 600);
}

#[test]
fn the_stop_sweep_respects_its_byte_budget_and_skips_unchanged_files() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let sid = "s1";
    let main = fx.path().join(format!("{sid}.jsonl"));
    let dir = fx.path().join(sid).join("subagents");
    std::fs::create_dir_all(&dir).unwrap();
    write_lines(&main, &[rec(10, "m1", SONNET, mi(1), Some(sid), false)]);
    write_lines(
        &dir.join("agent-a1.jsonl"),
        &[sub_rec(12, "x", mi(1), sid, "a1")],
    );
    write_lines(
        &dir.join("agent-a2.jsonl"),
        &[sub_rec(13, "y", mi(1), sid, "a2")],
    );

    // A budget smaller than any file defers both.
    let n = libra_governor_daemon::shared_budget::sweep_with_budget(
        &mut fx.ledger,
        &fx.config.shared_budget,
        sid,
        &s(&main),
        10,
    );
    // The first file is always processed, so progress is guaranteed.
    assert_eq!((n.charged, n.deferred), (1, 1));
    let st = fx.status();
    assert_eq!((st.used_usd_cents, st.sweep_deferred_files), (300, 1));

    // With room, both are charged...
    let n = libra_governor_daemon::shared_budget::sweep_with_budget(
        &mut fx.ledger,
        &fx.config.shared_budget,
        sid,
        &s(&main),
        1 << 30,
    );
    assert_eq!(n.charged, 1, "only the file not yet swept is charged");
    assert_eq!(fx.status().used_usd_cents, 600);

    // ...and an unchanged file is skipped before the budget is even
    // consulted (nothing is deferred again).
    let n = libra_governor_daemon::shared_budget::sweep_with_budget(
        &mut fx.ledger,
        &fx.config.shared_budget,
        sid,
        &s(&main),
        10,
    );
    assert_eq!((n.charged, n.deferred), (0, 0));
    assert_eq!(fx.status().sweep_deferred_files, 1);
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
fn another_sessions_subagent_transcript_is_not_imported_and_takes_the_fallback_once() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let sub = fx.path().join("agent.jsonl");
    write_lines(&sub, &[rec(10, "a1", SONNET, mi(5), Some("other"), true)]);
    let o = transcribed(fx.transcript_charge("s1", Some("x"), &s(&sub)));
    assert_eq!(o.charged, 1_000, "the fallback, not the foreign 1500");
    assert!(o.mismatch && o.unmeasured && !o.usage_known);
    let again = transcribed(fx.transcript_charge("s1", Some("x"), &s(&sub)));
    assert_eq!((again.charged, again.already), (0, true));
    assert_eq!(totals(&fx.status()), (1_000, 0));
    assert_eq!(fx.status().transcript_binding_mismatches, 2);
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

// ---------------------------------------------------------------------
// Flush lag, ordering, watermark (real-host findings, Claude Code 2.1.306)
// ---------------------------------------------------------------------

/// A subagent line: native `agentId` and `isSidechain`.
fn sub_rec(secs: i64, id: &str, tiers: [u64; 4], session: &str, agent: &str) -> String {
    rec(secs, id, SONNET, tiers, Some(session), true).replace(
        "\"isSidechain\":true,",
        &format!("\"isSidechain\":true,\"agentId\":\"{agent}\","),
    )
}

#[test]
fn a_record_flushed_after_the_stop_is_recovered_by_the_next_stop() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let tx = fx.path().join("s1.jsonl");
    fx.admit_hold("s1");
    write_lines(&tx, &[rec(10, "m1", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(60));
    fx.finalize("s1", Some(&s(&tx)));
    assert_eq!(totals(&fx.status()), (300, 0));

    // The host flushes m2 (stamped 55, BEFORE the Stop at 60) only later.
    append_lines(&tx, &[rec(55, "m2", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(100));
    fx.admit_hold("s1");
    append_lines(&tx, &[rec(110, "m3", SONNET, mi(2), Some("s1"), false)]);
    fx.clock.set(at(150));
    fx.finalize("s1", Some(&s(&tx)));
    assert_eq!(
        totals(&fx.status()),
        (300 + 300 + 600, 0),
        "m2 was stamped before the first Stop's wall clock but is not lost"
    );
}

#[test]
fn tail_blocks_of_an_already_counted_message_are_not_counted_again() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let tx = fx.path().join("s1.jsonl");
    fx.admit_hold("s1");
    // Only block 0 of message X is in the file at the first Stop.
    write_lines(&tx, &[rec(10, "x", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(60));
    fx.finalize("s1", Some(&s(&tx)));
    assert_eq!(totals(&fx.status()), (300, 0));

    // Block 1 of X (same id, same usage) arrives later, plus a new message.
    append_lines(&tx, &[rec(12, "x", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(100));
    fx.admit_hold("s1");
    append_lines(&tx, &[rec(110, "y", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(150));
    fx.finalize("s1", Some(&s(&tx)));
    assert_eq!(
        totals(&fx.status()),
        (600, 0),
        "X counted once across both windows; only Y is new"
    );
}

#[test]
fn an_out_of_order_old_line_does_not_hide_newer_records_above_it() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let tx = fx.path().join("s1.jsonl");
    fx.admit_hold("s1");
    write_lines(&tx, &[rec(100, "m0", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(150));
    fx.finalize("s1", Some(&s(&tx)));
    assert_eq!(totals(&fx.status()), (300, 0));

    // File order: A (new), O (an OLD user line, stamped 95), B (new).
    let old_user = format!(
        r#"{{"type":"user","timestamp":"{}","sessionId":"s1","isSidechain":false,"message":{{"role":"user","content":"x"}}}}"#,
        at(95).format(&Rfc3339).unwrap()
    );
    fx.admit_hold("s1");
    append_lines(
        &tx,
        &[
            rec(200, "a", SONNET, mi(1), Some("s1"), false),
            old_user,
            rec(210, "b", SONNET, mi(2), Some("s1"), false),
        ],
    );
    fx.clock.set(at(250));
    fx.finalize("s1", Some(&s(&tx)));
    assert_eq!(
        totals(&fx.status()),
        (300 + 900, 0),
        "both A and B are counted"
    );
}

#[test]
fn a_token_count_that_is_not_an_integer_is_flagged_not_dropped() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    fx.admit_hold("s1");
    let tx = fx.path().join("s1.jsonl");
    let bad = rec(10, "m1", SONNET, mi(1), Some("s1"), false)
        .replace("\"input_tokens\":1000000", "\"input_tokens\":1000000.5");
    write_lines(&tx, &[bad, rec(20, "m2", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(60));
    fx.finalize("s1", Some(&s(&tx)));
    let st = fx.status();
    assert_eq!(
        totals(&st),
        (1_000, 0),
        "the hold, not a silent partial sum"
    );
    assert_eq!(st.unmeasured_settlements, 1);
    assert_eq!(fx.lane("s1")[0].usage_known, Some(false));
}

#[test]
fn two_stops_for_one_invocation_are_handled() {
    // Background-subagent case: a second Stop (and a second prompt) can fire
    // for one print invocation.
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let tx = fx.path().join("s1.jsonl");
    fx.admit_hold("s1");
    write_lines(&tx, &[rec(10, "m1", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(60));
    fx.finalize("s1", Some(&s(&tx)));
    // A second Stop with no hold in between: nothing to settle.
    let before = fx.fingerprint();
    fx.finalize("s1", Some(&s(&tx)));
    assert_eq!(fx.fingerprint(), before);
    // The notification turn: a second prompt admits a new hold, a second Stop
    // settles it from the records written meanwhile.
    fx.clock.set(at(70));
    fx.admit_hold("s1");
    append_lines(&tx, &[rec(80, "m2", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(90));
    fx.finalize("s1", Some(&s(&tx)));
    assert_eq!(totals(&fx.status()), (600, 0));
}

// ---------------------------------------------------------------------
// Real-shaped fixtures (synthetic ids, structure of Claude Code 2.1.306)
// ---------------------------------------------------------------------

const REAL_SESSION: &str = "00000000-0000-4000-8000-0000000000a1";
const REAL_AGENT: &str = "a0123456789abcdef";
const REAL_START: &str = "2026-10-24T03:05:20Z";
const REAL_STOP: &str = "2026-10-24T03:05:30Z";

fn stage_real_layout(fx: &Fx) -> (std::path::PathBuf, std::path::PathBuf) {
    let main = fx.path().join(format!("{REAL_SESSION}.jsonl"));
    let subdir = fx.path().join(REAL_SESSION).join("subagents");
    std::fs::create_dir_all(&subdir).unwrap();
    let sub = subdir.join(format!("agent-{REAL_AGENT}.jsonl"));
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    std::fs::copy(fixtures.join("claude_code_2_1_306_main.jsonl"), &main).unwrap();
    std::fs::copy(fixtures.join("claude_code_2_1_306_subagent.jsonl"), &sub).unwrap();
    (main, sub)
}

#[test]
fn the_real_shaped_main_transcript_counts_each_message_once_and_ignores_the_noise() {
    let mut fx = Fx::new(Some(&cfg()), REAL_START);
    let (main, _sub) = stage_real_layout(&fx);
    fx.admit_hold(REAL_SESSION);
    fx.clock.set(t(REAL_STOP));
    fx.finalize(REAL_SESSION, Some(&s(&main)));
    // msg_a (two content-block lines): 606c; msg_b 200c; msg_c 200c. The
    // timestamp-less lines, the out-of-order user line and the tool-result
    // usage (9M tokens) contribute nothing.
    // The same Stop also swept the staged subagent file: 264c.
    let st = fx.status();
    assert_eq!(totals(&st), (1_006 + 264, 0));
    let main_row = st.executors.iter().find(|e| e.agent_id.is_none()).unwrap();
    assert_eq!(main_row.used_usd_cents, 1_006);
}

#[test]
fn the_stop_sweep_charges_a_subagent_file_that_lagged_its_own_subagent_stop() {
    let mut fx = Fx::new(Some(&cfg()), REAL_START);
    let (main, sub) = stage_real_layout(&fx);
    fx.admit_hold(REAL_SESSION);

    // SubagentStop fired when the file had no assistant line yet.
    let full = std::fs::read_to_string(&sub).unwrap();
    let no_assistant: String = full
        .lines()
        .filter(|l| !l.contains("\"assistant\""))
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(&sub, no_assistant).unwrap();
    // Explicit SubagentStop with nothing measurable: the unmeasured fallback
    // (1000c) is charged once, flagged.
    let early = transcribed(fx.transcript_charge(REAL_SESSION, Some(REAL_AGENT), &s(&sub)));
    assert_eq!((early.charged, early.unmeasured), (1_000, true));

    // The file catches up; the PARENT's Stop sweeps it: 264c measured is
    // below the 1000c fallback already booked, so nothing more is charged.
    std::fs::write(&sub, full).unwrap();
    fx.clock.set(t(REAL_STOP));
    fx.finalize(REAL_SESSION, Some(&s(&main)));
    let st = fx.status();
    assert_eq!(st.used_usd_cents, 1_006 + 1_000);
    let sub_row = st
        .executors
        .iter()
        .find(|e| e.agent_id.as_deref() == Some(REAL_AGENT))
        .unwrap();
    assert_eq!(sub_row.used_usd_cents, 1_000);
}

#[test]
fn the_stop_sweep_tops_up_a_subagent_by_delta_and_skips_files_with_nothing_yet() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let sid = "s1";
    let main = fx.path().join(format!("{sid}.jsonl"));
    let dir = fx.path().join(sid).join("subagents");
    std::fs::create_dir_all(&dir).unwrap();
    let a = dir.join("agent-aaa1.jsonl");
    let empty = dir.join("agent-bbb2.jsonl");
    write_lines(&main, &[rec(10, "m1", SONNET, mi(1), Some(sid), false)]);
    write_lines(&a, &[sub_rec(12, "s1a", mi(1), sid, "aaa1")]);
    // A subagent that has written only its prompt so far.
    write_lines(
        &empty,
        &[format!(
            r#"{{"type":"user","timestamp":"{}","sessionId":"s1","isSidechain":true,"agentId":"bbb2"}}"#,
            at(11).format(&Rfc3339).unwrap()
        )],
    );
    // aaa1's own SubagentStop saw 300c.
    let first = transcribed(fx.transcript_charge(sid, Some("aaa1"), &s(&a)));
    assert_eq!(first.charged, 300);
    // It then wrote more.
    append_lines(&a, &[sub_rec(13, "s1b", mi(2), sid, "aaa1")]);

    fx.admit_hold(sid);
    fx.clock.set(at(60));
    fx.finalize(sid, Some(&s(&main)));
    let st = fx.status();
    // parent 300 + aaa1 (300 + 600 delta) = 1200; bbb2 billed nothing.
    assert_eq!(totals(&st), (1_200, 0));
    assert!(st
        .executors
        .iter()
        .all(|e| e.agent_id.as_deref() != Some("bbb2")));

    // A second Stop sweeps again and adds nothing.
    fx.admit_hold(sid);
    fx.clock.set(at(100));
    append_lines(&main, &[rec(90, "m2", SONNET, mi(1), Some(sid), false)]);
    fx.finalize(sid, Some(&s(&main)));
    assert_eq!(totals(&fx.status()), (1_200 + 300, 0));
}

#[test]
fn a_swept_subagent_file_naming_another_agent_takes_the_fallback_once() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let sid = "s1";
    let main = fx.path().join(format!("{sid}.jsonl"));
    let dir = fx.path().join(sid).join("subagents");
    std::fs::create_dir_all(&dir).unwrap();
    write_lines(&main, &[rec(10, "m1", SONNET, mi(1), Some(sid), false)]);
    // The file name says aaa1; its lines say zzz9.
    write_lines(
        &dir.join("agent-aaa1.jsonl"),
        &[sub_rec(12, "q", mi(9), sid, "zzz9")],
    );
    fx.admit_hold(sid);
    fx.clock.set(at(60));
    fx.finalize(sid, Some(&s(&main)));
    fx.admit_hold(sid);
    fx.clock.set(at(80));
    fx.finalize(sid, Some(&s(&main)));
    let st = fx.status();
    let row = st
        .executors
        .iter()
        .find(|e| e.agent_id.as_deref() == Some("aaa1"))
        .unwrap();
    assert_eq!(
        row.used_usd_cents, 1_000,
        "fallback once, foreign 2700 never imported"
    );
}

// ---------------------------------------------------------------------
// SettleTail
// ---------------------------------------------------------------------

fn tail(fx: &mut Fx, session: &str, path: &str) -> SharedBudgetOutcome {
    fx.sb(SharedBudgetRequest::SettleTail {
        session_id: session.to_string(),
        transcript_path: path.to_string(),
    })
}

fn tail_charged(o: &SharedBudgetOutcome) -> (u64, bool) {
    match o {
        SharedBudgetOutcome::TailSettled {
            amount_charged_usd_cents,
            already_charged,
            ..
        } => (*amount_charged_usd_cents, *already_charged),
        other => panic!("expected TailSettled, got {other:?}"),
    }
}

#[test]
fn settle_tail_charges_the_final_message_exactly_once() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let tx = fx.path().join("s1.jsonl");
    fx.admit_hold("s1");
    write_lines(&tx, &[rec(10, "m1", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(60));
    fx.finalize("s1", Some(&s(&tx)));
    assert_eq!(totals(&fx.status()), (300, 0));

    // The session's final message is flushed after the last Stop; there is
    // no next Stop to recover it. SessionEnd runs SettleTail.
    append_lines(
        &tx,
        &[
            rec(58, "last", SONNET, mi(2), Some("s1"), false),
            rec(59, "last", SONNET, mi(2), Some("s1"), false),
        ],
    );
    fx.clock.set(at(200));
    let first = tail(&mut fx, "s1", &s(&tx));
    assert_eq!(tail_charged(&first), (600, false));
    let st = fx.status();
    assert_eq!(totals(&st), (900, 0));
    let row = st.executors.iter().find(|e| e.session_id == "s1").unwrap();
    assert_eq!(
        (row.executor_kind, row.agent_id.as_deref()),
        (ExecutorKind::AgentAbsent, None)
    );

    // Idempotent: a repeat charges nothing.
    let before = fx.fingerprint();
    let second = tail(&mut fx, "s1", &s(&tx));
    assert_eq!(tail_charged(&second), (0, false));
    assert_eq!(fx.fingerprint(), before);
    assert_eq!(totals(&fx.status()), (900, 0));

    // And a later Stop does not recount it either.
    fx.admit_hold("s1");
    fx.clock.set(at(300));
    fx.finalize("s1", Some(&s(&tx)));
    assert_eq!(
        totals(&fx.status()),
        (900 + 1_000, 0),
        "empty window -> hold"
    );
}

#[test]
fn settle_tail_charge_row_is_a_known_usage_main_executor_charge() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let tx = fx.path().join("s1.jsonl");
    fx.admit_hold("s1");
    write_lines(&tx, &[rec(10, "m1", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(60));
    fx.finalize("s1", Some(&s(&tx)));
    append_lines(&tx, &[rec(70, "last", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(100));
    let o = tail(&mut fx, "s1", &s(&tx));
    let SharedBudgetOutcome::TailSettled {
        reservation_id: Some(id),
        ..
    } = o
    else {
        panic!("expected a written row");
    };
    let row = fx.ledger.get_shared_reservation(id).unwrap().unwrap();
    assert_eq!(row.usage_known, Some(true));
    assert_eq!(row.executor_kind, Some(ExecutorKind::AgentAbsent));
    assert_eq!(row.charge_source, Some(ChargeSource::TurnHold));
}

#[test]
fn settle_tail_refuses_foreign_and_unmeasurable_transcripts_without_charging() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let tx = fx.path().join("s1.jsonl");
    fx.admit_hold("s1");
    write_lines(&tx, &[rec(10, "m1", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(60));
    fx.finalize("s1", Some(&s(&tx)));
    let used = fx.status().used_usd_cents;

    let foreign = fx.path().join("f.jsonl");
    write_lines(
        &foreign,
        &[rec(70, "z", SONNET, mi(9), Some("other"), false)],
    );
    match tail(&mut fx, "s1", &s(&foreign)) {
        SharedBudgetOutcome::TailSettled {
            binding_mismatch,
            amount_charged_usd_cents,
            ..
        } => {
            assert!(binding_mismatch);
            assert_eq!(amount_charged_usd_cents, 0);
        }
        other => panic!("{other:?}"),
    }
    let noid = fx.path().join("n.jsonl");
    write_lines(&noid, &[rec(70, "z", SONNET, mi(9), None, false)]);
    match tail(&mut fx, "s1", &s(&noid)) {
        SharedBudgetOutcome::TailSettled {
            transcript_unverified,
            unmeasured,
            ..
        } => {
            assert!(transcript_unverified && unmeasured);
        }
        other => panic!("{other:?}"),
    }
    // A lane that was never admitted, an empty session, a relative path.
    expect_refused(
        tail(&mut fx, "never-admitted", &s(&tx)),
        SharedBudgetRefusal::NoSettledLane,
    );
    expect_refused(
        tail(&mut fx, "", &s(&tx)),
        SharedBudgetRefusal::EmptySessionId,
    );
    match tail(&mut fx, "s1", "relative.jsonl") {
        SharedBudgetOutcome::TailSettled { unmeasured, .. } => assert!(unmeasured),
        other => panic!("{other:?}"),
    }
    assert_eq!(fx.status().used_usd_cents, used, "nothing was charged");

    std::fs::write(fx.path().join(SHARED_BUDGET_FILE_NAME), "nope").unwrap();
    expect_denied(
        tail(&mut fx, "s1", &s(&tx)),
        SharedBudgetDenyReason::ConfigInvalid,
    );
    let mut none = Fx::new(None, BASE);
    assert_eq!(
        tail(&mut none, "s1", "/x.jsonl"),
        SharedBudgetOutcome::NotConfigured
    );
}

#[test]
fn many_files_larger_than_the_budget_drain_over_successive_sweeps() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let sid = "s1";
    let main = fx.path().join(format!("{sid}.jsonl"));
    let dir = fx.path().join(sid).join("subagents");
    std::fs::create_dir_all(&dir).unwrap();
    write_lines(&main, &[rec(10, "m1", SONNET, mi(1), Some(sid), false)]);
    for (i, agent) in ["a1", "a2", "a3"].iter().enumerate() {
        write_lines(
            &dir.join(format!("agent-{agent}.jsonl")),
            &[sub_rec(12 + i as i64, &format!("x{i}"), mi(1), sid, agent)],
        );
    }
    // Every file is larger than the 10-byte budget.
    let mut deferred = Vec::new();
    for _ in 0..4 {
        let r = libra_governor_daemon::shared_budget::sweep_with_budget(
            &mut fx.ledger,
            &fx.config.shared_budget,
            sid,
            &s(&main),
            10,
        );
        deferred.push((r.charged, r.deferred));
    }
    assert_eq!(deferred, vec![(1, 2), (1, 1), (1, 0), (0, 0)]);
    assert_eq!(
        fx.status().used_usd_cents,
        900,
        "all three drained, none starved"
    );
}

#[test]
fn a_file_the_read_cap_truncates_takes_the_fallback_once_in_a_sweep() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let sid = "s1";
    let main = fx.path().join(format!("{sid}.jsonl"));
    let dir = fx.path().join(sid).join("subagents");
    std::fs::create_dir_all(&dir).unwrap();
    write_lines(&main, &[rec(10, "m1", SONNET, mi(1), Some(sid), false)]);
    {
        use std::io::Write;
        let mut f = std::fs::File::create(dir.join("agent-big.jsonl")).unwrap();
        let line = format!("{}\n", "x".repeat(1023));
        for _ in 0..(33 * 1024) {
            f.write_all(line.as_bytes()).unwrap();
        }
    }
    let sweep = |fx: &mut Fx| {
        libra_governor_daemon::shared_budget::sweep_with_budget(
            &mut fx.ledger,
            &fx.config.shared_budget,
            sid,
            &s(&main),
            1 << 30,
        )
    };
    assert_eq!(sweep(&mut fx).charged, 1, "not silently skipped");
    let st = fx.status();
    assert_eq!(st.unmeasured_charges, 1);
    assert_eq!(st.used_usd_cents, 1_000);
    assert_eq!(sweep(&mut fx).charged, 0, "and not charged again");
    assert_eq!(fx.status().used_usd_cents, 1_000);
}

#[test]
fn a_future_stamped_record_beside_in_bound_spend_books_the_priced_amount() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    fx.admit_hold("s1");
    let tx = fx.path().join("s1.jsonl");
    // 2 x 600 = 1200 priced in-bound (above the 1000 hold) + one ~2090 line.
    write_lines(
        &tx,
        &[
            rec(10, "m1", SONNET, mi(2), Some("s1"), false),
            rec(20, "m2", SONNET, mi(2), Some("s1"), false),
            rec(2_000_000_000, "future", SONNET, mi(7), Some("s1"), false),
        ],
    );
    fx.clock.set(at(60));
    fx.finalize("s1", Some(&s(&tx)));
    let st = fx.status();
    assert_eq!(
        totals(&st),
        (1_200, 0),
        "the measured in-bound spend, not the flat hold"
    );
    assert_eq!(st.unmeasured_settlements, 1, "flagged future_timestamp");
    let mark = fx
        .ledger
        .shared_lane_watermark(
            &principal(),
            "s1",
            None,
            ExecutorKind::AgentAbsent,
            OWNER_PREFIX,
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        mark.at,
        Some(at(20)),
        "never beyond the newest in-bound record"
    );
    for id in ["m1", "m2"] {
        assert!(mark.message_ids.iter().any(|m| m == id), "{id} remembered");
    }
    assert!(mark.message_ids.iter().all(|m| m != "future"));

    // m1 and m2 are not recounted: the next window prices only m3 (300) and,
    // the future line still being there, books max(300, hold).
    fx.clock.set(at(100));
    fx.admit_hold("s1");
    append_lines(&tx, &[rec(110, "m3", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(150));
    fx.finalize("s1", Some(&s(&tx)));
    assert_eq!(totals(&fx.status()), (1_200 + 1_000, 0));
}

#[test]
fn an_oversize_file_costs_the_sweep_no_more_than_one_read_can_scan() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let sid = "s1";
    let main = fx.path().join(format!("{sid}.jsonl"));
    let dir = fx.path().join(sid).join("subagents");
    std::fs::create_dir_all(&dir).unwrap();
    write_lines(&main, &[rec(10, "m1", SONNET, mi(1), Some(sid), false)]);
    {
        use std::io::Write;
        let mut f = std::fs::File::create(dir.join("agent-a-big.jsonl")).unwrap();
        let line = format!("{}\n", "x".repeat(1023));
        for _ in 0..(33 * 1024) {
            f.write_all(line.as_bytes()).unwrap();
        }
    }
    write_lines(
        &dir.join("agent-b-small.jsonl"),
        &[sub_rec(12, "s", mi(1), sid, "b-small")],
    );
    // Room for one full read (32 MiB) plus a little: the big file is charged
    // at the read cap, not its 33 MiB length, so the small file still fits.
    let budget = 32 * 1024 * 1024 + 4096;
    let r = libra_governor_daemon::shared_budget::sweep_with_budget(
        &mut fx.ledger,
        &fx.config.shared_budget,
        sid,
        &s(&main),
        budget,
    );
    assert_eq!((r.charged, r.deferred), (2, 0));
}

#[test]
fn settle_tail_starts_after_the_latest_settlement_when_no_mark_was_ever_written() {
    // Every earlier settlement was unmeasured (no session field), so the lane
    // has no mark. The tail must not price the session's whole history.
    let mut fx = Fx::new(Some(&cfg()), BASE);
    fx.admit_hold("s1");
    let unverifiable = fx.path().join("u.jsonl");
    write_lines(&unverifiable, &[rec(10, "u1", SONNET, mi(1), None, false)]);
    fx.clock.set(at(60));
    fx.finalize("s1", Some(&s(&unverifiable)));
    assert_eq!(totals(&fx.status()), (1_000, 0));

    let tx = fx.path().join("s1.jsonl");
    write_lines(
        &tx,
        &[
            rec(10, "old1", SONNET, mi(5), Some("s1"), false),
            rec(20, "old2", SONNET, mi(5), Some("s1"), false),
            rec(70, "after", SONNET, mi(1), Some("s1"), false),
        ],
    );
    fx.clock.set(at(100));
    let o = tail(&mut fx, "s1", &s(&tx));
    assert_eq!(
        tail_charged(&o),
        (300, false),
        "only the record after the settlement"
    );
    assert_eq!(totals(&fx.status()), (1_300, 0));
}

#[test]
fn settle_tail_with_a_poisoned_mark_falls_back_to_the_latest_settlement() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    fx.admit_hold("s1");
    let tx = fx.path().join("s1.jsonl");
    write_lines(&tx, &[rec(10, "m1", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(60));
    fx.finalize("s1", Some(&s(&tx)));
    let id = fx.lane("s1")[0].id;
    fx.ledger
        .record_shared_watermark(
            id,
            &libra_governor_ledger::SettleWatermark {
                at: Some(at(2_000_000_000)),
                message_ids: vec![],
            },
        )
        .unwrap();
    append_lines(&tx, &[rec(70, "m2", SONNET, mi(2), Some("s1"), false)]);
    fx.clock.set(at(100));
    // The clamped (ignored) mark falls back to the settle time 60: m1 (ts 10,
    // already booked) is not charged again.
    assert_eq!(tail_charged(&tail(&mut fx, "s1", &s(&tx))), (600, false));
}

#[test]
fn a_future_only_window_leaves_no_state_and_the_lane_measures_again() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    fx.admit_hold("s1");
    let tx = fx.path().join("s1.jsonl");
    write_lines(
        &tx,
        &[rec(
            2_000_000_000,
            "future",
            SONNET,
            mi(3),
            Some("s1"),
            false,
        )],
    );
    fx.clock.set(at(60));
    fx.finalize("s1", Some(&s(&tx)));
    assert_eq!(totals(&fx.status()), (1_000, 0));
    assert_eq!(
        fx.ledger
            .shared_lane_watermark(
                &principal(),
                "s1",
                None,
                ExecutorKind::AgentAbsent,
                OWNER_PREFIX
            )
            .unwrap(),
        None,
        "nothing was poisoned or remembered"
    );
    // Later normal records are measured (the lane still books at least the
    // hold while the bad line is present).
    fx.clock.set(at(100));
    fx.admit_hold("s1");
    append_lines(&tx, &[rec(110, "m1", SONNET, mi(5), Some("s1"), false)]);
    fx.clock.set(at(150));
    fx.finalize("s1", Some(&s(&tx)));
    assert_eq!(
        totals(&fx.status()),
        (1_000 + 1_500, 0),
        "priced above the hold"
    );
    assert_eq!(fx.lane("s1")[1].usage_known, Some(true));
}

#[test]
fn a_future_record_is_counted_exactly_once_when_the_clock_catches_up() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    fx.admit_hold("s1");
    let tx = fx.path().join("s1.jsonl");
    // "later" is stamped one hour ahead: beyond the 5 minute skew.
    write_lines(
        &tx,
        &[
            rec(10, "m1", SONNET, mi(1), Some("s1"), false),
            rec(3_660, "later", SONNET, mi(2), Some("s1"), false),
        ],
    );
    fx.clock.set(at(60));
    fx.finalize("s1", Some(&s(&tx)));
    assert_eq!(totals(&fx.status()), (1_000, 0), "max(300, hold), flagged");

    // The clock passes the record's stamp: it is now an ordinary record.
    fx.clock.set(at(3_800));
    fx.admit_hold("s1");
    fx.clock.set(at(4_000));
    fx.finalize("s1", Some(&s(&tx)));
    assert_eq!(
        totals(&fx.status()),
        (1_000 + 600, 0),
        "counted once, not lost"
    );
    assert_eq!(fx.lane("s1")[1].usage_known, Some(true));

    // A third window does not count it again.
    fx.clock.set(at(4_100));
    fx.admit_hold("s1");
    append_lines(&tx, &[rec(4_150, "m3", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(4_200));
    fx.finalize("s1", Some(&s(&tx)));
    assert_eq!(totals(&fx.status()), (1_000 + 600 + 300, 0));
}

#[test]
fn a_future_window_with_unpriced_in_bound_records_does_not_advance_the_mark() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    fx.admit_hold("s1");
    let tx = fx.path().join("s1.jsonl");
    write_lines(
        &tx,
        &[
            rec(10, "u", "mystery-model-9", mi(1), Some("s1"), false),
            rec(2_000_000_000, "future", SONNET, mi(1), Some("s1"), false),
        ],
    );
    fx.clock.set(at(60));
    fx.finalize("s1", Some(&s(&tx)));
    assert_eq!(totals(&fx.status()), (1_000, 0));
    assert_eq!(
        fx.ledger
            .shared_lane_watermark(
                &principal(),
                "s1",
                None,
                ExecutorKind::AgentAbsent,
                OWNER_PREFIX
            )
            .unwrap(),
        None,
        "unpriced in-bound records are rescanned, not skipped"
    );
}

// ---------------------------------------------------------------------
// HORO-2246: flush lag. The `Stop` hook polls `TranscriptReady` before it
// sends `Finalize`; these tests pin what "ready" means, that a probe is
// read-only, and that a late flush is measured once and never double
// counted. Fixtures are SYNTHETIC; no sleep is relied on (the flush is a
// file write between two requests, the clock is injected).
// ---------------------------------------------------------------------

impl Fx {
    fn ready(&mut self, session: &str, path: &Path) -> bool {
        match self.sb(SharedBudgetRequest::TranscriptReady {
            session_id: session.to_string(),
            transcript_path: s(path),
        }) {
            SharedBudgetOutcome::TranscriptReadiness { ready } => ready,
            other => panic!("expected TranscriptReadiness, got {other:?}"),
        }
    }

    fn tail(&mut self, session: &str, path: &Path) -> SharedBudgetOutcome {
        self.sb(SharedBudgetRequest::SettleTail {
            session_id: session.to_string(),
            transcript_path: s(path),
        })
    }
}

fn user_line(secs: i64, session: &str) -> String {
    format!(
        r#"{{"type":"user","timestamp":"{}","sessionId":"{session}","message":{{"role":"user","content":"hi"}}}}"#,
        at(secs).format(&Rfc3339).unwrap()
    )
}

#[test]
fn ready_is_false_only_while_waiting_could_change_what_a_stop_books() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    // Not ready: the file is not there yet (first turn of a session).
    fx.admit_hold("absent");
    assert!(!fx.ready("absent", &fx.path().join("absent.jsonl")));

    // Not ready: the file exists but holds no usage line (only the prompt).
    fx.admit_hold("promptonly");
    let p = fx.path().join("promptonly.jsonl");
    write_lines(&p, &[user_line(5, "promptonly")]);
    assert!(!fx.ready("promptonly", &p));
    // Ready the moment the assistant line lands.
    append_lines(
        &p,
        &[rec(10, "m1", SONNET, mi(1), Some("promptonly"), false)],
    );
    assert!(fx.ready("promptonly", &p));

    // Not ready on a resumed session: only the previous turn's lines are in
    // the file, and they are behind the lane's watermark.
    let tx = fx.path().join("resumed.jsonl");
    fx.admit_hold("resumed");
    write_lines(&tx, &[rec(10, "r1", SONNET, mi(1), Some("resumed"), false)]);
    fx.clock.set(at(60));
    fx.finalize("resumed", Some(&s(&tx)));
    fx.clock.set(at(100));
    fx.admit_hold("resumed");
    assert!(
        !fx.ready("resumed", &tx),
        "turn 1's line is already counted"
    );
    append_lines(
        &tx,
        &[rec(110, "r2", SONNET, mi(1), Some("resumed"), false)],
    );
    assert!(fx.ready("resumed", &tx));
}

#[test]
fn ready_is_true_for_every_outcome_that_waiting_cannot_improve() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let cases: Vec<(&str, Vec<String>)> = vec![
        // A different session's lines only: settles as binding_mismatch.
        (
            "foreign",
            vec![rec(10, "m1", SONNET, mi(1), Some("other"), false)],
        ),
        // A usage line with no session field: transcript_unverified.
        (
            "unverified",
            vec![rec(10, "m1", SONNET, mi(1), None, false)],
        ),
        // A model the pinned table cannot price: unpriced_model.
        (
            "unpriced",
            vec![rec(
                10,
                "m1",
                "no-such-model",
                mi(1),
                Some("unpriced"),
                false,
            )],
        ),
        // A line stamped far beyond now + skew: future_timestamp.
        (
            "future",
            vec![rec(10_000, "m1", SONNET, mi(1), Some("future"), false)],
        ),
    ];
    for (session, lines) in cases {
        fx.clock.set(at(0));
        fx.admit_hold(session);
        let p = fx.path().join(format!("{session}.jsonl"));
        write_lines(&p, &lines);
        fx.clock.set(at(60));
        assert!(
            fx.ready(session, &p),
            "{session}: a later probe cannot change this settlement, so the hook must not wait"
        );
    }
    // A session with no pending hold has nothing to settle.
    let never = fx.path().join("never.jsonl");
    assert!(fx.ready("never-admitted", &never));
    // So does one whose only hold is already settled.
    let tx = fx.path().join("done.jsonl");
    fx.admit_hold("done");
    write_lines(&tx, &[rec(10, "d1", SONNET, mi(1), Some("done"), false)]);
    fx.finalize("done", Some(&s(&tx)));
    assert!(fx.ready("done", &tx));
}

#[test]
fn a_probe_refuses_a_bad_session_and_answers_not_configured_without_a_file() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    expect_refused(
        fx.sb(SharedBudgetRequest::TranscriptReady {
            session_id: String::new(),
            transcript_path: "/x".into(),
        }),
        SharedBudgetRefusal::EmptySessionId,
    );
    let mut off = Fx::new(None, BASE);
    let before = off.fingerprint();
    assert!(matches!(
        off.sb(SharedBudgetRequest::TranscriptReady {
            session_id: "s1".into(),
            transcript_path: "/x".into(),
        }),
        SharedBudgetOutcome::NotConfigured
    ));
    assert_eq!(
        off.fingerprint(),
        before,
        "the unconfigured path writes nothing"
    );
    assert!(off.pools().is_empty());
}

#[test]
fn a_probe_writes_no_row_and_moves_no_counter() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    fx.admit_hold("s1");
    fx.admit_hold("s2");
    fx.admit_hold("s3");
    let mismatch = fx.path().join("s2.jsonl");
    write_lines(
        &mismatch,
        &[rec(10, "m1", SONNET, mi(1), Some("elsewhere"), false)],
    );
    let unpriced = fx.path().join("s3.jsonl");
    write_lines(
        &unpriced,
        &[rec(10, "m1", "no-such-model", mi(1), Some("s3"), false)],
    );
    let before = fx.fingerprint();
    let counters = |st: &SharedBudgetStatus| {
        (
            st.unmeasured_settlements,
            st.transcript_binding_mismatches,
            st.transcript_unverified,
            st.unpriced_models.clone(),
            st.foreign_lines,
            st.id_less_records,
        )
    };
    let st_before = counters(&fx.status());
    for _ in 0..3 {
        assert!(!fx.ready("s1", &fx.path().join("s1.jsonl")));
        assert!(fx.ready("s2", &mismatch));
        assert!(fx.ready("s3", &unpriced));
    }
    assert_eq!(fx.fingerprint(), before, "a poll never touches the ledger");
    assert_eq!(counters(&fx.status()), st_before, "nor the telemetry");
}

#[test]
fn a_late_flush_is_measured_once_and_the_next_turn_does_not_recount_it() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let tx = fx.path().join("s1.jsonl");
    fx.admit_hold("s1");
    // Stop is dispatched; the transcript is not written yet.
    fx.clock.set(at(60));
    assert!(!fx.ready("s1", &tx));
    assert_eq!(
        totals(&fx.status()),
        (0, 1_000),
        "nothing charged while waiting"
    );
    // The host flushes (lines stamped BEFORE the Stop, as in production).
    write_lines(&tx, &[rec(50, "m1", SONNET, mi(1), Some("s1"), false)]);
    assert!(fx.ready("s1", &tx));
    fx.finalize("s1", Some(&s(&tx)));
    assert_eq!(
        totals(&fx.status()),
        (300, 0),
        "the measured 300, not the 1000 hold"
    );
    assert_eq!(fx.lane("s1")[0].usage_known, Some(true));
    assert_eq!(fx.status().unmeasured_settlements, 0);

    // A replayed Stop (hook retry, duplicate) settles nothing.
    let settled = fx.fingerprint();
    fx.finalize("s1", Some(&s(&tx)));
    fx.finalize("s1", Some(&s(&tx)));
    assert_eq!(fx.fingerprint(), settled);

    // The next turn counts only its own line.
    fx.clock.set(at(100));
    fx.admit_hold("s1");
    assert!(!fx.ready("s1", &tx));
    append_lines(&tx, &[rec(110, "m2", SONNET, mi(2), Some("s1"), false)]);
    fx.clock.set(at(150));
    assert!(fx.ready("s1", &tx));
    fx.finalize("s1", Some(&s(&tx)));
    assert_eq!(totals(&fx.status()), (900, 0));
    // SessionEnd afterwards has nothing left to add.
    assert!(matches!(
        fx.tail("s1", &tx),
        SharedBudgetOutcome::TailSettled {
            amount_charged_usd_cents: 0,
            ..
        }
    ));
    assert_eq!(totals(&fx.status()), (900, 0));
}

#[test]
fn a_transcript_that_never_arrives_settles_at_the_flat_hold_and_the_session_end_tail_adds_nothing()
{
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let tx = fx.path().join("s1.jsonl");
    fx.admit_hold("s1");
    fx.clock.set(at(60));
    assert!(!fx.ready("s1", &tx));
    // The hook's deadline passes: Finalize is sent anyway and never fails.
    fx.finalize("s1", Some(&s(&tx)));
    assert_eq!(
        totals(&fx.status()),
        (1_000, 0),
        "the hold amount, an upper bound"
    );
    assert_eq!(fx.lane("s1")[0].usage_known, Some(false));
    assert_eq!(fx.status().unmeasured_settlements, 1);

    // The host flushes after the fact, stamped before the settle instant.
    write_lines(&tx, &[rec(50, "m1", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(70));
    // SettleTail starts at the flat settlement's instant: it recovers
    // nothing (and must not charge on top of the hold).
    assert!(matches!(
        fx.tail("s1", &tx),
        SharedBudgetOutcome::TailSettled {
            amount_charged_usd_cents: 0,
            ..
        }
    ));
    assert_eq!(
        totals(&fx.status()),
        (1_000, 0),
        "residual over-count, never a double charge"
    );
}

#[test]
fn a_hook_killed_while_waiting_leaves_the_hold_and_the_next_stop_measures_it_once() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let tx = fx.path().join("s1.jsonl");
    fx.admit_hold("s1");
    fx.clock.set(at(60));
    assert!(!fx.ready("s1", &tx));
    // The host kills the hook: no Finalize. Nothing was charged or released.
    assert_eq!(totals(&fx.status()), (0, 1_000));
    // The file lands later; the user sends another prompt (renewing the
    // lane's unexpired hold) and that turn stops normally.
    write_lines(&tx, &[rec(50, "m1", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(100));
    fx.admit_hold("s1");
    append_lines(&tx, &[rec(110, "m2", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(150));
    fx.finalize("s1", Some(&s(&tx)));
    assert_eq!(
        totals(&fx.status()),
        (600, 0),
        "both turns' lines are measured once; the killed turn costs its real spend"
    );
    assert_eq!(fx.lane("s1").len(), 1, "one lane hold, settled once");
}

#[test]
fn waiting_sessions_do_not_disturb_each_other() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let (a, b) = (fx.path().join("a.jsonl"), fx.path().join("b.jsonl"));
    fx.admit_hold("a");
    fx.admit_hold("b");
    fx.clock.set(at(60));
    assert!(!fx.ready("a", &a) && !fx.ready("b", &b));
    // b flushes first and settles while a is still waiting.
    write_lines(&b, &[rec(50, "mb", SONNET, mi(2), Some("b"), false)]);
    assert!(fx.ready("b", &b) && !fx.ready("a", &a));
    fx.finalize("b", Some(&s(&b)));
    assert_eq!(totals(&fx.status()), (600, 1_000), "a's hold is untouched");
    write_lines(&a, &[rec(55, "ma", SONNET, mi(1), Some("a"), false)]);
    assert!(fx.ready("a", &a));
    fx.finalize("a", Some(&s(&a)));
    assert_eq!(totals(&fx.status()), (900, 0));
}

// ---------------------------------------------------------------------
// M1 (HORO-2250): a duplicate Stop must not consume the next turn's hold
// ---------------------------------------------------------------------

/// Turn 1 (prompt `p1`) settles at 300c; turn 2 (prompt `p2`) is admitted.
fn after_turn_one_and_the_next_admit() -> (Fx, std::path::PathBuf) {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let tx = fx.path().join("s1.jsonl");
    grant(fx.admit("s1", None, Some("p1")));
    write_lines(&tx, &[rec(10, "m1", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(60));
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p1"));
    assert_eq!(totals(&fx.status()), (300, 0));
    fx.clock.set(at(100));
    grant(fx.admit("s1", None, Some("p2")));
    assert_eq!(totals(&fx.status()), (300, 1_000));
    (fx, tx)
}

#[test]
fn a_duplicate_stop_after_the_next_admit_does_not_settle_the_new_hold() {
    let (mut fx, tx) = after_turn_one_and_the_next_admit();
    fx.clock.set(at(110));
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p1"));
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p1"));
    assert_eq!(
        totals(&fx.status()),
        (300, 1_000),
        "turn 2's hold is still held and nothing is booked for a turn that has not happened"
    );
    let lane = fx.lane("s1");
    assert_eq!(lane.len(), 2);
    assert_eq!(
        lane[1].state,
        libra_governor_domain::ReservationState::Active
    );

    // The real Stop for turn 2 still settles it, measured, once.
    append_lines(&tx, &[rec(120, "m2", SONNET, mi(2), Some("s1"), false)]);
    fx.clock.set(at(150));
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p2"));
    assert_eq!(totals(&fx.status()), (900, 0), "300 + 600, each once");
    // ...and a late duplicate of either Stop changes nothing.
    let before = fx.fingerprint();
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p1"));
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p2"));
    assert_eq!(fx.fingerprint(), before);
}

#[test]
fn a_stop_for_a_re_prompted_turn_still_settles_the_renewed_hold() {
    // Interrupt, then re-prompt: the second Admit renews hold H1 (its key
    // still names p1) and the Stop names p2. That is not a replay.
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let tx = fx.path().join("s1.jsonl");
    grant(fx.admit("s1", None, Some("p1")));
    fx.clock.set(at(5));
    grant(fx.admit("s1", None, Some("p2")));
    write_lines(&tx, &[rec(10, "m1", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(60));
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p2"));
    assert_eq!(totals(&fx.status()), (300, 0));
}

/// Cap 2500, hold 1000: s2's turn settles at 3000 while s1's interrupted
/// turn still holds 1000, leaving the pool at 2500 - 3000 - 1000 = -1500.
fn overdrawn_pool_with_s1_holding_an_interrupted_turn() -> Fx {
    let mut fx = Fx::new(Some(&cfg_json(2_500, "UTC", 1_000)), BASE);
    let tx = fx.path().join("s2.jsonl");
    grant(fx.admit("s1", None, Some("p1")));
    grant(fx.admit("s2", None, Some("q1")));
    write_lines(&tx, &[rec(10, "m1", SONNET, mi(10), Some("s2"), false)]);
    fx.clock.set(at(60));
    fx.finalize_for_prompt("s2", Some(&s(&tx)), Some("q1"));
    assert_eq!(totals(&fx.status()), (3_000, 1_000));
    fx
}

#[test]
fn a_re_prompt_cannot_renew_a_hold_while_the_pool_is_overdrawn() {
    // Stop does not fire on interrupt, so s1's hold is still live. Renewing
    // it for a NEW prompt starts another turn with no capacity check, which
    // a fresh reservation (needs remaining >= hold) would have refused.
    let mut fx = overdrawn_pool_with_s1_holding_an_interrupted_turn();
    fx.clock.set(at(70));
    expect_cap_exhausted(fx.admit("s1", None, Some("p2")), -1_500);
    assert_eq!(totals(&fx.status()), (3_000, 1_000), "nothing was written");
}

#[test]
fn a_retry_of_the_prompt_a_live_hold_already_accepted_stays_allowed_when_overdrawn() {
    // Idempotent: the same prompt id is not a new turn.
    let mut fx = overdrawn_pool_with_s1_holding_an_interrupted_turn();
    fx.clock.set(at(70));
    grant(fx.admit("s1", None, Some("p1")));
}

#[test]
fn a_duplicate_stop_without_a_prompt_id_cannot_be_told_from_a_real_one() {
    // Residual, pinned so it is not mistaken for a guarantee: with no
    // prompt id an empty-window Stop is the flush-lag case, and the pending
    // hold settles at its flat amount.
    let (mut fx, tx) = after_turn_one_and_the_next_admit();
    fx.clock.set(at(110));
    fx.finalize("s1", Some(&s(&tx)));
    assert_eq!(totals(&fx.status()), (1_300, 0));
}

// ---------------------------------------------------------------------
// M3 (HORO-2250): a charge older than the retention horizon is replay
// ---------------------------------------------------------------------

impl Fx {
    /// `shared-budget prune --apply --keep-months 1`, as the CLI does it.
    fn prune_keeping_one_month(&mut self) -> libra_governor_ledger::PruneReport {
        use libra_governor_daemon::shared_budget::Clock as _;
        let zone = libra_governor_domain::IanaTimeZone::new("UTC").unwrap();
        self.ledger
            .prune_closed_periods(libra_governor_ledger::PruneRequest {
                keep_months: 1,
                now: self.clock.now(),
                time_zone: &zone,
                dry_run: false,
            })
            .unwrap()
    }
}

/// A session with a charged subagent file in January, whose January pool is
/// then pruned while the session lives on into May.
fn january_subagent_charged_then_pruned() -> (Fx, std::path::PathBuf, std::path::PathBuf) {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let sid = "s1";
    let main = fx.path().join("s1.jsonl");
    let dir = fx.path().join(sid).join("subagents");
    std::fs::create_dir_all(&dir).unwrap();
    let sub = dir.join("agent-aaa1.jsonl");
    fx.admit_hold(sid);
    write_lines(&main, &[rec(10, "m1", SONNET, mi(1), Some(sid), false)]);
    write_lines(&sub, &[sub_rec(12, "s1a", mi(1), sid, "aaa1")]);
    fx.clock.set(at(60));
    fx.finalize(sid, Some(&s(&main)));
    assert_eq!(fx.status().used_usd_cents, 600, "300 parent + 300 subagent");
    assert_eq!(fx.pools().len(), 1);

    // The session is resumed in May; the January pool is outside the window.
    fx.clock.set(t("2027-05-10T12:00:00Z"));
    fx.admit_hold(sid);
    let report = fx.prune_keeping_one_month();
    assert_eq!(report.pools_pruned, 1, "{report:?}");
    assert_eq!(fx.pools().len(), 1, "only the May pool remains");
    (fx, main, sub)
}

#[test]
fn a_stop_sweep_does_not_recharge_a_subagent_file_whose_period_was_pruned() {
    let (mut fx, main, sub) = january_subagent_charged_then_pruned();
    // A daemon restart forgets which files it already swept; the same
    // unchanged file is visited again at the next Stop.
    let body = std::fs::read_to_string(&sub).unwrap();
    std::fs::write(&sub, body).unwrap();
    fx.clock.set(t("2027-05-10T12:05:00Z"));
    fx.finalize("s1", Some(&s(&main)));
    assert_eq!(
        fx.pools().len(),
        1,
        "the pruned January pool was not recreated: {:?}",
        fx.pools()
    );
    assert!(
        fx.status()
            .executors
            .iter()
            .all(|e| e.agent_id.as_deref() != Some("aaa1")),
        "the May pool carries no replayed January subagent spend"
    );
}

#[test]
fn a_charge_agent_replay_into_a_pruned_period_is_refused() {
    let (mut fx, _main, _sub) = january_subagent_charged_then_pruned();
    let outcome = fx.sb(SharedBudgetRequest::ChargeAgent {
        session_id: "s1".into(),
        agent_id: Some("aaa1".into()),
        amount_usd_cents: 300,
        idempotency_key: "k1".into(),
        charge_source: libra_governor_domain::ChargeSource::SubagentTranscript,
        event_time: Some("2027-01-15T12:00:12Z".into()),
    });
    expect_refused(outcome, SharedBudgetRefusal::EventBeforeRetention);
    assert_eq!(fx.pools().len(), 1);
}

fn charge_agent_at(fx: &mut Fx, key: &str, event: &str) -> SharedBudgetOutcome {
    fx.sb(SharedBudgetRequest::ChargeAgent {
        session_id: "s1".into(),
        agent_id: Some("aaa1".into()),
        amount_usd_cents: 300,
        idempotency_key: key.into(),
        charge_source: libra_governor_domain::ChargeSource::SubagentTranscript,
        event_time: Some(event.into()),
    })
}

#[test]
fn pruning_every_pool_does_not_reopen_the_guard() {
    // Prune with NO later admit: the January pool is the only one and goes.
    let mut fx = Fx::new(Some(&cfg()), BASE);
    grant(fx.admit("s1", None, Some("p1")));
    fx.clock.set(at(60));
    fx.finalize_for_prompt("s1", None, Some("p1"));
    fx.clock.set(t("2027-05-10T12:00:00Z"));
    let report = fx.prune_keeping_one_month();
    assert_eq!(report.pools_pruned, 1, "{report:?}");
    assert!(fx.pools().is_empty(), "no pool remains");

    expect_refused(
        charge_agent_at(&mut fx, "k1", "2027-01-15T12:00:12Z"),
        SharedBudgetRefusal::EventBeforeRetention,
    );
    assert!(fx.pools().is_empty(), "January was not recreated");
    // The current period still works with no pool at all.
    let ok = charge_agent_at(&mut fx, "k2", "2027-05-10T11:00:00Z");
    assert!(matches!(ok, SharedBudgetOutcome::Charged { .. }), "{ok:?}");
    assert_eq!(fx.pools().len(), 1);
}

#[test]
fn events_at_or_before_the_prune_horizon_are_refused_and_later_ones_book_to_their_own_period() {
    let (mut fx, _main, _sub) = january_subagent_charged_then_pruned();
    // January is the horizon; May is the only pool. April was never booked.
    let before = fx.fingerprint();
    for event in ["2027-01-31T23:00:00Z", "2026-12-31T23:00:00Z"] {
        expect_refused(
            charge_agent_at(&mut fx, "k1", event),
            SharedBudgetRefusal::EventBeforeRetention,
        );
    }
    assert_eq!(fx.fingerprint(), before, "refusals write nothing");

    // April is after the horizon: real spend, booked to April's own pool.
    let ok = charge_agent_at(&mut fx, "k2", "2027-04-30T23:00:00Z");
    match ok {
        SharedBudgetOutcome::Charged {
            pool_id,
            already_charged: false,
            event_time_clamped: false,
            ..
        } => assert!(pool_id.ends_with("monthly-2027-04"), "{pool_id}"),
        other => panic!("{other:?}"),
    }
    // The current period works, and a retry resolves to the existing row.
    let ok = charge_agent_at(&mut fx, "k3", "2027-05-01T00:00:00Z");
    assert!(matches!(ok, SharedBudgetOutcome::Charged { .. }), "{ok:?}");
    let again = charge_agent_at(&mut fx, "k2", "2027-04-30T23:00:00Z");
    assert!(
        matches!(
            again,
            SharedBudgetOutcome::Charged {
                already_charged: true,
                ..
            }
        ),
        "{again:?}"
    );
}

#[test]
fn on_a_never_pruned_ledger_an_event_before_the_first_pool_books_to_its_own_period() {
    let mut fx = Fx::new(Some(&cfg()), "2027-10-05T12:00:00Z");
    grant(fx.admit("s1", None, Some("p1")));
    assert_eq!(fx.pools().len(), 1, "October is the first pool");
    match charge_agent_at(&mut fx, "k1", "2027-09-30T22:00:00Z") {
        SharedBudgetOutcome::Charged { pool_id, .. } => {
            assert!(pool_id.ends_with("monthly-2027-09"), "{pool_id}")
        }
        other => panic!("{other:?}"),
    }
}

fn subagent_used(fx: &mut Fx) -> u64 {
    fx.status()
        .executors
        .iter()
        .filter(|e| e.agent_id.as_deref() == Some("aaa1"))
        .map(|e| e.used_usd_cents)
        .sum()
}

#[test]
fn a_grown_subagent_file_after_a_prune_books_exactly_the_new_delta() {
    // The ADR-0026 example: January charged 300c, January pruned. The file
    // grows to a cumulative 600c in May: only the 300c delta is booked, not
    // 600c (the setup asserts the 300c subagent share inside the 600c total).
    let (mut fx, main, sub) = january_subagent_charged_then_pruned();
    let may_secs = (t("2027-05-10T12:01:00Z") - t(BASE)).whole_seconds();
    append_lines(&sub, &[sub_rec(may_secs, "s1b", mi(1), "s1", "aaa1")]);
    fx.clock.set(t("2027-05-10T12:05:00Z"));
    fx.finalize("s1", Some(&s(&main)));
    assert_eq!(subagent_used(&mut fx), 300, "May books the delta only");
    // And the same file swept again books nothing more.
    let body = std::fs::read_to_string(&sub).unwrap();
    std::fs::write(&sub, body).unwrap();
    fx.finalize("s1", Some(&s(&main)));
    assert_eq!(subagent_used(&mut fx), 300);
}

#[test]
fn replaying_the_pruned_subagent_total_books_zero() {
    let (mut fx, main, sub) = january_subagent_charged_then_pruned();
    let body = std::fs::read_to_string(&sub).unwrap();
    std::fs::write(&sub, body).unwrap();
    fx.clock.set(t("2027-05-10T12:05:00Z"));
    fx.finalize("s1", Some(&s(&main)));
    // Also the explicit SubagentStop path.
    let c = transcribed(fx.transcript_charge("s1", Some("aaa1"), &s(&sub)));
    assert_eq!((c.charged, c.already), (0, true), "{}", c.cumulative);
    assert_eq!(subagent_used(&mut fx), 0, "nothing was re-booked");
    assert_eq!(fx.pools().len(), 1);
}

// ---------------------------------------------------------------------
// HORO-2250 follow-up: a RENEWED prompt's Stop replayed after the next Admit
// ---------------------------------------------------------------------

impl Fx {
    /// A daemon restart: the ledger file is reopened, every in-memory fact lost.
    fn restart(&mut self) {
        self.ledger = LedgerStore::open(&self.config.ledger_path).unwrap();
    }
}

fn state_of(
    h: &libra_governor_domain::SharedPoolReservation,
) -> libra_governor_domain::ReservationState {
    h.state
}

/// p1 admits H1, a re-prompt p2 renews it, `Stop(p2)` settles it at 300c.
fn renewed_then_settled(restart: bool) -> (Fx, std::path::PathBuf) {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let tx = fx.path().join("s1.jsonl");
    grant(fx.admit("s1", None, Some("p1")));
    fx.clock.set(at(5));
    grant(fx.admit("s1", None, Some("p2")));
    assert_eq!(fx.lane("s1").len(), 1, "p2 renewed H1");
    write_lines(&tx, &[rec(10, "m1", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(60));
    if restart {
        fx.restart();
    }
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p2"));
    assert_eq!(totals(&fx.status()), (300, 0));
    (fx, tx)
}

fn assert_h3_untouched(fx: &mut Fx) {
    let lane = fx.lane("s1");
    assert_eq!(lane.len(), 2, "H1 and H3 only");
    assert_eq!(
        state_of(&lane[0]),
        libra_governor_domain::ReservationState::Settled
    );
    assert_eq!(lane[0].settled_amount, Some(300));
    assert_eq!(
        state_of(&lane[1]),
        libra_governor_domain::ReservationState::Active
    );
    assert_eq!(lane[1].settled_amount, None, "H3 was never settled");
    assert_eq!(totals(&fx.status()), (300, 1_000));
}

#[test]
fn a_replayed_stop_of_a_renewed_prompt_does_not_settle_the_next_hold() {
    let (mut fx, tx) = renewed_then_settled(false);
    fx.clock.set(at(100));
    grant(fx.admit("s1", None, Some("p3")));
    let before = fx.fingerprint();
    fx.clock.set(at(110));
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p2"));
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p2"));
    assert_h3_untouched(&mut fx);
    assert_eq!(fx.fingerprint(), before, "the replays wrote nothing");
}

#[test]
fn after_a_replay_the_real_stop_for_the_next_prompt_settles_exactly_once() {
    let (mut fx, tx) = renewed_then_settled(false);
    fx.clock.set(at(100));
    grant(fx.admit("s1", None, Some("p3")));
    fx.clock.set(at(110));
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p2"));
    append_lines(&tx, &[rec(120, "m2", SONNET, mi(2), Some("s1"), false)]);
    fx.clock.set(at(150));
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p3"));
    assert_eq!(totals(&fx.status()), (900, 0), "300 + 600, each once");
    let before = fx.fingerprint();
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p3"));
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p2"));
    assert_eq!(fx.fingerprint(), before);
}

#[test]
fn the_renewed_prompt_identity_survives_a_daemon_restart() {
    let (mut fx, tx) = renewed_then_settled(true);
    fx.clock.set(at(100));
    grant(fx.admit("s1", None, Some("p3")));
    fx.restart();
    fx.clock.set(at(110));
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p2"));
    assert_h3_untouched(&mut fx);
    fx.restart();
    append_lines(&tx, &[rec(120, "m2", SONNET, mi(2), Some("s1"), false)]);
    fx.clock.set(at(150));
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p3"));
    assert_eq!(totals(&fx.status()), (900, 0));
}

#[test]
fn every_prompt_a_hold_renewed_through_is_recognised_as_a_replay() {
    // p1 -> p2 -> p3 all land on H1; Stop(p3) settles it.
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let tx = fx.path().join("s1.jsonl");
    for (secs, p) in [(0, "p1"), (3, "p2"), (6, "p3")] {
        fx.clock.set(at(secs));
        grant(fx.admit("s1", None, Some(p)));
    }
    assert_eq!(fx.lane("s1").len(), 1);
    write_lines(&tx, &[rec(10, "m1", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(60));
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p3"));
    assert_eq!(totals(&fx.status()), (300, 0));
    fx.clock.set(at(100));
    grant(fx.admit("s1", None, Some("p4")));
    for p in ["p1", "p2", "p3"] {
        fx.clock.set(at(110));
        fx.finalize_for_prompt("s1", Some(&s(&tx)), Some(p));
        assert_h3_untouched(&mut fx);
    }
}

#[test]
fn a_renewed_prompts_own_stop_still_settles_its_active_hold() {
    // The guard must not swallow the legitimate case: Stop(p2) for the hold
    // p2 renewed, with no later hold.
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let tx = fx.path().join("s1.jsonl");
    grant(fx.admit("s1", None, Some("p1")));
    fx.clock.set(at(5));
    grant(fx.admit("s1", None, Some("p2")));
    write_lines(&tx, &[rec(10, "m1", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(60));
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p2"));
    assert_eq!(totals(&fx.status()), (300, 0));
}

#[test]
fn the_renewed_prompt_identity_survives_a_prune_of_its_settled_hold() {
    // H1 (renewed through p2) settles in January and its pool is pruned;
    // H3 is in May. Replaying Stop(p2) must not consume H3.
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let tx = fx.path().join("s1.jsonl");
    grant(fx.admit("s1", None, Some("p1")));
    fx.clock.set(at(5));
    grant(fx.admit("s1", None, Some("p2")));
    write_lines(&tx, &[rec(10, "m1", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(60));
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p2"));
    fx.clock.set(t("2027-05-10T12:00:00Z"));
    grant(fx.admit("s1", None, Some("p3")));
    let report = fx.prune_keeping_one_month();
    assert_eq!(report.pools_pruned, 1, "{report:?}");
    assert_eq!(fx.lane("s1").len(), 1, "only H3 remains");
    let before = fx.fingerprint();
    fx.clock.set(t("2027-05-10T12:05:00Z"));
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p2"));
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p1"));
    assert_eq!(
        fx.fingerprint(),
        before,
        "replays after the prune write nothing"
    );
    let lane = fx.lane("s1");
    assert_eq!(
        state_of(&lane[0]),
        libra_governor_domain::ReservationState::Active
    );
    assert_eq!(lane[0].settled_amount, None);
}

/// H1 (p1 renewed by p2) settled; H3 admitted for p3 and still active.
fn h1_settled_and_h3_active(restart: bool) -> (Fx, std::path::PathBuf) {
    let (mut fx, tx) = renewed_then_settled(false);
    fx.clock.set(at(100));
    grant(fx.admit("s1", None, Some("p3")));
    if restart {
        fx.restart();
    }
    (fx, tx)
}

#[test]
fn a_delayed_admit_of_a_settled_renewed_prompt_never_rebinds_it_to_the_next_hold() {
    // QA 29119: Admit(r1) Admit(r2) Finalize(r2) Admit(r3) Admit(r2)' Finalize(r2)'.
    let (mut fx, tx) = h1_settled_and_h3_active(false);
    let before = fx.fingerprint();
    fx.clock.set(at(105));
    expect_refused(
        fx.admit("s1", None, Some("p2")),
        SharedBudgetRefusal::KeyConflict,
    );
    assert_eq!(fx.fingerprint(), before, "the delayed Admit wrote nothing");
    fx.clock.set(at(110));
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p2"));
    assert_h3_untouched(&mut fx);
    assert_eq!(fx.fingerprint(), before, "the stale Stop wrote nothing");
}

#[test]
fn a_delayed_admit_of_the_first_prompt_of_a_renewed_hold_is_refused_too() {
    let (mut fx, tx) = h1_settled_and_h3_active(false);
    let before = fx.fingerprint();
    fx.clock.set(at(105));
    expect_refused(
        fx.admit("s1", None, Some("p1")),
        SharedBudgetRefusal::KeyConflict,
    );
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p1"));
    assert_h3_untouched(&mut fx);
    assert_eq!(fx.fingerprint(), before);
}

#[test]
fn after_a_refused_delayed_admit_the_successors_own_stop_settles_exactly_once() {
    let (mut fx, tx) = h1_settled_and_h3_active(false);
    fx.clock.set(at(105));
    expect_refused(
        fx.admit("s1", None, Some("p2")),
        SharedBudgetRefusal::KeyConflict,
    );
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p2"));
    append_lines(&tx, &[rec(120, "m2", SONNET, mi(2), Some("s1"), false)]);
    fx.clock.set(at(150));
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p3"));
    assert_eq!(totals(&fx.status()), (900, 0), "300 + 600, each once");
    let before = fx.fingerprint();
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p3"));
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p2"));
    assert_eq!(fx.fingerprint(), before);
}

#[test]
fn a_delayed_admit_is_refused_after_a_daemon_restart() {
    let (mut fx, tx) = h1_settled_and_h3_active(true);
    let before = fx.fingerprint();
    fx.clock.set(at(105));
    expect_refused(
        fx.admit("s1", None, Some("p2")),
        SharedBudgetRefusal::KeyConflict,
    );
    fx.restart();
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p2"));
    assert_h3_untouched(&mut fx);
    assert_eq!(fx.fingerprint(), before);
}

#[test]
fn a_delayed_admit_with_no_successor_hold_does_not_open_one() {
    // Nothing to rebind, but a stale Admit must not authorize unreserved work.
    let (mut fx, _tx) = renewed_then_settled(false);
    let before = fx.fingerprint();
    fx.clock.set(at(100));
    expect_refused(
        fx.admit("s1", None, Some("p2")),
        SharedBudgetRefusal::KeyConflict,
    );
    assert_eq!(fx.fingerprint(), before);
    assert_eq!(fx.lane("s1").len(), 1);
    assert_eq!(totals(&fx.status()), (300, 0));
}

#[test]
fn a_re_sent_admit_for_the_live_renewed_hold_is_still_idempotent() {
    // The accepted-prompt check must not swallow a retry while the hold is
    // still the lane's active one.
    let mut fx = Fx::new(Some(&cfg()), BASE);
    grant(fx.admit("s1", None, Some("p1")));
    fx.clock.set(at(5));
    grant(fx.admit("s1", None, Some("p2")));
    fx.clock.set(at(8));
    let h1 = fx.lane("s1")[0].id;
    assert_eq!(grant(fx.admit("s1", None, Some("p2"))).id, h1);
    assert_eq!(grant(fx.admit("s1", None, Some("p1"))).id, h1);
    assert_eq!(fx.lane("s1").len(), 1);
    assert_eq!(totals(&fx.status()), (0, 1_000));
}

#[test]
fn a_delayed_admit_after_the_hold_expired_unsettled_does_not_rebind_the_successor() {
    // H1 (p1, renewed by p2) expires with no Stop; H3 is admitted for p3.
    let mut fx = Fx::new(Some(&cfg()), BASE);
    grant(fx.admit("s1", None, Some("p1")));
    fx.clock.set(at(5));
    grant(fx.admit("s1", None, Some("p2")));
    fx.clock.set(at(100 + 24 * 3600));
    grant(fx.admit("s1", None, Some("p3")));
    let before = fx.fingerprint();
    fx.clock.set(at(110 + 24 * 3600));
    expect_refused(
        fx.admit("s1", None, Some("p2")),
        SharedBudgetRefusal::KeyConflict,
    );
    assert_eq!(fx.fingerprint(), before);
}

#[test]
fn a_delayed_admit_is_refused_after_a_prune_of_its_settled_hold() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let tx = fx.path().join("s1.jsonl");
    grant(fx.admit("s1", None, Some("p1")));
    fx.clock.set(at(5));
    grant(fx.admit("s1", None, Some("p2")));
    write_lines(&tx, &[rec(10, "m1", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(60));
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p2"));
    fx.clock.set(t("2027-05-10T12:00:00Z"));
    grant(fx.admit("s1", None, Some("p3")));
    let report = fx.prune_keeping_one_month();
    assert_eq!(report.pools_pruned, 1, "{report:?}");
    let before = fx.fingerprint();
    fx.clock.set(t("2027-05-10T12:05:00Z"));
    expect_refused(
        fx.admit("s1", None, Some("p2")),
        SharedBudgetRefusal::KeyConflict,
    );
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p2"));
    assert_eq!(fx.fingerprint(), before);
    assert_eq!(fx.lane("s1").len(), 1, "H3 only, still active");
}

#[test]
fn a_delayed_admit_in_one_session_never_touches_another_sessions_hold() {
    let (mut fx, _tx) = h1_settled_and_h3_active(false);
    grant(fx.admit("s2", None, Some("p2")));
    let before = fx.fingerprint();
    fx.clock.set(at(105));
    expect_refused(
        fx.admit("s1", None, Some("p2")),
        SharedBudgetRefusal::KeyConflict,
    );
    assert_eq!(fx.fingerprint(), before);
    assert_eq!(fx.lane("s2").len(), 1, "s2's own p2 is a different turn");
}

#[test]
fn a_delayed_admit_cannot_consume_the_cap_when_the_pool_is_full() {
    // Cap 2,000, hold 1,000: H1 settled at 300, H3 active, 700 free. A
    // delayed Admit must neither be granted nor change the books.
    let mut fx = Fx::new(Some(&cfg_json(2_000, "UTC", 1_000)), BASE);
    let tx = fx.path().join("s1.jsonl");
    grant(fx.admit("s1", None, Some("p1")));
    write_lines(&tx, &[rec(10, "m1", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(60));
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p1"));
    fx.clock.set(at(100));
    grant(fx.admit("s1", None, Some("p3")));
    let before = fx.fingerprint();
    fx.clock.set(at(105));
    expect_refused(
        fx.admit("s1", None, Some("p1")),
        SharedBudgetRefusal::KeyConflict,
    );
    assert_eq!(fx.fingerprint(), before);
    assert_eq!(totals(&fx.status()), (300, 1_000));
}

// ---------------------------------------------------------------------
// HORO-2250 review round 1: lanes, sessions, expiry, upgrade
// ---------------------------------------------------------------------

fn lane_states(fx: &Fx) -> Vec<(libra_governor_domain::ReservationState, Option<u64>)> {
    fx.lane("s1")
        .iter()
        .map(|h| (h.state, h.settled_amount))
        .collect()
}

#[test]
fn a_subagent_admit_carrying_the_parents_prompt_id_does_not_take_over_the_mapping() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let tx = fx.path().join("s1.jsonl");
    grant(fx.admit("s1", None, Some("p1")));
    // The subagent lane is admitted, then renewed, both naming the parent's p1.
    fx.clock.set(at(3));
    grant(fx.admit("s1", Some("aaa1"), Some("p1")));
    fx.clock.set(at(6));
    grant(fx.admit("s1", Some("aaa1"), Some("p1")));
    write_lines(&tx, &[rec(10, "m1", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(60));
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p1"));
    let main = fx.lane("s1");
    assert_eq!(main.len(), 1);
    assert_eq!(
        (main[0].state, main[0].settled_amount),
        (libra_governor_domain::ReservationState::Settled, Some(300)),
        "the main Stop still settles the main hold"
    );
}

#[test]
fn a_re_sent_prompt_id_after_its_hold_settled_is_refused_not_a_new_turn() {
    // A prompt id names one turn. Once its hold settled, an Admit carrying it
    // is a replay: it must not open a second hold (HORO-2250, QA 29119).
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let tx = fx.path().join("s1.jsonl");
    grant(fx.admit("s1", None, Some("p1")));
    write_lines(&tx, &[rec(10, "m1", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(60));
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p1"));
    let before = fx.fingerprint();
    fx.clock.set(at(100));
    expect_refused(
        fx.admit("s1", None, Some("p1")),
        SharedBudgetRefusal::KeyConflict,
    );
    assert_eq!(fx.fingerprint(), before, "the refused Admit wrote nothing");
    assert_eq!(fx.lane("s1").len(), 1, "no second hold");
    assert_eq!(totals(&fx.status()), (300, 0));
}

#[test]
fn a_replayed_stop_does_not_settle_an_expired_pending_hold_but_its_own_stop_does() {
    let (mut fx, tx) = after_turn_one_and_the_next_admit();
    // H2 outlives its TTL unswept; Stop(p1) is replayed.
    fx.clock.set(at(100 + 24 * 3600));
    let before = fx.fingerprint();
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p1"));
    assert_eq!(fx.fingerprint(), before, "the replay wrote nothing");
    // A late real Stop for p2 still settles that hold in its own row.
    append_lines(&tx, &[rec(120, "m2", SONNET, mi(2), Some("s1"), false)]);
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p2"));
    assert_eq!(fx.lane("s1")[1].settled_amount, Some(600));
}

#[test]
fn the_same_prompt_id_in_two_sessions_never_interacts() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let t1 = fx.path().join("s1.jsonl");
    let t2 = fx.path().join("s2.jsonl");
    grant(fx.admit("s1", None, Some("p1")));
    write_lines(&t1, &[rec(10, "m1", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(60));
    fx.finalize_for_prompt("s1", Some(&s(&t1)), Some("p1"));
    grant(fx.admit("s2", None, Some("p1")));
    write_lines(&t2, &[rec(70, "m2", SONNET, mi(2), Some("s2"), false)]);
    // Replaying s1's Stop leaves s2's hold alone; s2's own Stop settles it.
    fx.clock.set(at(80));
    fx.finalize_for_prompt("s1", Some(&s(&t1)), Some("p1"));
    assert_eq!(totals(&fx.status()), (300, 1_000));
    fx.finalize_for_prompt("s2", Some(&s(&t2)), Some("p1"));
    assert_eq!(totals(&fx.status()), (900, 0));
}

#[test]
fn holds_admitted_before_the_prompt_table_existed_keep_the_first_prompt_guard() {
    let (mut fx, tx) = after_turn_one_and_the_next_admit();
    // Simulate a ledger written before migration 0024: no prompt rows.
    {
        let raw = rusqlite::Connection::open(&fx.config.ledger_path).unwrap();
        raw.execute_batch("DELETE FROM shared_hold_prompts")
            .unwrap();
    }
    fx.restart();
    fx.clock.set(at(110));
    let before = fx.fingerprint();
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p1"));
    assert_eq!(
        fx.fingerprint(),
        before,
        "the first-prompt key still catches p1"
    );
    // Without a mapping, Stop(p2) proceeds as before and settles H2.
    append_lines(&tx, &[rec(120, "m2", SONNET, mi(2), Some("s1"), false)]);
    fx.clock.set(at(150));
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p2"));
    assert_eq!(totals(&fx.status()), (900, 0));
}

#[test]
fn a_late_stop_for_an_older_unsettled_hold_does_not_settle_the_newer_pending_one() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let tx = fx.path().join("s1.jsonl");
    grant(fx.admit("s1", None, Some("p1")));
    // H1's Stop never arrives; its TTL lapses and the next Admit sweeps it.
    fx.clock.set(at(24 * 3600));
    grant(fx.admit("s1", None, Some("p2")));
    let states = lane_states(&fx);
    assert_eq!(states.len(), 2);
    assert_eq!(
        states[0].0,
        libra_governor_domain::ReservationState::Expired
    );
    write_lines(
        &tx,
        &[rec(24 * 3600 + 5, "m1", SONNET, mi(1), Some("s1"), false)],
    );
    fx.clock.set(at(24 * 3600 + 10));
    let before = fx.fingerprint();
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p1"));
    assert_eq!(fx.fingerprint(), before, "the newer hold is not consumed");
    assert_eq!(
        fx.lane("s1")[1].state,
        libra_governor_domain::ReservationState::Active
    );
}

#[test]
fn an_over_long_prompt_id_is_treated_as_absent_and_never_truncated() {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let long = "x".repeat(257);
    let tx = fx.path().join("s1.jsonl");
    grant(fx.admit("s1", None, Some(&long)));
    let rows: i64 = rusqlite::Connection::open(&fx.config.ledger_path)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM shared_hold_prompts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0, "nothing was stored for it");
    write_lines(&tx, &[rec(10, "m1", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(60));
    // With the id ignored, the Stop settles like an id-less one.
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some(&long));
    assert_eq!(totals(&fx.status()), (300, 0));
    // The longest accepted id (256 bytes) is kept.
    let edge = "y".repeat(256);
    grant(fx.admit("s1", None, Some(&edge)));
    let rows: i64 = rusqlite::Connection::open(&fx.config.ledger_path)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM shared_hold_prompts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 1);
}

// ---------------------------------------------------------------------
// HORO-2250: post-grant compensation (release + forget) is one atomic step
// ---------------------------------------------------------------------

/// SQL fault injection through a separate connection, as the ledger tests do.
fn raw_exec(fx: &Fx, sql: &str) {
    rusqlite::Connection::open(&fx.config.ledger_path)
        .unwrap()
        .execute_batch(sql)
        .unwrap();
}

/// Makes the capacity read that follows a fresh grant fail: the grant's last
/// write (the prompt row) corrupts the pool's stored capacity, so the hold is
/// committed and the `remaining` read in `allowed()` then errors.
fn inject_post_grant_read_fault(fx: &Fx) {
    raw_exec(
        fx,
        "CREATE TRIGGER fault_read AFTER INSERT ON shared_hold_prompts
         BEGIN UPDATE quota_pools SET capacity = 'corrupt'; END;",
    );
}

fn inject_forget_fault(fx: &Fx) {
    raw_exec(
        fx,
        "CREATE TRIGGER fault_forget BEFORE DELETE ON shared_hold_prompts
         BEGIN SELECT RAISE(ABORT, 'forget boom'); END;",
    );
}

fn inject_release_fault(fx: &Fx) {
    raw_exec(
        fx,
        "CREATE TRIGGER fault_release BEFORE UPDATE ON shared_pool_reservations
         WHEN NEW.state = 'released'
         BEGIN SELECT RAISE(ABORT, 'release boom'); END;",
    );
}

/// Removes every injected fault and restores the pool's configured capacity.
fn clear_faults(fx: &Fx, capacity: u64) {
    raw_exec(
        fx,
        &format!(
            "DROP TRIGGER IF EXISTS fault_read;
             DROP TRIGGER IF EXISTS fault_forget;
             DROP TRIGGER IF EXISTS fault_release;
             UPDATE quota_pools SET capacity = {capacity};"
        ),
    );
}

fn prompt_rows(fx: &Fx, session: &str) -> Vec<String> {
    let conn = rusqlite::Connection::open(&fx.config.ledger_path).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT prompt_id FROM shared_hold_prompts WHERE session_id = ?1 ORDER BY prompt_id",
        )
        .unwrap();
    let rows = stmt
        .query_map([session], |r| r.get::<_, String>(0))
        .unwrap();
    rows.map(Result::unwrap).collect()
}

fn admit_error(fx: &mut Fx, session: &str, prompt: &str) -> String {
    let response = fx.call(Request::SharedBudget {
        shared_budget: SharedBudgetRequest::Admit {
            session_id: session.to_string(),
            agent_id: None,
            prompt_id: Some(prompt.to_string()),
        },
    });
    match response {
        Response::Error { message } => message,
        other => panic!("expected an error response, got {other:?}"),
    }
}

fn daemon_log(fx: &Fx) -> String {
    std::fs::read_to_string(&fx.config.log_path).unwrap_or_default()
}

/// A settled p0 turn in s1, so a historical binding exists before the fault.
fn settled_p0() -> (Fx, std::path::PathBuf) {
    let mut fx = Fx::new(Some(&cfg()), BASE);
    let tx = fx.path().join("s1.jsonl");
    grant(fx.admit("s1", None, Some("p0")));
    write_lines(&tx, &[rec(10, "m0", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(60));
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p0"));
    assert_eq!(totals(&fx.status()), (300, 0));
    (fx, tx)
}

#[test]
fn a_failed_post_grant_read_releases_the_unstarted_hold_and_frees_the_prompt_for_a_retry() {
    let (mut fx, tx) = settled_p0();
    // Another session's equal prompt id must survive s1's compensation.
    grant(fx.admit("s2", None, Some("p1")));
    fx.clock.set(at(100));
    inject_post_grant_read_fault(&fx);
    admit_error(&mut fx, "s1", "p1");
    clear_faults(&fx, 100_000);

    let lane = fx.lane("s1");
    assert_eq!(lane.len(), 2);
    assert_eq!(
        state_of(&lane[1]),
        libra_governor_domain::ReservationState::Released
    );
    assert_eq!(
        prompt_rows(&fx, "s1"),
        vec!["p0".to_string()],
        "only p1 was forgotten"
    );
    assert_eq!(prompt_rows(&fx, "s2"), vec!["p1".to_string()]);
    assert_eq!(
        state_of(&fx.lane("s2")[0]),
        libra_governor_domain::ReservationState::Active
    );
    // Only s2's hold still reserves capacity.
    assert_eq!(totals(&fx.status()), (300, 1_000));

    // The host's retry of the same prompt is a fresh grant, not a replay.
    fx.clock.set(at(110));
    let retry = grant(fx.admit("s1", None, Some("p1")));
    assert_ne!(retry.id, lane[1].id);
    assert_eq!(totals(&fx.status()), (300, 2_000));
    // Its legitimate Stop settles exactly once.
    append_lines(&tx, &[rec(120, "m1", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(150));
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p1"));
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p1"));
    assert_eq!(totals(&fx.status()), (600, 1_000));
    // The historical p0 turn stays bound.
    expect_refused(
        fx.admit("s1", None, Some("p0")),
        SharedBudgetRefusal::KeyConflict,
    );
}

#[test]
fn a_failed_forget_rolls_the_release_back_and_keeps_hold_and_binding_together() {
    let (mut fx, tx) = settled_p0();
    fx.clock.set(at(100));
    inject_post_grant_read_fault(&fx);
    inject_forget_fault(&fx);
    admit_error(&mut fx, "s1", "p1");
    let log = daemon_log(&fx);
    clear_faults(&fx, 100_000);

    // Nothing half-applied: the hold is still active and still bound to p1.
    let lane = fx.lane("s1");
    assert_eq!(
        state_of(&lane[1]),
        libra_governor_domain::ReservationState::Active
    );
    assert_eq!(
        prompt_rows(&fx, "s1"),
        vec!["p0".to_string(), "p1".to_string()]
    );
    assert_eq!(
        totals(&fx.status()),
        (300, 1_000),
        "no capacity created or lost"
    );

    // A restart loses nothing; the retry reuses the same hold.
    fx.restart();
    fx.clock.set(at(110));
    let before = fx.fingerprint();
    match fx.admit("s1", None, Some("p1")) {
        SharedBudgetOutcome::Allowed {
            reservation_id,
            hold_reused,
            ..
        } => {
            assert_eq!(reservation_id, lane[1].id);
            assert!(hold_reused);
        }
        other => panic!("expected the same hold, got {other:?}"),
    }
    assert_eq!(fx.lane("s1").len(), 2, "no second hold");
    assert_ne!(
        fx.fingerprint(),
        before,
        "the renewal extended the expiry only"
    );
    assert_eq!(totals(&fx.status()), (300, 1_000));
    append_lines(&tx, &[rec(120, "m1", SONNET, mi(1), Some("s1"), false)]);
    fx.clock.set(at(150));
    fx.finalize_for_prompt("s1", Some(&s(&tx)), Some("p1"));
    assert_eq!(totals(&fx.status()), (600, 0), "settles once");
    expect_refused(
        fx.admit("s1", None, Some("p0")),
        SharedBudgetRefusal::KeyConflict,
    );
    // The cleanup failure is reported, not swallowed.
    assert!(
        log.contains("forget boom"),
        "cleanup error in the log: {log}"
    );
}

#[test]
fn a_failed_release_leaves_the_binding_so_the_retry_stays_idempotent() {
    let (mut fx, _tx) = settled_p0();
    fx.clock.set(at(100));
    inject_post_grant_read_fault(&fx);
    inject_release_fault(&fx);
    admit_error(&mut fx, "s1", "p1");
    let log = daemon_log(&fx);
    clear_faults(&fx, 100_000);

    let lane = fx.lane("s1");
    assert_eq!(
        state_of(&lane[1]),
        libra_governor_domain::ReservationState::Active
    );
    assert_eq!(
        prompt_rows(&fx, "s1"),
        vec!["p0".to_string(), "p1".to_string()],
        "the binding was not discarded with the failed release"
    );
    fx.clock.set(at(110));
    match fx.admit("s1", None, Some("p1")) {
        SharedBudgetOutcome::Allowed {
            reservation_id,
            hold_reused,
            ..
        } => {
            assert_eq!(reservation_id, lane[1].id);
            assert!(hold_reused);
        }
        other => panic!("expected the same hold, got {other:?}"),
    }
    assert_eq!(totals(&fx.status()), (300, 1_000));
    assert!(
        log.contains("release boom"),
        "cleanup error in the log: {log}"
    );
}

#[test]
fn an_unreleased_unstarted_hold_keeps_its_capacity_reserved_until_it_expires() {
    // Cap 1,000 = exactly one hold. A compensation that fails must not create
    // capacity; the stranded hold frees it only by expiring.
    let mut fx = Fx::new(Some(&cfg_json(1_000, "UTC", 1_000)), BASE);
    inject_post_grant_read_fault(&fx);
    inject_release_fault(&fx);
    admit_error(&mut fx, "s1", "p1");
    clear_faults(&fx, 1_000);
    expect_denied(
        fx.admit("s2", None, Some("q1")),
        SharedBudgetDenyReason::CapExhausted,
    );
    assert_eq!(totals(&fx.status()), (0, 1_000));
    fx.clock.set(at(2 * 3600));
    grant(fx.admit("s2", None, Some("q1")));
    assert_eq!(totals(&fx.status()), (0, 1_000), "never above the hard cap");
}
