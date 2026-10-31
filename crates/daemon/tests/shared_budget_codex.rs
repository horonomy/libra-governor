//! HORO-2282 (C2): Codex turn settlement, turn-scoped from the rollout, dark.
//!
//! Evidence class: **TestOwnedLive** -- synthetic rollouts that reproduce the
//! shape of `fixtures/codex-rollout-token-usage.jsonl` (the shared fixture has
//! no `task_started` line, so a turn-scoped reader cannot start from it; the
//! numbers below are its turn 2 and turn 1, with the start markers added). No
//! real Codex host runs.
//!
//! `settle_codex_turn` is exercised directly against a `LedgerStore` and a
//! `ManualClock` runtime: the `SettleTurn` wire request and every hook are C3.
//! Codex `Admit` stays `not_configured` in this slice, so the holds a Codex
//! turn would have been admitted with are seeded through the same ledger
//! calls `admit` makes (`reserve_shared_metered_for_prompt`,
//! `renew_shared_for_prompt`) on the derived Codex lane.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use libra_governor_daemon::shared_budget::{
    budget_lane, settle_codex_turn, status, Clock, CodexSettlement, ManualClock,
    SharedBudgetRuntime,
};
use libra_governor_daemon::shared_budget_file::SHARED_BUDGET_FILE_NAME;
use libra_governor_daemon::usage::codex_priced::price_codex_turn;
use libra_governor_daemon::usage::TurnUsage;
use libra_governor_domain::{
    ChargeSource, ExecutorKind, PoolId, PrincipalId, QuotaUnit, ReservationState,
    SharedPoolReservation, SharedPoolReservationId, LOCAL_BUDGET_OWNER,
};
use libra_governor_ledger::{
    CapacitySource, ExecutorAttribution, LedgerStore, MeteredReserveRequest,
    SharedPoolReserveRequest,
};

const BASE: &str = "2027-01-15T12:00:00Z";
const OWNER_PREFIX: &str = "metered:local-budget/";
const POOL_JAN: &str = "metered:local-budget/monthly-2027-01";
const SESSION: &str = "codex-sess-1";
const MODEL: &str = "gpt-test-1";
const HOLD: u64 = 1_000;
const CAP: u64 = 100_000;

/// $/Mtok chosen so that fixture turn 2 (fresh 100, cache-write 400,
/// cache-read 1500, output 80) is 2,750,080 micro-dollars = 275.008 cents ->
/// 276 after one round-up, and 351 if the 30 reasoning tokens were added on
/// top of output (3,500,110 micro-dollars = 350.011 cents).
const OVERRIDES: &str = r#","pricing_overrides":{"gpt-test-1":{"input_usd_per_mtok":1000.0,
    "cache_creation_input_usd_per_mtok":1250.0,"cache_read_input_usd_per_mtok":100.0,
    "output_usd_per_mtok":25001.0}}"#;

fn t(s: &str) -> OffsetDateTime {
    OffsetDateTime::parse(s, &Rfc3339).unwrap()
}

fn cfg_json(hold: u64, overrides: bool) -> String {
    format!(
        r#"{{"schema_version":1,"monthly_cap_usd_cents":{CAP},"time_zone":"UTC","turn_hold_usd_cents":{hold}{}}}"#,
        if overrides { OVERRIDES } else { "" }
    )
}

fn principal() -> PrincipalId {
    PrincipalId(LOCAL_BUDGET_OWNER.to_string())
}

fn lp(s: &str) -> String {
    format!("{}:{s}", s.len())
}

fn lane() -> String {
    budget_lane(SESSION, Some("codex")).unwrap()
}

struct Fx {
    hold: u64,
    dir: tempfile::TempDir,
    clock: Arc<ManualClock>,
    rt: SharedBudgetRuntime,
    ledger: Option<LedgerStore>,
}

impl Fx {
    fn new(hold: u64, config: &str, marker: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(SHARED_BUDGET_FILE_NAME), config).unwrap();
        if marker {
            std::fs::write(dir.path().join("shared-budget.codex"), b"").unwrap();
        }
        let clock = Arc::new(ManualClock::new(t(BASE)));
        let rt = SharedBudgetRuntime::load(dir.path()).with_clock(clock.clone());
        let ledger = Some(LedgerStore::open(dir.path().join("ledger.sqlite3")).unwrap());
        Self {
            hold,
            dir,
            clock,
            rt,
            ledger,
        }
    }

    fn ledger(&mut self) -> &mut LedgerStore {
        self.ledger.as_mut().unwrap()
    }

    /// A daemon restart: the ledger connection and the runtime are rebuilt
    /// from disk.
    fn restart(&mut self) {
        self.ledger = None;
        self.rt = SharedBudgetRuntime::load(self.dir.path()).with_clock(self.clock.clone());
        self.ledger = Some(LedgerStore::open(self.dir.path().join("ledger.sqlite3")).unwrap());
    }

    /// What `admit` does for a main-lane prompt, on the derived Codex lane:
    /// renew the lane's live hold and record the prompt, else reserve a hold
    /// for it.
    fn admit(&mut self, turn: &str) -> SharedPoolReservationId {
        let now = self.clock.now();
        let pool = PoolId(POOL_JAN.to_string());
        let lane = lane();
        let hold = self.hold;
        let ledger = self.ledger();
        ledger
            .ensure_pool(&pool, &QuotaUnit::UsdCents, CAP, now)
            .unwrap();
        ledger.expire_stale_shared_pool_reservations(now).unwrap();
        let live = ledger
            .active_shared_hold_for_lane(
                &pool,
                &principal(),
                &lane,
                None,
                ExecutorKind::AgentAbsent,
                now,
            )
            .unwrap();
        if let Some(existing) = live {
            ledger
                .renew_shared_for_prompt(existing.id, &principal(), &lane, now, 3600, Some(turn))
                .unwrap();
            return existing.id;
        }
        let key = format!("hold:{}|-|p{}", lp(&lane), lp(turn));
        let outcome = ledger
            .reserve_shared_metered_for_prompt(
                MeteredReserveRequest {
                    base: SharedPoolReserveRequest {
                        pool_id: &pool,
                        principal_id: &principal(),
                        session_id: &lane,
                        amount: hold,
                        unit: &QuotaUnit::UsdCents,
                        idempotency_key: &key,
                        now,
                        ttl_secs: 3600,
                        attribution: ExecutorAttribution::main_executor(Some(
                            ChargeSource::TurnHold,
                        )),
                    },
                    session_cap: None,
                    capacity_source: CapacitySource::LocalConfigured { cap: CAP },
                },
                Some(turn),
            )
            .unwrap();
        match outcome {
            libra_governor_ledger::MeteredReserveOutcome::Granted(h) => h.id,
            other => panic!("expected a grant, got {other:?}"),
        }
    }

    fn settle(&mut self, turn: &str, model: Option<&str>, path: Option<&Path>) -> CodexSettlement {
        let path = path.map(|p| p.to_str().unwrap().to_string());
        let rt = &self.rt;
        let ledger = self.ledger.as_mut().unwrap();
        settle_codex_turn(ledger, rt, SESSION, turn, model, path.as_deref()).unwrap()
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

    fn post_hoc(&self, turn: &str) -> Option<SharedPoolReservation> {
        let key = format!("codex-turn:{}|{}", lp(&lane()), lp(turn));
        self.ledger
            .as_ref()
            .unwrap()
            .find_shared_reservation_by_key(&principal(), &key, OWNER_PREFIX)
            .unwrap()
    }

    fn used_reserved(&self) -> (u64, u64) {
        let s = status(self.ledger.as_ref().unwrap(), &self.rt).unwrap();
        (s.used_usd_cents, s.reserved_usd_cents)
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

fn fx_with_hold(hold: u64, overrides: bool, marker: bool) -> Fx {
    Fx::new(hold, &cfg_json(hold, overrides), marker)
}

fn fx() -> Fx {
    fx_with_hold(HOLD, true, true)
}

// ---------------------------------------------------------------------
// Synthetic rollout lines (shapes from codex_turn.rs's own tests)
// ---------------------------------------------------------------------

fn task_started(turn: &str) -> String {
    format!(r#"{{"type":"event_msg","payload":{{"type":"task_started","turn_id":"{turn}"}}}}"#)
}

fn task_complete(turn: &str) -> String {
    format!(r#"{{"type":"event_msg","payload":{{"type":"task_complete","turn_id":"{turn}"}}}}"#)
}

/// `usage` = `[input, cached, cache_write, output, reasoning]`; the turn's
/// running total is `turn_total` (the same as `usage` for a one-record turn).
fn usage_record(turn: &str, session: &str, usage: [i64; 5], turn_total: [i64; 5]) -> String {
    let fields = |u: [i64; 5]| {
        format!(
            r#"{{"input_tokens":{},"cached_input_tokens":{},"cache_write_input_tokens":{},"output_tokens":{},"reasoning_output_tokens":{},"total_tokens":{}}}"#,
            u[0],
            u[1],
            u[2],
            u[3],
            u[4],
            u[0] + u[3]
        )
    };
    format!(
        r#"{{"type":"token_usage_record","payload":{{"turn_id":"{turn}","thread_id":"{SESSION}","session_id":"{session}","root_turn_id":"{turn}","response_id":"resp","usage":{},"turn_token_usage":{},"thread_token_usage":{}}}}}"#,
        fields(usage),
        fields(turn_total),
        fields(turn_total),
    )
}

/// Fixture turn 2: fresh 100, cache-write 400, cache-read 1500, output 80
/// (reasoning 30 is inside the 80). Priced at 276 cents by [`OVERRIDES`].
const TURN_BIG: [i64; 5] = [2000, 1500, 400, 80, 30];
const BIG_CENTS: u64 = 276;
/// Fixture turn 1: fresh 100, cache-write 300, cache-read 600, output 50.
/// 1,785,050 micro-dollars = 178.505 cents -> 179.
const TURN_SMALL: [i64; 5] = [1000, 600, 300, 50, 20];
const SMALL_CENTS: u64 = 179;

fn turn_lines(turn: &str, usage: [i64; 5]) -> Vec<String> {
    vec![
        task_started(turn),
        usage_record(turn, SESSION, usage, usage),
        task_complete(turn),
    ]
}

fn settled(o: &CodexSettlement) -> (u64, bool, Option<&'static str>) {
    match o {
        CodexSettlement::Settled {
            settled_usd_cents,
            usage_known,
            unmeasured_reason,
            ..
        } => (*settled_usd_cents, *usage_known, *unmeasured_reason),
        other => panic!("expected Settled, got {other:?}"),
    }
}

// ---------------------------------------------------------------------
// Pricing
// ---------------------------------------------------------------------

#[test]
fn price_codex_turn_refuses_an_unknown_model_even_for_zero_usage() {
    let table = libra_governor_gateway::pricing::PricingTable::pinned();
    assert_eq!(
        price_codex_turn(&TurnUsage::default(), "gpt-unknown-9", &table),
        None,
        "an unknown model is unpriced, never a priced zero"
    );
}

// ---------------------------------------------------------------------
// Measured settlement
// ---------------------------------------------------------------------

#[test]
fn measured_turn_with_overrides_books_exact_cents_with_reasoning_not_double_added() {
    let mut fx = fx();
    fx.admit("turn-a");
    let path = fx.rollout("r.jsonl", &turn_lines("turn-a", TURN_BIG));
    let out = fx.settle("turn-a", Some(MODEL), Some(&path));
    // 276 = ceil(275.008); adding the 30 reasoning tokens would be 351.
    assert_eq!(settled(&out), (BIG_CENTS, true, None));
    let holds = fx.holds();
    assert_eq!(holds.len(), 1);
    assert_eq!(holds[0].state, ReservationState::Settled);
    assert_eq!(holds[0].settled_amount, Some(BIG_CENTS));
    assert_eq!(holds[0].usage_known, Some(true));
    assert_eq!(fx.used_reserved(), (BIG_CENTS, 0));
}

#[test]
fn a_measured_turn_above_the_hold_records_the_overrun_unclamped() {
    let mut fx = fx_with_hold(100, true, true);
    fx.admit("turn-a");
    let path = fx.rollout("r.jsonl", &turn_lines("turn-a", TURN_BIG));
    let out = fx.settle("turn-a", Some(MODEL), Some(&path));
    match out {
        CodexSettlement::Settled {
            hold_usd_cents,
            settled_usd_cents,
            overrun_usd_cents,
            usage_known,
            ..
        } => {
            assert_eq!(hold_usd_cents, 100);
            assert_eq!(settled_usd_cents, BIG_CENTS);
            assert_eq!(overrun_usd_cents, BIG_CENTS - 100);
            assert!(usage_known);
        }
        other => panic!("expected Settled, got {other:?}"),
    }
}

#[test]
fn default_without_gpt_overrides_books_the_flat_hold_unmeasured_and_names_the_model() {
    let mut fx = fx_with_hold(HOLD, false, true);
    fx.admit("turn-a");
    let path = fx.rollout("r.jsonl", &turn_lines("turn-a", TURN_BIG));
    let out = fx.settle("turn-a", Some(MODEL), Some(&path));
    assert_eq!(settled(&out), (HOLD, false, Some("unpriced_model")));
    assert_eq!(fx.holds()[0].usage_known, Some(false));
    let st = status(fx.ledger.as_ref().unwrap(), &fx.rt).unwrap();
    assert_eq!(st.unpriced_models, vec![MODEL.to_string()]);
    assert_eq!(st.unmeasured_settlements, 1);
}

#[test]
fn an_absent_model_books_the_flat_hold_unmeasured() {
    let mut fx = fx();
    fx.admit("turn-a");
    let path = fx.rollout("r.jsonl", &turn_lines("turn-a", TURN_BIG));
    let out = fx.settle("turn-a", None, Some(&path));
    assert_eq!(settled(&out), (HOLD, false, Some("unpriced_model")));
}

// ---------------------------------------------------------------------
// Unmeasurable rollouts settle at the flat hold, never at 0
// ---------------------------------------------------------------------

fn assert_flat_hold(out: &CodexSettlement, reason: &str) {
    assert_eq!(settled(out), (HOLD, false, Some(reason_static(reason))));
}

fn reason_static(reason: &str) -> &'static str {
    match reason {
        "transcript_inconsistent" => "transcript_inconsistent",
        "no_transcript_path" => "no_transcript_path",
        "transcript_unreadable" => "transcript_unreadable",
        other => panic!("unexpected reason {other}"),
    }
}

#[test]
fn a_half_written_tail_books_the_flat_hold() {
    let mut fx = fx();
    fx.admit("turn-a");
    let path = fx.rollout("r.jsonl", &turn_lines("turn-a", TURN_BIG));
    // The writer was mid-line: no trailing newline.
    let mut body = std::fs::read_to_string(&path).unwrap();
    body.pop();
    std::fs::write(&path, body).unwrap();
    let out = fx.settle("turn-a", Some(MODEL), Some(&path));
    assert_flat_hold(&out, "transcript_inconsistent");
}

#[test]
fn a_rollout_of_another_session_books_the_flat_hold_and_imports_nothing() {
    let mut fx = fx();
    fx.admit("turn-a");
    let path = fx.rollout(
        "r.jsonl",
        &[
            task_started("turn-a"),
            usage_record("turn-a", "some-other-session", TURN_BIG, TURN_BIG),
            task_complete("turn-a"),
        ],
    );
    let out = fx.settle("turn-a", Some(MODEL), Some(&path));
    assert_flat_hold(&out, "transcript_inconsistent");
    assert_eq!(fx.used_reserved(), (HOLD, 0));
}

#[test]
fn a_running_total_mismatch_books_the_flat_hold() {
    let mut fx = fx();
    fx.admit("turn-a");
    let mut running = TURN_BIG;
    running[3] += 1;
    let path = fx.rollout(
        "r.jsonl",
        &[
            task_started("turn-a"),
            usage_record("turn-a", SESSION, TURN_BIG, running),
            task_complete("turn-a"),
        ],
    );
    let out = fx.settle("turn-a", Some(MODEL), Some(&path));
    assert_flat_hold(&out, "transcript_inconsistent");
}

#[test]
fn a_turn_id_absent_from_the_rollout_books_the_flat_hold() {
    let mut fx = fx();
    fx.admit("turn-a");
    let path = fx.rollout("r.jsonl", &turn_lines("some-other-turn", TURN_BIG));
    let out = fx.settle("turn-a", Some(MODEL), Some(&path));
    assert_flat_hold(&out, "transcript_inconsistent");
}

#[test]
fn no_transcript_path_and_a_missing_file_book_the_flat_hold() {
    let mut fx = fx();
    fx.admit("turn-a");
    let out = fx.settle("turn-a", Some(MODEL), None);
    assert_flat_hold(&out, "no_transcript_path");

    let mut fx = self::fx();
    fx.admit("turn-a");
    let missing = fx.dir.path().join("does-not-exist.jsonl");
    let out = fx.settle("turn-a", Some(MODEL), Some(&missing));
    assert_flat_hold(&out, "transcript_unreadable");
}

#[test]
fn an_invalid_config_books_the_flat_hold_and_does_not_use_its_overrides() {
    let mut fx = fx();
    fx.admit("turn-a");
    let path = fx.rollout("r.jsonl", &turn_lines("turn-a", TURN_BIG));
    std::fs::write(fx.dir.path().join(SHARED_BUDGET_FILE_NAME), "{ not json").unwrap();
    let out = fx.settle("turn-a", Some(MODEL), Some(&path));
    assert_eq!(settled(&out), (HOLD, false, Some("config_invalid")));
}

// ---------------------------------------------------------------------
// Idempotency and turn scoping
// ---------------------------------------------------------------------

#[test]
fn a_replayed_settle_turn_writes_nothing() {
    let mut fx = fx();
    fx.admit("turn-a");
    let path = fx.rollout("r.jsonl", &turn_lines("turn-a", TURN_BIG));
    fx.settle("turn-a", Some(MODEL), Some(&path));
    let before = fx.fingerprint();
    let again = fx.settle("turn-a", Some(MODEL), Some(&path));
    assert!(
        matches!(again, CodexSettlement::AlreadySettled),
        "{again:?}"
    );
    assert_eq!(fx.fingerprint(), before, "a replay writes no row");
}

#[test]
fn settle_turn_for_t1_after_admit_of_t2_leaves_the_t2_hold_untouched() {
    let mut fx = fx();
    let h1 = fx.admit("turn-1");
    let mut lines = turn_lines("turn-1", TURN_SMALL);
    let path = fx.rollout("r.jsonl", &lines);
    fx.settle("turn-1", Some(MODEL), Some(&path));
    let h2 = fx.admit("turn-2");
    assert_ne!(h1, h2, "a settled hold is not renewed");
    lines.extend(turn_lines("turn-2", TURN_BIG));
    fx.rollout("r.jsonl", &lines);
    let before = fx.fingerprint();
    // A late or duplicate Stop for T1 must not settle T2's pending hold.
    let again = fx.settle("turn-1", Some(MODEL), Some(&path));
    assert!(
        matches!(again, CodexSettlement::AlreadySettled),
        "{again:?}"
    );
    assert_eq!(fx.fingerprint(), before);
    let pending: Vec<_> = fx
        .holds()
        .into_iter()
        .filter(|h| h.state == ReservationState::Active)
        .collect();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id, h2);
}

#[test]
fn interrupt_then_reprompt_books_both_turns_of_the_renewed_hold() {
    let mut fx = fx();
    let h1 = fx.admit("turn-1");
    // No Stop for turn-1 (interrupted); the re-prompt renews the same hold.
    let h2 = fx.admit("turn-2");
    assert_eq!(h1, h2, "a live hold is renewed, not reserved again");
    let mut lines = turn_lines("turn-1", TURN_SMALL);
    lines.extend(turn_lines("turn-2", TURN_BIG));
    let path = fx.rollout("r.jsonl", &lines);
    let out = fx.settle("turn-2", Some(MODEL), Some(&path));
    assert_eq!(
        settled(&out),
        (SMALL_CENTS + BIG_CENTS, true, None),
        "a reader of turn-2 alone would book only {BIG_CENTS}"
    );
    assert_eq!(fx.holds().len(), 1);
}

#[test]
fn an_unmeasurable_turn_in_a_renewed_hold_settles_at_the_larger_of_priced_and_hold() {
    // turn-1 measured (276 > hold 100), turn-2 missing from the rollout:
    // the measured part alone already exceeds the hold, so it is booked as
    // measured.
    let mut fx = fx_with_hold(100, true, true);
    fx.admit("turn-1");
    fx.admit("turn-2");
    let path = fx.rollout("r.jsonl", &turn_lines("turn-1", TURN_BIG));
    let out = fx.settle("turn-2", Some(MODEL), Some(&path));
    assert_eq!(
        settled(&out),
        (BIG_CENTS, true, Some("transcript_inconsistent"))
    );

    // Under the hold, the hold stands and is recorded as unmeasured.
    let mut fx = fx_with_hold(HOLD, true, true);
    fx.admit("turn-1");
    fx.admit("turn-2");
    let path = fx.rollout("r.jsonl", &turn_lines("turn-1", TURN_BIG));
    let out = fx.settle("turn-2", Some(MODEL), Some(&path));
    assert_eq!(
        settled(&out),
        (HOLD, false, Some("transcript_inconsistent"))
    );
}

#[test]
fn an_unmapped_stop_never_settles_another_turns_pending_hold() {
    let mut fx = fx();
    let pending = fx.admit("turn-a");
    // A Stop for a turn that was never admitted (a subagent turn firing Stop
    // with the parent's session id, say). Its rollout is measurable.
    let path = fx.rollout("r.jsonl", &turn_lines("turn-x", TURN_BIG));
    let out = fx.settle("turn-x", Some(MODEL), Some(&path));
    match out {
        CodexSettlement::Unadmitted {
            charged_usd_cents,
            usage_known,
            already_charged,
            unmeasured_reason,
            ..
        } => {
            assert_eq!(charged_usd_cents, BIG_CENTS);
            assert!(usage_known);
            assert!(!already_charged);
            assert_eq!(unmeasured_reason, None);
        }
        other => panic!("expected Unadmitted, got {other:?}"),
    }
    let holds = fx.holds();
    assert_eq!(holds.len(), 1);
    assert_eq!(holds[0].id, pending);
    assert_eq!(holds[0].state, ReservationState::Active, "hold untouched");
    let charge = fx.post_hoc("turn-x").expect("a post-hoc charge row");
    assert_eq!(charge.settled_amount, Some(BIG_CENTS));
    assert_eq!(charge.usage_known, Some(true));
    assert_eq!(charge.executor_kind, Some(ExecutorKind::AgentAbsent));
    assert_eq!(charge.charge_source, Some(ChargeSource::TurnHold));
    assert_eq!(fx.used_reserved(), (BIG_CENTS, HOLD));
    assert_eq!(fx.rt.unadmitted_turns(), 1);
}

#[test]
fn a_post_hoc_charge_for_an_unmeasurable_unadmitted_turn_is_the_flat_hold() {
    let mut fx = fx();
    let out = fx.settle("turn-x", Some(MODEL), None);
    match out {
        CodexSettlement::Unadmitted {
            charged_usd_cents,
            usage_known,
            unmeasured_reason,
            ..
        } => {
            assert_eq!(charged_usd_cents, HOLD);
            assert!(!usage_known);
            assert_eq!(unmeasured_reason, Some("no_transcript_path"));
        }
        other => panic!("expected Unadmitted, got {other:?}"),
    }
}

#[test]
fn a_post_hoc_charge_is_idempotent_on_retry() {
    let mut fx = fx();
    let path = fx.rollout("r.jsonl", &turn_lines("turn-x", TURN_BIG));
    fx.settle("turn-x", Some(MODEL), Some(&path));
    let before = fx.fingerprint();
    let again = fx.settle("turn-x", Some(MODEL), Some(&path));
    match again {
        CodexSettlement::Unadmitted {
            already_charged, ..
        } => assert!(already_charged),
        other => panic!("expected Unadmitted, got {other:?}"),
    }
    assert_eq!(fx.fingerprint(), before);
    assert_eq!(fx.used_reserved(), (BIG_CENTS, 0));
}

#[test]
fn a_late_settle_of_an_expired_hold_lands_in_its_own_row_and_period() {
    let mut fx = fx();
    let h = fx.admit("turn-a");
    let path = fx.rollout("r.jsonl", &turn_lines("turn-a", TURN_BIG));
    // The hold expires unsettled, the month rolls over, and only then does
    // the Stop arrive.
    fx.clock.set(t("2027-01-15T14:00:00Z"));
    let now = fx.clock.now();
    fx.ledger()
        .expire_stale_shared_pool_reservations(now)
        .unwrap();
    assert_eq!(fx.holds()[0].state, ReservationState::Expired);
    fx.clock.set(t("2027-02-02T09:00:00Z"));
    let out = fx.settle("turn-a", Some(MODEL), Some(&path));
    assert_eq!(settled(&out), (BIG_CENTS, true, None));
    let holds = fx.holds();
    assert_eq!(holds.len(), 1);
    assert_eq!(holds[0].id, h);
    assert_eq!(holds[0].pool_id.0, POOL_JAN, "the hold's own period");
    assert_eq!(holds[0].state, ReservationState::Settled);
    let pools: Vec<String> = fx
        .ledger()
        .shared_pool_ids_with_prefix(OWNER_PREFIX)
        .unwrap()
        .into_iter()
        .map(|p| p.0)
        .collect();
    assert_eq!(pools, vec![POOL_JAN.to_string()], "no February pool");
}

#[test]
fn a_restart_between_admit_and_settle_settles_exactly_once() {
    let mut fx = fx();
    fx.admit("turn-a");
    let path = fx.rollout("r.jsonl", &turn_lines("turn-a", TURN_BIG));
    fx.restart();
    let first = fx.settle("turn-a", Some(MODEL), Some(&path));
    assert_eq!(settled(&first), (BIG_CENTS, true, None));
    fx.restart();
    let second = fx.settle("turn-a", Some(MODEL), Some(&path));
    assert!(
        matches!(second, CodexSettlement::AlreadySettled),
        "{second:?}"
    );
    assert_eq!(fx.holds().len(), 1);
    assert_eq!(fx.used_reserved(), (BIG_CENTS, 0));
}

#[test]
fn a_codex_settle_never_touches_the_claude_hold_of_the_same_raw_session_id() {
    let mut fx = fx();
    // A Claude hold for the same raw session id, through the real admit.
    let claude = libra_governor_daemon::shared_budget::admit(
        fx.ledger.as_mut().unwrap(),
        &fx.rt,
        SESSION,
        None,
        Some("turn-a"),
        None,
    )
    .unwrap();
    assert!(matches!(
        claude,
        libra_governor_protocol::SharedBudgetOutcome::Allowed { .. }
    ));
    fx.admit("turn-a");
    let path = fx.rollout("r.jsonl", &turn_lines("turn-a", TURN_BIG));
    fx.settle("turn-a", Some(MODEL), Some(&path));
    let claude_holds = fx
        .ledger()
        .shared_lane_holds(
            &principal(),
            SESSION,
            None,
            ExecutorKind::AgentAbsent,
            OWNER_PREFIX,
        )
        .unwrap();
    assert_eq!(claude_holds.len(), 1);
    assert_eq!(claude_holds[0].state, ReservationState::Active);
}

// ---------------------------------------------------------------------
// Dark: marker-gated
// ---------------------------------------------------------------------

#[test]
fn without_the_marker_every_codex_settle_is_not_configured_and_writes_zero_rows() {
    let mut fx = fx_with_hold(HOLD, true, false);
    // A hold that would have been settled, and a measurable rollout.
    fx.admit("turn-a");
    let path = fx.rollout("r.jsonl", &turn_lines("turn-a", TURN_BIG));
    let other = fx.rollout("o.jsonl", &turn_lines("turn-x", TURN_BIG));
    let before = fx.fingerprint();
    for out in [
        fx.settle("turn-a", Some(MODEL), Some(&path)),
        fx.settle("turn-x", Some(MODEL), Some(&other)),
        fx.settle("turn-a", None, None),
    ] {
        assert!(matches!(out, CodexSettlement::NotConfigured), "{out:?}");
    }
    assert_eq!(fx.fingerprint(), before, "the whole ledger is unchanged");
    assert_eq!(fx.rt.unadmitted_turns(), 0);
    assert!(!fx.rt.codex_enrolled());
}

#[test]
fn the_marker_is_read_on_every_call_so_removing_it_switches_the_path_off() {
    let fx = fx();
    assert!(fx.rt.codex_enrolled());
    std::fs::remove_file(fx.dir.path().join("shared-budget.codex")).unwrap();
    assert!(!fx.rt.codex_enrolled());
}

#[test]
fn an_unusable_turn_id_is_not_applicable_and_writes_nothing() {
    let mut fx = fx();
    fx.admit("turn-a");
    let before = fx.fingerprint();
    for turn in ["", "   ", &"x".repeat(257)] {
        let out = fx.settle(turn, Some(MODEL), None);
        assert!(matches!(out, CodexSettlement::NotApplicable), "{out:?}");
    }
    assert_eq!(fx.fingerprint(), before);
}

#[test]
fn a_codex_request_answers_not_configured_and_writes_nothing_with_or_without_the_marker() {
    use libra_governor_daemon::shared_budget::{admit, charge, check};
    use libra_governor_protocol::SharedBudgetOutcome;
    for marker in [false, true] {
        let mut fx = fx_with_hold(HOLD, true, marker);
        let before = fx.fingerprint();
        let rt = &fx.rt;
        let ledger = fx.ledger.as_mut().unwrap();
        let admitted = admit(ledger, rt, SESSION, None, Some("turn-a"), Some("codex")).unwrap();
        assert!(
            matches!(admitted, SharedBudgetOutcome::NotConfigured),
            "{admitted:?}"
        );
        let checked = check(ledger, rt, SESSION, None, Some("codex")).unwrap();
        assert!(
            matches!(checked, SharedBudgetOutcome::NotConfigured),
            "{checked:?}"
        );
        let charged = charge(
            ledger,
            rt,
            SESSION,
            None,
            5,
            "k",
            ChargeSource::TurnHold,
            None,
            Some("codex"),
        )
        .unwrap();
        assert!(
            matches!(charged, SharedBudgetOutcome::NotConfigured),
            "{charged:?}"
        );
        assert_eq!(
            fx.fingerprint(),
            before,
            "marker={marker}: nothing reserved or charged"
        );
    }
}

#[test]
fn removing_the_marker_after_enabling_returns_settlement_to_inert() {
    let mut fx = fx();
    fx.admit("turn-a");
    fx.admit("turn-b");
    let path = fx.rollout("r.jsonl", &turn_lines("turn-a", TURN_BIG));
    // Enrolled: an unadmitted turn is charged.
    let on = fx.settle("turn-x", Some(MODEL), None);
    assert!(matches!(on, CodexSettlement::Unadmitted { .. }), "{on:?}");
    std::fs::remove_file(fx.dir.path().join("shared-budget.codex")).unwrap();
    let before = fx.fingerprint();
    let off_hold = fx.settle("turn-a", Some(MODEL), Some(&path));
    let off_charge = fx.settle("turn-y", Some(MODEL), None);
    assert!(
        matches!(off_hold, CodexSettlement::NotConfigured),
        "{off_hold:?}"
    );
    assert!(
        matches!(off_charge, CodexSettlement::NotConfigured),
        "{off_charge:?}"
    );
    assert_eq!(
        fx.fingerprint(),
        before,
        "whole ledger unchanged once inert"
    );
}

// ---------------------------------------------------------------------
// Turn cap (HORO-2282 review follow-up)
// ---------------------------------------------------------------------

fn many_turn_hold(fx: &mut Fx, turns: usize) -> PathBuf {
    let mut lines = Vec::new();
    for i in 0..turns {
        let id = format!("turn-{i:02}");
        fx.admit(&id);
        lines.extend(turn_lines(&id, TURN_SMALL));
    }
    fx.rollout("r.jsonl", &lines)
}

#[test]
fn a_hold_over_the_turn_cap_measures_the_first_turns_and_books_the_larger_of_priced_and_hold() {
    // 20 turns on one hold; only the first 16 are measured: 16 * 179 = 2864,
    // above the 1,000 hold, so it is booked as measured with the cap label.
    let mut fx = fx();
    let path = many_turn_hold(&mut fx, 20);
    let out = fx.settle("turn-19", Some(MODEL), Some(&path));
    assert_eq!(
        settled(&out),
        (16 * SMALL_CENTS, true, Some("scan_cap_reached"))
    );
}

#[test]
fn a_hold_over_the_turn_cap_never_books_below_the_flat_hold() {
    // 16 * 179 = 2864 is under a 50,000 hold: the hold stands, unmeasured.
    let mut fx = fx_with_hold(50_000, true, true);
    let path = many_turn_hold(&mut fx, 20);
    let out = fx.settle("turn-19", Some(MODEL), Some(&path));
    assert_eq!(settled(&out), (50_000, false, Some("scan_cap_reached")));
}
