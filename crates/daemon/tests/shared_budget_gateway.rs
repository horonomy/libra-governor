//! HORO-2274 (S11d): the daemon-side gateway reserve/settle primitives of the
//! shared budget, driven directly (nothing wires the gateway to them yet).
//! A manual clock decides every time-dependent result.
//!
//! Prices in these tests are chosen so that every figure is exact in binary
//! floating point: input at $100000/Mtok makes a token cost 10 cents, so the
//! 1000-token tool-prompt allowance alone is 10000 cents.

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use libra_governor_daemon::shared_budget::{
    admit, gateway_key, gateway_orphans_at_start, gateway_release, gateway_renew, gateway_reserve,
    gateway_settle, GatewayHold, GatewayRefusal, GatewayReleaseOutcome, GatewayRenewOutcome,
    GatewayReserveOutcome, GatewayReserveRequest, GatewaySettleOutcome, GatewaySettleRequest,
    ManualClock, SharedBudgetRuntime,
};
use libra_governor_daemon::shared_budget_file::SHARED_BUDGET_FILE_NAME;
use libra_governor_domain::{
    ChargeSource, ExecutorKind, PoolId, ReservationState, SharedPoolReservation,
    SharedPoolReservationId,
};
use libra_governor_gateway::cost::{EnforcementGap, SettleFlags};
use libra_governor_gateway::usage::ObservedUsage;
use libra_governor_ledger::{CapacitySource, LedgerStore};
use libra_governor_protocol::{SharedBudgetOutcome, SharedBudgetRefusal};

const JAN: &str = "2027-01-15T12:00:00Z";
const CAP: u64 = 100_000;
/// 1000 tool-prompt tokens at 10 cents plus 10 output tokens at 5 cents.
const WORST: u64 = 10_050;

/// A well-formed gateway key.
fn gwkey(boot_id: &str, request_id: &str) -> String {
    gateway_key(boot_id, request_id).unwrap()
}

fn t(s: &str) -> OffsetDateTime {
    OffsetDateTime::parse(s, &Rfc3339).unwrap()
}

/// A valid file with one override, `flat-model`, and the given output price.
fn file(enforcement: &str, cap: u64, output_usd_per_mtok: u64) -> String {
    format!(
        r#"{{"schema_version":1,"monthly_cap_usd_cents":{cap},"time_zone":"UTC",
        "enforcement":"{enforcement}","pricing_overrides":{{"flat-model":{{
        "input_usd_per_mtok":100000,"cache_creation_input_usd_per_mtok":100000,
        "cache_read_input_usd_per_mtok":10000,"output_usd_per_mtok":{output_usd_per_mtok}}}}}}}"#
    )
}

struct Fx {
    dir: tempfile::TempDir,
    clock: Arc<ManualClock>,
    rt: SharedBudgetRuntime,
    ledger: LedgerStore,
}

impl Fx {
    fn new(file: Option<&str>, start: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        if let Some(json) = file {
            std::fs::write(dir.path().join(SHARED_BUDGET_FILE_NAME), json).unwrap();
        }
        let clock = Arc::new(ManualClock::new(t(start)));
        let rt = SharedBudgetRuntime::load(dir.path()).with_clock(clock.clone());
        let ledger = LedgerStore::open(dir.path().join("ledger.sqlite3")).unwrap();
        Self {
            dir,
            clock,
            rt,
            ledger,
        }
    }

    fn gateway() -> Self {
        Self::new(Some(&file("gateway", CAP, 50_000)), JAN)
    }

    fn ledger_path(&self) -> PathBuf {
        self.dir.path().join("ledger.sqlite3")
    }

    fn fingerprint(&self) -> String {
        common::ledger_fingerprint(&self.ledger_path())
    }

    fn reserve(&mut self, req: &GatewayReserveRequest<'_>) -> GatewayReserveOutcome {
        gateway_reserve(&mut self.ledger, &self.rt, req).unwrap()
    }

    fn hold(&mut self, key: &str) -> GatewayHold {
        match self.reserve(&request(key)) {
            GatewayReserveOutcome::Granted(hold) => hold,
            other => panic!("expected a grant, got {other:?}"),
        }
    }

    fn settle(
        &mut self,
        hold: &GatewayHold,
        observed: Option<ObservedUsage>,
        clean: bool,
    ) -> GatewaySettleOutcome {
        let req = GatewaySettleRequest {
            reservation_id: hold.reservation_id,
            observed,
            completed_cleanly: clean,
            saw_message_start: observed.is_some(),
            model: &hold.model,
            max_tokens: hold.max_tokens,
            input_bound_tokens: hold.input_bound_tokens,
        };
        gateway_settle(&mut self.ledger, &self.rt, &req).unwrap()
    }

    fn row(&self, id: SharedPoolReservationId) -> SharedPoolReservation {
        self.ledger.get_shared_reservation(id).unwrap().unwrap()
    }

    fn remaining(&self, pool: &str) -> i64 {
        self.ledger
            .pool_admission_for(
                &PoolId(pool.to_string()),
                self.rt.now(),
                CapacitySource::LocalConfigured { cap: CAP },
            )
            .unwrap()
            .unwrap()
            .remaining()
    }

    fn pools(&self) -> Vec<String> {
        self.ledger
            .shared_pool_ids_with_prefix("metered:")
            .unwrap()
            .into_iter()
            .map(|p| p.0)
            .collect()
    }

    /// Asserts `req` is refused with `expected` and the whole ledger file is
    /// byte-for-byte what it was.
    fn assert_refused_writing_nothing(
        &mut self,
        req: &GatewayReserveRequest<'_>,
        expected: GatewayRefusal,
    ) {
        let before = self.fingerprint();
        assert_eq!(self.reserve(req), GatewayReserveOutcome::Refused(expected));
        assert_eq!(self.fingerprint(), before, "a refusal wrote to the ledger");
    }
}

fn request(key: &str) -> GatewayReserveRequest<'_> {
    GatewayReserveRequest {
        session_id: "sess-1",
        model: "flat-model",
        body_bytes: 0,
        max_tokens: Some(10),
        image_count: 0,
        key,
        ttl_secs: 600,
    }
}

/// (booked, usage_known, flags, overrun) of a settlement that must have happened.
fn settled(outcome: GatewaySettleOutcome) -> (u64, bool, SettleFlags, Option<u64>) {
    match outcome {
        GatewaySettleOutcome::Settled {
            booked_usd_cents,
            usage_known,
            flags,
            overrun_usd_cents,
            ..
        } => (booked_usd_cents, usage_known, flags, overrun_usd_cents),
        other => panic!("expected a settlement, got {other:?}"),
    }
}

fn usage(input: u64, cache_read: u64, output: u64) -> ObservedUsage {
    ObservedUsage {
        input_tokens: input,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: cache_read,
        output_tokens: output,
    }
}

/// 200 input at 10 cents, 100 cache-read at 1 cent, 10 output at 5 cents.
const MEASURED: u64 = 2_150;

// ---- refuse and write nothing ----

#[test]
fn a_missing_file_refuses_and_writes_nothing() {
    let mut fx = Fx::new(None, JAN);
    let key = gwkey("boot1", "r1");
    fx.assert_refused_writing_nothing(&request(&key), GatewayRefusal::NotConfigured);
    assert!(fx.pools().is_empty());
}

#[test]
fn an_invalid_file_refuses_and_writes_nothing() {
    let mut fx = Fx::new(Some("{ not json"), JAN);
    let key = gwkey("boot1", "r1");
    let before = fx.fingerprint();
    let GatewayReserveOutcome::Refused(GatewayRefusal::ConfigInvalid(reason)) =
        fx.reserve(&request(&key))
    else {
        panic!("expected config_invalid");
    };
    assert!(!reason.is_empty());
    assert_eq!(fx.fingerprint(), before);
}

#[test]
fn a_file_that_does_not_ask_for_gateway_enforcement_refuses_and_writes_nothing() {
    let mut fx = Fx::new(Some(&file("hooks", CAP, 50_000)), JAN);
    let key = gwkey("boot1", "r1");
    fx.assert_refused_writing_nothing(&request(&key), GatewayRefusal::NotGatewayEnforced);
}

#[test]
fn a_later_pool_is_a_clock_anomaly_that_writes_nothing() {
    let mut fx = Fx::gateway();
    fx.clock.set(t("2027-02-10T00:00:00Z"));
    let february = fx.hold(&gwkey("boot1", "feb")).pool_id;
    fx.clock.set(t(JAN));
    let key = gwkey("boot1", "jan");
    fx.assert_refused_writing_nothing(
        &request(&key),
        GatewayRefusal::ClockAnomaly {
            later_pool: february,
        },
    );
}

#[test]
fn unusable_identity_key_or_request_refuses_and_writes_nothing() {
    let mut fx = Fx::gateway();
    let key = gwkey("boot1", "r1");
    let long = "x".repeat(257);
    let long_key = format!("gw:boot1:{long}");
    let cases: Vec<(GatewayReserveRequest<'_>, GatewayRefusal)> = vec![
        (
            GatewayReserveRequest {
                session_id: "  ",
                ..request(&key)
            },
            GatewayRefusal::Identity(SharedBudgetRefusal::EmptySessionId),
        ),
        (
            GatewayReserveRequest {
                session_id: "execution-owner-v1:x",
                ..request(&key)
            },
            GatewayRefusal::Identity(SharedBudgetRefusal::ReservedSessionScope),
        ),
        (
            GatewayReserveRequest {
                session_id: &long,
                ..request(&key)
            },
            GatewayRefusal::SessionIdTooLong,
        ),
        (
            GatewayReserveRequest {
                key: "hold:1:s",
                ..request(&key)
            },
            GatewayRefusal::InvalidKey,
        ),
        (
            GatewayReserveRequest {
                key: "gw:boot1:",
                ..request(&key)
            },
            GatewayRefusal::InvalidKey,
        ),
        (
            GatewayReserveRequest {
                key: "gw::r1",
                ..request(&key)
            },
            GatewayRefusal::InvalidKey,
        ),
        (
            GatewayReserveRequest {
                key: &long_key,
                ..request(&key)
            },
            GatewayRefusal::InvalidKey,
        ),
        (
            GatewayReserveRequest {
                max_tokens: None,
                ..request(&key)
            },
            GatewayRefusal::Unenforceable(EnforcementGap::UnboundedRequest),
        ),
        (
            GatewayReserveRequest {
                model: "unlisted-model",
                ..request(&key)
            },
            GatewayRefusal::Unenforceable(EnforcementGap::UnpricedModel {
                model: "unlisted-model".into(),
            }),
        ),
    ];
    for (req, expected) in cases {
        fx.assert_refused_writing_nothing(&req, expected);
    }
    assert!(fx.pools().is_empty());
}

#[test]
fn over_the_cap_is_refused_and_remaining_is_unchanged() {
    let mut fx = Fx::new(Some(&file("gateway", 15_000, 50_000)), JAN);
    let first = fx.hold(&gwkey("boot1", "r1"));
    assert_eq!(first.remaining_usd_cents, 15_000 - WORST as i64);
    let key = gwkey("boot1", "r2");
    fx.assert_refused_writing_nothing(
        &request(&key),
        GatewayRefusal::CapExhausted {
            remaining_usd_cents: 15_000 - WORST as i64,
        },
    );
}

// ---- reserve, settle, release ----

#[test]
fn reserve_settle_and_release_round_trip_in_exact_cents() {
    let mut fx = Fx::gateway();
    let a = fx.hold(&gwkey("boot1", "a"));
    assert_eq!(a.reserved_usd_cents, WORST);
    assert_eq!(a.input_bound_tokens, 1_000);
    assert_eq!(a.remaining_usd_cents, CAP as i64 - WORST as i64);
    assert!(!a.replayed);
    let row = fx.row(a.reservation_id);
    assert_eq!(row.charge_source, Some(ChargeSource::Gateway));
    assert_eq!(row.executor_kind, Some(ExecutorKind::Unattributed));
    assert_eq!(row.agent_id, None);
    assert_eq!(row.session_id, "sess-1");

    let GatewaySettleOutcome::Settled {
        booked_usd_cents,
        reserved_usd_cents,
        usage_known,
        flags,
        overrun_usd_cents,
    } = fx.settle(&a, Some(usage(200, 100, 10)), true)
    else {
        panic!("expected a settlement");
    };
    assert_eq!(
        (
            booked_usd_cents,
            reserved_usd_cents,
            usage_known,
            overrun_usd_cents
        ),
        (MEASURED, WORST, true, None)
    );
    assert!(!flags.truncated_settle && !flags.input_bound_violated);
    assert_eq!(fx.remaining(&a.pool_id), CAP as i64 - MEASURED as i64);
    assert_eq!(
        fx.settle(&a, Some(usage(1, 0, 1)), true),
        GatewaySettleOutcome::AlreadyFinal
    );

    let b = fx.hold(&gwkey("boot1", "b"));
    assert_eq!(
        gateway_release(&mut fx.ledger, &fx.rt, b.reservation_id).unwrap(),
        GatewayReleaseOutcome::Released
    );
    assert_eq!(fx.row(b.reservation_id).state, ReservationState::Released);
    assert_eq!(fx.remaining(&a.pool_id), CAP as i64 - MEASURED as i64);
    assert_eq!(
        gateway_release(&mut fx.ledger, &fx.rt, b.reservation_id).unwrap(),
        GatewayReleaseOutcome::AlreadyFinal
    );
    assert_eq!(
        fx.settle(&b, None, false),
        GatewaySettleOutcome::AlreadyFinal
    );
}

#[test]
fn a_retry_of_the_same_key_is_replayed_without_reserving_again() {
    let mut fx = Fx::gateway();
    let key = gwkey("boot1", "r1");
    let first = fx.hold(&key);
    let again = fx.hold(&key);
    assert!(again.replayed);
    assert_eq!(again.reservation_id, first.reservation_id);
    assert_eq!(again.remaining_usd_cents, first.remaining_usd_cents);
}

#[test]
fn unmeasured_and_truncated_requests_book_the_s11b_amount_marked_unknown() {
    let mut fx = Fx::gateway();

    let none = fx.hold(&gwkey("boot1", "none"));
    let (booked, known, _, _) = settled(fx.settle(&none, None, false));
    assert_eq!((booked, known), (WORST, false));
    assert_eq!(fx.row(none.reservation_id).usage_known, Some(false));

    // Cut off after message_start: the exact input plus max_tokens of output
    // (2150 here), marked unknown, even though it is below the reservation.
    let cut = fx.hold(&gwkey("boot1", "cut"));
    let (booked, known, flags, _) = settled(fx.settle(&cut, Some(usage(200, 100, 3)), false));
    assert_eq!((booked, known), (MEASURED, false));
    assert!(flags.truncated_settle);
    let row = fx.row(cut.reservation_id);
    assert_eq!(
        (row.settled_amount, row.usage_known),
        (Some(MEASURED), Some(false))
    );

    // Cut off with an input overrun: above the reservation, booked in full
    // and still marked unknown.
    let over = fx.hold(&gwkey("boot1", "over"));
    let (booked, known, flags, overrun) =
        settled(fx.settle(&over, Some(usage(5_000, 0, 3)), false));
    assert_eq!(
        (booked, known, overrun),
        (50_050, false, Some(50_050 - WORST))
    );
    assert!(flags.truncated_settle && flags.input_bound_violated);
    assert_eq!(fx.row(over.reservation_id).usage_known, Some(false));
}

#[test]
fn an_input_bound_overrun_is_booked_unclamped_and_flagged() {
    let mut fx = Fx::gateway();
    let hold = fx.hold(&gwkey("boot1", "r1"));
    // 5000 prompt tokens against a 1000-token bound: 50000 cents.
    let (booked_usd_cents, usage_known, flags, overrun_usd_cents) =
        settled(fx.settle(&hold, Some(usage(5_000, 0, 0)), true));
    assert_eq!((booked_usd_cents, usage_known), (50_000, true));
    assert!(flags.input_bound_violated);
    assert_eq!(overrun_usd_cents, Some(50_000 - WORST));
    assert_eq!(fx.remaining(&hold.pool_id), CAP as i64 - 50_000);
}

#[test]
fn a_file_removed_before_settlement_books_the_reservation() {
    let mut fx = Fx::gateway();
    let hold = fx.hold(&gwkey("boot1", "r1"));
    std::fs::remove_file(fx.dir.path().join(SHARED_BUDGET_FILE_NAME)).unwrap();
    let (booked_usd_cents, usage_known, _, _) =
        settled(fx.settle(&hold, Some(usage(200, 100, 10)), true));
    assert_eq!((booked_usd_cents, usage_known), (WORST, false));
}

// ---- pricing ----

#[test]
fn the_file_pricing_overrides_price_both_reserve_and_settle() {
    let mut fx = Fx::gateway();
    // `flat-model` is in no pinned table: it is priced only by the file.
    let hold = fx.hold(&gwkey("boot1", "r1"));
    assert_eq!(hold.reserved_usd_cents, WORST);

    // A pinned model is priced from the pinned table, a different figure.
    let key = gwkey("boot1", "pinned");
    let GatewayReserveOutcome::Granted(pinned) = fx.reserve(&GatewayReserveRequest {
        model: "claude-sonnet-4-5",
        ..request(&key)
    }) else {
        panic!("expected a grant");
    };
    assert!(pinned.reserved_usd_cents < WORST);

    // Editing the file's price takes effect on the next request.
    std::fs::write(
        fx.dir.path().join(SHARED_BUDGET_FILE_NAME),
        file("gateway", CAP, 100_000),
    )
    .unwrap();
    let repriced = fx.hold(&gwkey("boot1", "r2"));
    assert_eq!(repriced.reserved_usd_cents, 10_000 + 100);
    // Settlement reads the same table: 10 output tokens now cost 10 cents each.
    let (booked_usd_cents, _, _, _) =
        settled(fx.settle(&repriced, Some(usage(200, 100, 10)), true));
    assert_eq!(booked_usd_cents, 2_000 + 100 + 100);
}

// ---- periods, renewal, recovery ----

#[test]
fn a_hold_settles_in_its_admission_pool_after_the_month_rolls_over() {
    let mut fx = Fx::new(Some(&file("gateway", CAP, 50_000)), "2027-01-31T23:30:00Z");
    let mut req = request("gw:boot1:r1");
    req.ttl_secs = 3_600;
    let GatewayReserveOutcome::Granted(hold) = fx.reserve(&req) else {
        panic!("expected a grant");
    };
    assert!(hold.pool_id.ends_with("2027-01"), "{}", hold.pool_id);

    fx.clock.set(t("2027-02-01T00:10:00Z"));
    assert!(matches!(
        fx.settle(&hold, Some(usage(200, 100, 10)), true),
        GatewaySettleOutcome::Settled {
            booked_usd_cents: MEASURED,
            ..
        }
    ));
    assert_eq!(fx.row(hold.reservation_id).pool_id.0, hold.pool_id);
    assert_eq!(
        fx.pools(),
        vec![hold.pool_id.clone()],
        "settling created no pool"
    );
    assert_eq!(fx.remaining(&hold.pool_id), CAP as i64 - MEASURED as i64);
}

#[test]
fn renewal_extends_a_live_hold_and_refuses_a_lapsed_one() {
    let mut fx = Fx::gateway();
    let hold = fx.hold(&gwkey("boot1", "r1"));
    fx.clock.set(t("2027-01-15T12:05:00Z"));
    assert_eq!(
        gateway_renew(&mut fx.ledger, &fx.rt, hold.reservation_id, 600).unwrap(),
        GatewayRenewOutcome::Renewed
    );
    assert_eq!(
        fx.row(hold.reservation_id).expires_at,
        t("2027-01-15T12:15:00Z")
    );
    fx.clock.set(t("2027-01-15T12:20:00Z"));
    assert_eq!(
        gateway_renew(&mut fx.ledger, &fx.rt, hold.reservation_id, 600).unwrap(),
        GatewayRenewOutcome::Refused
    );
}

#[test]
fn a_gateway_hold_that_expires_unsettled_is_booked_not_freed() {
    let mut fx = Fx::gateway();
    let hold = fx.hold(&gwkey("boot1", "r1"));
    fx.clock.set(t("2027-01-15T12:20:00Z"));
    // The next reserve sweeps the lapsed hold first.
    let next = fx.hold(&gwkey("boot1", "r2"));
    let row = fx.row(hold.reservation_id);
    assert_eq!(row.state, ReservationState::Settled);
    assert_eq!(
        (row.settled_amount, row.usage_known),
        (Some(WORST), Some(false))
    );
    assert_eq!(next.remaining_usd_cents, CAP as i64 - 2 * WORST as i64);
    assert_eq!(
        fx.settle(&hold, Some(usage(1, 0, 1)), true),
        GatewaySettleOutcome::AlreadyFinal
    );
}

#[test]
fn startup_books_a_previous_boots_holds_and_keeps_the_current_boots() {
    let mut fx = Fx::gateway();
    let old = fx.hold(&gwkey("old", "r1"));
    let current = fx.hold(&gwkey("new", "r1"));

    let booked = gateway_orphans_at_start(&mut fx.ledger, &fx.rt, "new").unwrap();
    assert_eq!(booked.len(), 1);
    assert_eq!(booked[0].id, old.reservation_id);
    let row = fx.row(old.reservation_id);
    assert_eq!(
        (row.state, row.settled_amount, row.usage_known),
        (ReservationState::Settled, Some(WORST), Some(false))
    );
    assert_eq!(
        fx.row(current.reservation_id).state,
        ReservationState::Active
    );
    assert!(gateway_orphans_at_start(&mut fx.ledger, &fx.rt, "new")
        .unwrap()
        .is_empty());
    assert!(gateway_orphans_at_start(&mut fx.ledger, &fx.rt, "a:b").is_err());
    assert!(gateway_orphans_at_start(&mut fx.ledger, &fx.rt, "").is_err());
}

#[test]
fn gateway_calls_never_touch_a_row_that_is_not_a_gateway_hold() {
    let mut fx = Fx::gateway();
    let SharedBudgetOutcome::Allowed { reservation_id, .. } =
        admit(&mut fx.ledger, &fx.rt, "sess-2", None, None, None).unwrap()
    else {
        panic!("expected a turn hold");
    };
    let before = fx.fingerprint();
    let req = GatewaySettleRequest {
        reservation_id,
        observed: Some(usage(1, 0, 1)),
        completed_cleanly: true,
        saw_message_start: true,
        model: "flat-model",
        max_tokens: 10,
        input_bound_tokens: 1_000,
    };
    assert_eq!(
        gateway_settle(&mut fx.ledger, &fx.rt, &req).unwrap(),
        GatewaySettleOutcome::NotGatewayHold
    );
    assert_eq!(
        gateway_release(&mut fx.ledger, &fx.rt, reservation_id).unwrap(),
        GatewayReleaseOutcome::NotGatewayHold
    );
    assert_eq!(
        gateway_renew(&mut fx.ledger, &fx.rt, reservation_id, 600).unwrap(),
        GatewayRenewOutcome::NotGatewayHold
    );
    assert_eq!(fx.fingerprint(), before);

    let unknown = SharedPoolReservationId::new();
    assert_eq!(
        gateway_release(&mut fx.ledger, &fx.rt, unknown).unwrap(),
        GatewayReleaseOutcome::NotFound
    );
    assert_eq!(
        gateway_renew(&mut fx.ledger, &fx.rt, unknown, 600).unwrap(),
        GatewayRenewOutcome::NotFound
    );
}

// ---- key reuse, replay mismatch, key shape (PR #318 review) ----

#[test]
fn a_key_whose_hold_was_released_settled_or_booked_is_not_granted_again() {
    let mut fx = Fx::gateway();
    let released = fx.hold(&gwkey("boot1", "released"));
    gateway_release(&mut fx.ledger, &fx.rt, released.reservation_id).unwrap();
    let key = gwkey("boot1", "released");
    fx.assert_refused_writing_nothing(&request(&key), GatewayRefusal::KeyNotActive);

    let done = fx.hold(&gwkey("boot1", "settled"));
    fx.settle(&done, Some(usage(200, 100, 10)), true);
    let key = gwkey("boot1", "settled");
    fx.assert_refused_writing_nothing(&request(&key), GatewayRefusal::KeyNotActive);

    // Booked on expiry: the sweep inside the reserve writes (the booking), and
    // the retry is then refused rather than granted a hold of nothing.
    let lapsed = fx.hold(&gwkey("boot1", "lapsed"));
    fx.clock.set(t("2027-01-15T12:20:00Z"));
    let key = gwkey("boot1", "lapsed");
    assert_eq!(
        fx.reserve(&request(&key)),
        GatewayReserveOutcome::Refused(GatewayRefusal::KeyNotActive)
    );
    assert_eq!(
        fx.row(lapsed.reservation_id).state,
        ReservationState::Settled
    );
}

#[test]
fn a_replay_must_match_the_held_session_and_size() {
    let mut fx = Fx::gateway();
    let key = gwkey("boot1", "r1");
    fx.hold(&key);
    let other_session = GatewayReserveRequest {
        session_id: "sess-2",
        ..request(&key)
    };
    fx.assert_refused_writing_nothing(&other_session, GatewayRefusal::KeyConflict);
    let other_size = GatewayReserveRequest {
        max_tokens: Some(20),
        ..request(&key)
    };
    fx.assert_refused_writing_nothing(&other_size, GatewayRefusal::KeyConflict);
}

#[test]
fn settlement_prices_from_the_model_and_max_tokens_it_is_given() {
    let mut fx = Fx::gateway();
    let hold = fx.hold(&gwkey("boot1", "r1"));
    assert_eq!((hold.model.as_str(), hold.max_tokens), ("flat-model", 10));
    // Cut off: 200 input (2000) + 100 cache read (100) + max_tokens of
    // output at 5 cents each.
    let req = GatewaySettleRequest {
        reservation_id: hold.reservation_id,
        observed: Some(usage(200, 100, 3)),
        completed_cleanly: false,
        saw_message_start: true,
        model: &hold.model,
        max_tokens: 20,
        input_bound_tokens: hold.input_bound_tokens,
    };
    let (booked, ..) = settled(gateway_settle(&mut fx.ledger, &fx.rt, &req).unwrap());
    assert_eq!(booked, 2_000 + 100 + 100);
}

#[test]
fn keys_are_ascii_graphic_and_a_boot_id_cannot_contain_a_colon() {
    let mut fx = Fx::gateway();
    for bad in [
        "gw:boot1:a\nb",
        "gw:boot1:a b",
        "gw:bo ot:r1",
        "gw:boot1:r\u{e9}",
        "gw:boot1:\tr",
    ] {
        fx.assert_refused_writing_nothing(&request(bad), GatewayRefusal::InvalidKey);
    }
    assert_eq!(gateway_key("a:b", "r1"), None);
    assert_eq!(gateway_key("", "r1"), None);
    assert_eq!(gateway_key("boot1", ""), None);
    assert_eq!(gateway_key("boot1", "a\nb"), None);
    assert_eq!(gateway_key("boot1", "r:1").as_deref(), Some("gw:boot1:r:1"));
}
