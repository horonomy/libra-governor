//! HORO-2325 independent QA (QA-4): Codex orphan recovery and true-up (C4),
//! driven by an injected `ManualClock`, a real `LedgerStore` and real rollout
//! files. Written from the documented contract (ADR 0026, "Codex orphan
//! recovery and true-up"), not from the developer tests' assertions.
//!
//! Prices (independently computed): with `OVERRIDES`, BIG = 2,750,080
//! micro-dollars -> ceil(275.008) = 276 cents; SMALL = 1,785,050 -> 179.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use libra_governor_daemon::shared_budget::{
    admit, budget_lane, settle_codex_turn, status, CodexSettlement, ManualClock,
    SharedBudgetRuntime,
};
use libra_governor_daemon::shared_budget_file::SHARED_BUDGET_FILE_NAME;
use libra_governor_domain::{
    ExecutorKind, PrincipalId, ReservationState, SharedPoolReservation, SharedPoolReservationId,
    LOCAL_BUDGET_OWNER,
};
use libra_governor_ledger::LedgerStore;
use libra_governor_protocol::SharedBudgetOutcome;

const BASE: &str = "2027-01-15T12:00:00Z";
const OWNER_PREFIX: &str = "metered:local-budget/";
const POOL_JAN: &str = "metered:local-budget/monthly-2027-01";
const SESSION: &str = "qa4-sess";
const MODEL: &str = "gpt-test-1";
const HOLD: u64 = 1_000;
const TTL: i64 = 3600;
const OVERRIDES: &str = r#","pricing_overrides":{"gpt-test-1":{"input_usd_per_mtok":1000.0,
    "cache_creation_input_usd_per_mtok":1250.0,"cache_read_input_usd_per_mtok":100.0,
    "output_usd_per_mtok":25001.0}}"#;

/// `[input, cached, cache_write, output, reasoning]`
const BIG: [i64; 5] = [2000, 1500, 400, 80, 30];
const BIG_CENTS: u64 = 276;
const SMALL: [i64; 5] = [1000, 600, 300, 50, 20];
const SMALL_CENTS: u64 = 179;
/// BIG - SMALL, the record that arrives late.
const LATE: [i64; 5] = [1000, 900, 100, 30, 10];
/// 500 fresh input tokens: 500,000 micro-dollars = 50 cents on its own.
const EXTRA: [i64; 5] = [500, 0, 0, 0, 0];

fn t(s: &str) -> OffsetDateTime {
    OffsetDateTime::parse(s, &Rfc3339).unwrap()
}

fn at(offset_secs: i64) -> OffsetDateTime {
    t(BASE) + time::Duration::seconds(offset_secs)
}

fn principal() -> PrincipalId {
    PrincipalId(LOCAL_BUDGET_OWNER.to_string())
}

fn lane() -> String {
    budget_lane(SESSION, Some("codex")).unwrap()
}

fn lp(s: &str) -> String {
    format!("{}:{s}", s.len())
}

fn cfg_json(hold: u64) -> String {
    format!(
        r#"{{"schema_version":1,"monthly_cap_usd_cents":1000000,"time_zone":"UTC","turn_hold_usd_cents":{hold}{OVERRIDES}}}"#
    )
}

fn started(turn: &str) -> String {
    format!(r#"{{"type":"event_msg","payload":{{"type":"task_started","turn_id":"{turn}"}}}}"#)
}

fn complete(turn: &str) -> String {
    format!(r#"{{"type":"event_msg","payload":{{"type":"task_complete","turn_id":"{turn}"}}}}"#)
}

fn counters(u: [i64; 5]) -> String {
    format!(
        r#"{{"input_tokens":{},"cached_input_tokens":{},"cache_write_input_tokens":{},"output_tokens":{},"reasoning_output_tokens":{},"total_tokens":{}}}"#,
        u[0],
        u[1],
        u[2],
        u[3],
        u[4],
        u[0].saturating_add(u[3])
    )
}

/// One `token_usage_record` with its own `response_id`; `running` is the
/// turn's cumulative total after this record.
fn record(turn: &str, resp: &str, usage: [i64; 5], running: [i64; 5]) -> String {
    format!(
        r#"{{"type":"token_usage_record","payload":{{"turn_id":"{turn}","thread_id":"{SESSION}","session_id":"{SESSION}","root_turn_id":"{turn}","response_id":"{resp}","usage":{},"turn_token_usage":{},"thread_token_usage":{}}}}}"#,
        counters(usage),
        counters(running),
        counters(running)
    )
}

fn turn_lines(turn: &str, usage: [i64; 5]) -> Vec<String> {
    vec![
        started(turn),
        record(turn, &format!("{turn}-r1"), usage, usage),
        complete(turn),
    ]
}

struct Fx {
    dir: tempfile::TempDir,
    clock: Arc<ManualClock>,
    rt: SharedBudgetRuntime,
    ledger: Option<LedgerStore>,
}

impl Fx {
    fn new(hold: u64) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(SHARED_BUDGET_FILE_NAME), cfg_json(hold)).unwrap();
        std::fs::write(dir.path().join("shared-budget.codex"), b"").unwrap();
        let clock = Arc::new(ManualClock::new(t(BASE)));
        let rt = SharedBudgetRuntime::load(dir.path()).with_clock(clock.clone());
        let ledger = Some(LedgerStore::open(dir.path().join("ledger.sqlite3")).unwrap());
        Self {
            dir,
            clock,
            rt,
            ledger,
        }
    }

    fn restart(&mut self) {
        self.ledger = None;
        self.rt = SharedBudgetRuntime::load(self.dir.path()).with_clock(self.clock.clone());
        self.ledger = Some(LedgerStore::open(self.dir.path().join("ledger.sqlite3")).unwrap());
    }

    fn admit(&mut self, turn: &str) -> SharedPoolReservationId {
        let ledger = self.ledger.as_mut().unwrap();
        match admit(ledger, &self.rt, SESSION, None, Some(turn), Some("codex")).unwrap() {
            SharedBudgetOutcome::Allowed { reservation_id, .. } => reservation_id,
            other => panic!("expected a grant, got {other:?}"),
        }
    }

    fn settle(&mut self, turn: &str, path: &Path) -> CodexSettlement {
        let p = path.to_str().unwrap().to_string();
        let ledger = self.ledger.as_mut().unwrap();
        settle_codex_turn(ledger, &self.rt, SESSION, turn, Some(MODEL), Some(&p)).unwrap()
    }

    fn holds(&self) -> Vec<SharedPoolReservation> {
        self.ledger
            .as_ref()
            .unwrap()
            .shared_lane_holds(
                &principal(),
                &lane(),
                None,
                ExecutorKind::AgentAbsent,
                OWNER_PREFIX,
            )
            .unwrap()
    }

    fn hold(&self, id: SharedPoolReservationId) -> SharedPoolReservation {
        self.holds().into_iter().find(|h| h.id == id).unwrap()
    }

    fn status(&self) -> libra_governor_protocol::SharedBudgetStatus {
        status(self.ledger.as_ref().unwrap(), &self.rt).unwrap()
    }

    fn used_reserved(&self) -> (u64, u64) {
        let s = self.status();
        (s.used_usd_cents, s.reserved_usd_cents)
    }

    fn trueup_total(&self, hold: SharedPoolReservationId) -> u64 {
        let prefix = format!("codex-trueup:{}|{}|", lp(&lane()), hold.0);
        self.ledger
            .as_ref()
            .unwrap()
            .shared_charges_total_by_key_prefix(&principal(), &prefix, OWNER_PREFIX)
            .unwrap()
    }

    /// Every table's rows as text, read through a separate connection.
    fn fingerprint(&self) -> String {
        let conn = rusqlite::Connection::open_with_flags(
            self.dir.path().join("ledger.sqlite3"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let tables: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
                .unwrap();
            let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
            rows.map(Result::unwrap).collect()
        };
        let mut out = String::new();
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

    fn rollout(&self, name: &str, lines: &[String]) -> PathBuf {
        let path = self.dir.path().join(name);
        let mut body = lines.join("\n");
        body.push('\n');
        std::fs::write(&path, body).unwrap();
        path
    }
}

fn settled_of(o: &CodexSettlement) -> (u64, bool) {
    match o {
        CodexSettlement::Settled {
            settled_usd_cents,
            usage_known,
            ..
        } => (*settled_usd_cents, *usage_known),
        other => panic!("expected Settled, got {other:?}"),
    }
}

/// Scenario driver: A admitted at BASE and killed; B admitted `gap` seconds
/// after A's admission; returns (A's hold, B's hold).
fn kill_a_then_admit_b(
    fx: &mut Fx,
    gap: i64,
) -> (SharedPoolReservationId, SharedPoolReservationId) {
    let a = fx.admit("turn-a");
    fx.clock.set(at(gap));
    let b = fx.admit("turn-b");
    (a, b)
}

// (1) -------------------------------------------------------------------

#[test]
fn s01_killed_turn_recovered_measured_from_rollout_after_ttl() {
    let mut fx = Fx::new(HOLD);
    let (a, b) = kill_a_then_admit_b(&mut fx, TTL + 600);
    assert_ne!(a, b);
    assert_eq!(fx.hold(a).state, ReservationState::Expired);
    assert_eq!(
        fx.hold(a).settled_amount,
        None,
        "orphan not settled before the next settle"
    );
    let path = fx.rollout(
        "r.jsonl",
        &[turn_lines("turn-a", BIG), turn_lines("turn-b", SMALL)].concat(),
    );
    let out = fx.settle("turn-b", &path);
    assert_eq!(settled_of(&out), (SMALL_CENTS, true));
    let orphan = fx.hold(a);
    assert_eq!(orphan.state, ReservationState::Settled);
    assert_eq!(orphan.settled_amount, Some(BIG_CENTS));
    assert_eq!(orphan.usage_known, Some(true));
    assert_eq!(orphan.pool_id.0, POOL_JAN);
    assert_eq!(fx.used_reserved(), (BIG_CENTS + SMALL_CENTS, 0));
}

// (2) -------------------------------------------------------------------

#[test]
fn s02_killed_turn_without_usage_settles_flat_hold_unmeasured() {
    // Variant 1: the rollout has no trace of the orphan. Variant 2: the
    // orphan has start/complete markers but no usage record.
    for variant in 0..2 {
        let mut fx = Fx::new(HOLD);
        let (a, _b) = kill_a_then_admit_b(&mut fx, TTL + 600);
        let mut lines = turn_lines("turn-b", SMALL);
        if variant == 1 {
            lines = [vec![started("turn-a"), complete("turn-a")], lines].concat();
        }
        let path = fx.rollout("r.jsonl", &lines);
        let before = fx.status().unmeasured_settlements;
        let out = fx.settle("turn-b", &path);
        assert_eq!(settled_of(&out), (SMALL_CENTS, true), "variant {variant}");
        let orphan = fx.hold(a);
        assert_eq!(orphan.state, ReservationState::Settled, "variant {variant}");
        assert_eq!(
            orphan.settled_amount,
            Some(HOLD),
            "variant {variant}: flat hold, never zero"
        );
        assert_eq!(orphan.usage_known, Some(false), "variant {variant}");
        // The reason is not surfaced for an orphan; the only observable
        // signal is the unmeasured_settlements counter.
        assert_eq!(
            fx.status().unmeasured_settlements,
            before + 1,
            "variant {variant}"
        );
        assert_eq!(
            fx.used_reserved(),
            (HOLD + SMALL_CENTS, 0),
            "variant {variant}"
        );
    }
}

// (3) -------------------------------------------------------------------

/// B is admitted `TTL + delta` seconds after A. Before expiry B joins A's live
/// hold; at or after expiry A is an orphan with its own row.
fn boundary(delta: i64) -> (Fx, SharedPoolReservationId, SharedPoolReservationId) {
    let mut fx = Fx::new(HOLD);
    let (a, b) = kill_a_then_admit_b(&mut fx, TTL + delta);
    let path = fx.rollout(
        "r.jsonl",
        &[turn_lines("turn-a", BIG), turn_lines("turn-b", SMALL)].concat(),
    );
    fx.settle("turn-b", &path);
    (fx, a, b)
}

#[test]
fn s03a_one_second_before_ttl_hold_is_live_and_not_recovered_as_orphan() {
    let (fx, a, b) = boundary(-1);
    assert_eq!(a, b, "a re-prompt before expiry joins the live hold");
    let h = fx.hold(a);
    assert_eq!(h.state, ReservationState::Settled);
    // One hold covers both turns; no separate orphan row exists.
    assert_eq!(
        fx.holds()
            .iter()
            .filter(|h| h.state != ReservationState::Settled)
            .count(),
        0
    );
    assert_eq!(fx.holds().len(), 1);
    assert_eq!(
        h.settled_amount,
        Some(BIG_CENTS + SMALL_CENTS),
        "both turns, each priced once"
    );
    assert_eq!(fx.used_reserved(), (BIG_CENTS + SMALL_CENTS, 0));
}

#[test]
fn s03b_exactly_at_ttl_hold_is_expired_and_recovered() {
    let (fx, a, b) = boundary(0);
    assert_ne!(a, b);
    assert_eq!(fx.hold(a).state, ReservationState::Settled);
    assert_eq!(fx.hold(a).settled_amount, Some(BIG_CENTS));
    assert_eq!(fx.hold(b).settled_amount, Some(SMALL_CENTS));
    assert_eq!(fx.used_reserved(), (BIG_CENTS + SMALL_CENTS, 0));
}

#[test]
fn s03c_one_second_after_ttl_hold_is_expired_and_recovered() {
    let (fx, a, b) = boundary(1);
    assert_ne!(a, b);
    assert_eq!(fx.hold(a).settled_amount, Some(BIG_CENTS));
    assert_eq!(fx.hold(b).settled_amount, Some(SMALL_CENTS));
    assert_eq!(fx.used_reserved(), (BIG_CENTS + SMALL_CENTS, 0));
}

/// A hold past its TTL is only moved to `expired` by a sweep that runs in
/// `admit` (and the launch gate). A Stop-only settle with no admit in between
/// finds the hold still `active`. This pins what the daemon does so the
/// behaviour is recorded; see the report for the severity discussion.
#[test]
fn s03d_stop_only_settle_past_ttl_without_an_admit_sweep() {
    let mut fx = Fx::new(HOLD);
    let a = fx.admit("turn-a");
    // Unadmitted turn X (no mapping) settles at TTL - 1 s and at TTL + 1 s.
    let path = fx.rollout(
        "r.jsonl",
        &[turn_lines("turn-a", BIG), turn_lines("turn-x", SMALL)].concat(),
    );
    fx.clock.set(at(TTL - 1));
    fx.settle("turn-x", &path);
    assert_eq!(
        fx.hold(a).state,
        ReservationState::Active,
        "not recovered before expiry"
    );
    fx.clock.set(at(TTL + 1));
    fx.settle("turn-x", &path);
    let state_after = fx.hold(a).state;
    // Record observed behaviour for the report.
    eprintln!("S03D observed: orphan state after post-TTL Stop-only settle = {state_after:?}");
    assert!(matches!(
        state_after,
        ReservationState::Active | ReservationState::Settled
    ));
}

// (4) -------------------------------------------------------------------

#[test]
fn s04_more_than_sixteen_orphans_cap_respected_rest_on_later_settles_no_double_booking() {
    const ORPHANS: usize = 20;
    let mut fx = Fx::new(HOLD);
    let mut ids = Vec::new();
    let mut lines = Vec::new();
    for i in 0..ORPHANS {
        // Each admit lands after the previous hold's TTL, so none joins another.
        fx.clock.set(at(i as i64 * (TTL + 100)));
        let turn = format!("orphan-{i}");
        ids.push(fx.admit(&turn));
        lines.extend(turn_lines(&turn, BIG));
    }
    fx.clock.set(at(ORPHANS as i64 * (TTL + 100)));
    let b = fx.admit("turn-b");
    lines.extend(turn_lines("turn-b", SMALL));
    let path = fx.rollout("r.jsonl", &lines);
    let settled_orphans = |fx: &Fx| {
        ids.iter()
            .filter(|id| fx.hold(**id).state == ReservationState::Settled)
            .count()
    };
    assert_eq!(
        ids.iter()
            .filter(|id| fx.hold(**id).state == ReservationState::Expired)
            .count(),
        ORPHANS
    );

    fx.settle("turn-b", &path);
    assert_eq!(settled_orphans(&fx), 16, "cap of 16 per settle");
    assert_eq!(fx.hold(b).settled_amount, Some(SMALL_CENTS));
    assert_eq!(fx.used_reserved(), (16 * BIG_CENTS + SMALL_CENTS, 0));

    // Replay of the same Stop: remaining 4 are recovered, nothing else moves.
    let out = fx.settle("turn-b", &path);
    assert!(matches!(out, CodexSettlement::AlreadySettled));
    assert_eq!(settled_orphans(&fx), ORPHANS);
    let expected = ORPHANS as u64 * BIG_CENTS + SMALL_CENTS;
    assert_eq!(fx.used_reserved(), (expected, 0));
    for id in &ids {
        assert_eq!(
            fx.hold(*id).settled_amount,
            Some(BIG_CENTS),
            "each exactly once"
        );
    }
    let before = fx.fingerprint();
    fx.settle("turn-b", &path);
    fx.restart();
    fx.settle("turn-b", &path);
    assert_eq!(fx.fingerprint(), before, "nothing double-booked");
}

// (5) -------------------------------------------------------------------

#[test]
fn s05_replayed_stop_and_repeated_recovery_never_book_twice() {
    let mut fx = Fx::new(HOLD);
    let (a, b) = kill_a_then_admit_b(&mut fx, TTL + 600);
    let path = fx.rollout(
        "r.jsonl",
        &[turn_lines("turn-a", BIG), turn_lines("turn-b", SMALL)].concat(),
    );
    fx.settle("turn-b", &path);
    let before = fx.fingerprint();
    let totals = fx.used_reserved();
    for _ in 0..3 {
        let again = fx.settle("turn-b", &path);
        assert!(matches!(again, CodexSettlement::AlreadySettled));
    }
    // A Stop for the orphan's own id (host finally delivers it late).
    let late = fx.settle("turn-a", &path);
    assert!(matches!(late, CodexSettlement::AlreadySettled));
    assert_eq!(fx.fingerprint(), before);
    assert_eq!(fx.used_reserved(), totals);
    assert_eq!(fx.hold(a).settled_amount, Some(BIG_CENTS));
    assert_eq!(fx.hold(b).settled_amount, Some(SMALL_CENTS));
}

// (6) -------------------------------------------------------------------

#[test]
fn s06_restart_between_orphan_creation_and_recovery() {
    // Restart 1: between the orphan's admit and the next admit.
    // Restart 2: between the sweep (next admit) and the settle that recovers.
    let mut fx = Fx::new(HOLD);
    let a = fx.admit("turn-a");
    fx.restart();
    fx.clock.set(at(TTL + 600));
    fx.restart();
    let _b = fx.admit("turn-b");
    assert_eq!(fx.hold(a).state, ReservationState::Expired);
    fx.restart();
    let path = fx.rollout(
        "r.jsonl",
        &[turn_lines("turn-a", BIG), turn_lines("turn-b", SMALL)].concat(),
    );
    fx.settle("turn-b", &path);
    assert_eq!(fx.hold(a).settled_amount, Some(BIG_CENTS));
    assert_eq!(fx.hold(a).usage_known, Some(true));
    assert_eq!(fx.used_reserved(), (BIG_CENTS + SMALL_CENTS, 0));
    let before = fx.fingerprint();
    fx.restart();
    fx.settle("turn-b", &path);
    assert_eq!(fx.fingerprint(), before);
}

// (7) -------------------------------------------------------------------

#[test]
fn s07_month_rollover_between_admit_and_recovery_books_into_the_admit_pool() {
    let mut fx = Fx::new(HOLD);
    fx.clock.set(t("2027-01-31T23:30:00Z"));
    let a = fx.admit("turn-a");
    fx.clock.set(t("2027-02-01T02:00:00Z"));
    let b = fx.admit("turn-b");
    let path = fx.rollout(
        "r.jsonl",
        &[turn_lines("turn-a", BIG), turn_lines("turn-b", SMALL)].concat(),
    );
    fx.settle("turn-b", &path);
    let (ha, hb) = (fx.hold(a), fx.hold(b));
    assert_eq!(ha.pool_id.0, POOL_JAN);
    assert_eq!(hb.pool_id.0, "metered:local-budget/monthly-2027-02");
    assert_eq!(ha.settled_amount, Some(BIG_CENTS));
    assert_eq!(hb.settled_amount, Some(SMALL_CENTS));
    // Feb pool carries only B; Jan pool carries only A.
    assert_eq!(fx.used_reserved(), (SMALL_CENTS, 0));
    fx.clock.set(t("2027-01-31T23:59:00Z"));
    assert_eq!(fx.used_reserved(), (BIG_CENTS, 0));
}

// (8) -------------------------------------------------------------------

#[test]
fn s08_recovered_measured_turn_above_hold_records_full_overrun() {
    let mut fx = Fx::new(100);
    let (a, _b) = kill_a_then_admit_b(&mut fx, TTL + 600);
    let path = fx.rollout(
        "r.jsonl",
        &[turn_lines("turn-a", BIG), turn_lines("turn-b", SMALL)].concat(),
    );
    fx.settle("turn-b", &path);
    let h = fx.hold(a);
    assert_eq!(h.amount, 100);
    assert_eq!(
        h.settled_amount,
        Some(BIG_CENTS),
        "no clipping to the 100-cent hold"
    );
    assert_eq!(h.usage_known, Some(true));
    assert_eq!(fx.used_reserved(), (BIG_CENTS + SMALL_CENTS, 0));
}

// (9) -------------------------------------------------------------------

fn grown(turn: &str, extra: bool) -> Vec<String> {
    let mut v = vec![
        started(turn),
        record(turn, "r1", SMALL, SMALL),
        record(turn, "r2", LATE, BIG),
    ];
    if extra {
        let total = [BIG[0] + EXTRA[0], BIG[1], BIG[2], BIG[3], BIG[4]];
        v.push(record(turn, "r3", EXTRA, total));
    }
    v.push(complete(turn));
    v
}

#[test]
fn s09a_measured_short_hold_is_trued_up_once_upward_and_idempotently() {
    let mut fx = Fx::new(HOLD);
    let h = fx.admit("turn-a");
    let path = fx.rollout(
        "r.jsonl",
        &[
            started("turn-a"),
            record("turn-a", "r1", SMALL, SMALL),
            complete("turn-a"),
        ],
    );
    assert_eq!(settled_of(&fx.settle("turn-a", &path)), (SMALL_CENTS, true));
    fx.rollout("r.jsonl", &grown("turn-a", false));
    fx.clock.set(at(600));
    fx.settle("turn-a", &path);
    assert_eq!(fx.trueup_total(h), BIG_CENTS - SMALL_CENTS);
    assert_eq!(
        fx.hold(h).settled_amount,
        Some(SMALL_CENTS),
        "hold row untouched"
    );
    assert_eq!(fx.used_reserved(), (BIG_CENTS, 0));
    let before = fx.fingerprint();
    fx.settle("turn-a", &path);
    fx.restart();
    fx.settle("turn-a", &path);
    assert_eq!(
        fx.fingerprint(),
        before,
        "idempotent across replay and restart"
    );
    // Downward: a smaller measurement never refunds.
    fx.rollout(
        "r.jsonl",
        &[
            started("turn-a"),
            record("turn-a", "r1", SMALL, SMALL),
            complete("turn-a"),
        ],
    );
    fx.settle("turn-a", &path);
    assert_eq!(fx.fingerprint(), before, "never a refund");
    // Further growth books only the further delta.
    fx.rollout("r.jsonl", &grown("turn-a", true));
    fx.settle("turn-a", &path);
    assert_eq!(
        fx.trueup_total(h),
        326 - SMALL_CENTS,
        "cumulative delta only"
    );
    assert_eq!(fx.used_reserved(), (326, 0));
}

#[test]
fn s09b_incomplete_turn_is_not_trued_up_until_task_complete() {
    let mut fx = Fx::new(HOLD);
    let h = fx.admit("turn-a");
    let path = fx.rollout(
        "r.jsonl",
        &[
            started("turn-a"),
            record("turn-a", "r1", SMALL, SMALL),
            complete("turn-a"),
        ],
    );
    fx.settle("turn-a", &path);
    let mut lines = grown("turn-a", false);
    lines.pop(); // no task_complete yet
    fx.rollout("r.jsonl", &lines);
    fx.settle("turn-a", &path);
    assert_eq!(fx.trueup_total(h), 0);
    fx.rollout("r.jsonl", &grown("turn-a", false));
    fx.settle("turn-a", &path);
    assert_eq!(fx.trueup_total(h), BIG_CENTS - SMALL_CENTS);
}

/// ADR: a hold settled at the flat amount (usage_known=false) is never trued
/// up, even if its rollout later gains usage. Covers a flat direct settle and
/// a flat orphan recovery.
#[test]
fn s09c_flat_settled_hold_is_never_trued_up_even_when_rollout_later_gains_usage() {
    let mut fx = Fx::new(HOLD);
    let (a, b) = kill_a_then_admit_b(&mut fx, TTL + 600);
    // First settle: turn-a absent from the rollout -> flat orphan.
    let path = fx.rollout("r.jsonl", &turn_lines("turn-b", SMALL));
    fx.settle("turn-b", &path);
    assert_eq!(fx.hold(a).usage_known, Some(false));
    assert_eq!(fx.hold(a).settled_amount, Some(HOLD));
    // The orphan's records now appear.
    fx.rollout(
        "r.jsonl",
        &[turn_lines("turn-a", BIG), turn_lines("turn-b", SMALL)].concat(),
    );
    let before = fx.fingerprint();
    fx.settle("turn-b", &path);
    fx.settle("turn-b", &path);
    assert_eq!(fx.trueup_total(a), 0);
    assert_eq!(
        fx.hold(a).settled_amount,
        Some(HOLD),
        "stays at the flat hold"
    );
    assert_eq!(fx.trueup_total(b), 0, "B was complete and measured exactly");
    assert_eq!(fx.fingerprint(), before);
}

// (10) ------------------------------------------------------------------

fn corrupt_variant(kind: usize) -> Vec<String> {
    let good_b = turn_lines("turn-b", SMALL);
    let a_lines = match kind {
        // all counters negative
        0 => vec![
            started("turn-a"),
            record(
                "turn-a",
                "ra",
                [-2000, -1500, -400, -80, -30],
                [-2000, -1500, -400, -80, -30],
            ),
            complete("turn-a"),
        ],
        // one negative counter among valid ones
        1 => vec![
            started("turn-a"),
            record(
                "turn-a",
                "ra",
                [2000, -1500, 400, 80, 30],
                [2000, -1500, 400, 80, 30],
            ),
            complete("turn-a"),
        ],
        // truncated JSON record
        2 => vec![
            started("turn-a"),
            record("turn-a", "ra", BIG, BIG)[..60].to_string(),
            complete("turn-a"),
        ],
        // counters as strings
        3 => vec![
            started("turn-a"),
            format!(
                r#"{{"type":"token_usage_record","payload":{{"turn_id":"turn-a","thread_id":"{SESSION}","session_id":"{SESSION}","root_turn_id":"turn-a","response_id":"ra","usage":{{"input_tokens":"9999999","output_tokens":"9999999"}}}}}}"#
            ),
            complete("turn-a"),
        ],
        // absurdly large (i64::MAX) counters
        _ => vec![
            started("turn-a"),
            record(
                "turn-a",
                "ra",
                [i64::MAX, 0, 0, i64::MAX, 0],
                [i64::MAX, 0, 0, i64::MAX, 0],
            ),
            complete("turn-a"),
        ],
    };
    [a_lines, good_b].concat()
}

#[test]
fn s10_corrupt_or_negative_counter_rollout_does_not_book_a_bogus_number() {
    let mut report = Vec::new();
    for kind in 0..5 {
        let mut fx = Fx::new(HOLD);
        let (a, _b) = kill_a_then_admit_b(&mut fx, TTL + 600);
        let path = fx.rollout("r.jsonl", &corrupt_variant(kind));
        fx.settle("turn-b", &path);
        let h = fx.hold(a);
        report.push(format!(
            "variant {kind}: state={:?} settled={:?} usage_known={:?}",
            h.state, h.settled_amount, h.usage_known
        ));
        assert_eq!(h.state, ReservationState::Settled, "variant {kind}");
        let settled = h.settled_amount.unwrap();
        assert!(
            settled > 0,
            "variant {kind}: an orphan is never settled at zero ({settled})"
        );
        // Contract: unmeasurable -> exactly the flat hold; a number other than
        // the hold must only come from a real measurement of real counters.
        assert!(
            (h.usage_known == Some(false) && settled == HOLD)
                || (h.usage_known == Some(true) && settled > HOLD),
            "variant {kind}: booked an unexplained number {settled} usage_known={:?}",
            h.usage_known
        );
    }
    eprintln!("S10 observed:\n{}", report.join("\n"));
}
