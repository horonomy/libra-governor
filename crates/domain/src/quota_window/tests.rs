//! Deterministic fixtures for HORO-1762's acceptance criteria. See
//! `docs/adr/0015-quota-windows.md` for the DST instants used here,
//! independently verified against jiff 0.2.37's bundled tzdb.

use time::macros::datetime;

use super::*;
use crate::economic_attribution::Attributed;
use crate::economic_event::{EconomicEvent, FactRole, ResourceBasis, ResourceFact};
use crate::{
    EconomicAttribution, EconomicScope, ExecutionIdentityBuilder, ReservationClass,
    ReservationState as ResState,
};

fn scope_at(observed_at: OffsetDateTime) -> QuotaScope {
    QuotaScope {
        subject: QuotaSubject::SharedPool(PoolId("team-a".to_string())),
        source: EntitlementSource::OperatorConfigured,
        confidence: Confidence::High,
        observed_at,
        valid_until: None,
    }
}

fn window(unit: QuotaUnit, kind: WindowKind) -> QuotaWindow {
    QuotaWindow::validated(
        QuotaWindowId::new(),
        scope_at(datetime!(2026-01-01 0:00:00 UTC)),
        unit,
        kind,
    )
    .expect("valid fixture window")
}

fn usage(occurred_at: OffsetDateTime, value: u64) -> QuotaUsage {
    QuotaUsage::new(
        EconomicEventId::new(),
        occurred_at,
        QuotaAmount::new(QuotaUnit::Tokens, value),
    )
    .expect("valid fixture usage")
}

fn ny() -> IanaTimeZone {
    IanaTimeZone::new("America/New_York").unwrap()
}

fn empty_evidence() -> QuotaEvidence<'static> {
    QuotaEvidence {
        usage: &[],
        holds: &[],
        snapshots: &[],
    }
}

// --- Fixtures 1-6: FixedAligned DST correctness -----------------------

#[test]
fn daily_reset_spans_23_hours_across_spring_forward() {
    let w = window(
        QuotaUnit::Tokens,
        WindowKind::FixedAligned {
            period: AlignedPeriod::Day {
                at: WallClockTime::new(0, 0).unwrap(),
            },
            time_zone: ny(),
            limit: 100,
        },
    );
    // now is within the 2026-03-08 local day (before spring-forward, 06:00Z).
    let now = datetime!(2026-03-08 06:00:00 UTC);
    let eval = w.evaluate(&empty_evidence(), now);
    let WindowState::Period(p) = eval.state else {
        panic!("expected Period state")
    };
    assert_eq!(p.start, datetime!(2026-03-08 05:00:00 UTC));
    assert_eq!(p.end, datetime!(2026-03-09 04:00:00 UTC));
    assert_eq!((p.end - p.start).whole_hours(), 23);
}

#[test]
fn daily_reset_spans_25_hours_across_fall_back() {
    let w = window(
        QuotaUnit::Tokens,
        WindowKind::FixedAligned {
            period: AlignedPeriod::Day {
                at: WallClockTime::new(0, 0).unwrap(),
            },
            time_zone: ny(),
            limit: 100,
        },
    );
    let now = datetime!(2026-11-01 06:00:00 UTC);
    let eval = w.evaluate(&empty_evidence(), now);
    let WindowState::Period(p) = eval.state else {
        panic!("expected Period state")
    };
    assert_eq!(p.start, datetime!(2026-11-01 04:00:00 UTC));
    assert_eq!(p.end, datetime!(2026-11-02 05:00:00 UTC));
    assert_eq!((p.end - p.start).whole_hours(), 25);
}

#[test]
fn daily_reset_in_a_spring_forward_gap_resolves_to_the_later_instant() {
    let w = window(
        QuotaUnit::Tokens,
        WindowKind::FixedAligned {
            period: AlignedPeriod::Day {
                at: WallClockTime::new(2, 30).unwrap(),
            },
            time_zone: ny(),
            limit: 100,
        },
    );
    let now = datetime!(2026-03-08 08:00:00 UTC);
    let eval = w.evaluate(&empty_evidence(), now);
    let WindowState::Period(p) = eval.state else {
        panic!("expected Period state")
    };
    // 02:30 doesn't exist that day (clocks jump 02:00->03:00); resolves
    // to 03:30 EDT = 07:30Z (Compatible: later instant in a gap).
    assert_eq!(p.start, datetime!(2026-03-08 07:30:00 UTC));
}

#[test]
fn daily_reset_in_a_fall_back_fold_resolves_to_the_earlier_occurrence() {
    let w = window(
        QuotaUnit::Tokens,
        WindowKind::FixedAligned {
            period: AlignedPeriod::Day {
                at: WallClockTime::new(1, 30).unwrap(),
            },
            time_zone: ny(),
            limit: 100,
        },
    );
    let now = datetime!(2026-11-01 06:00:00 UTC);
    let eval = w.evaluate(&empty_evidence(), now);
    let WindowState::Period(p) = eval.state else {
        panic!("expected Period state")
    };
    // 01:30 occurs twice (fold); Compatible takes the first (EDT, -04).
    assert_eq!(p.start, datetime!(2026-11-01 05:30:00 UTC));
}

#[test]
fn hourly_window_during_the_fold_treats_each_local_hour_as_a_separate_period() {
    let w = window(
        QuotaUnit::Tokens,
        WindowKind::FixedAligned {
            period: AlignedPeriod::Hour,
            time_zone: ny(),
            limit: 100,
        },
    );
    let eval1 = w.evaluate(&empty_evidence(), datetime!(2026-11-01 05:30:00 UTC));
    let eval2 = w.evaluate(&empty_evidence(), datetime!(2026-11-01 06:30:00 UTC));
    let (WindowState::Period(p1), WindowState::Period(p2)) = (eval1.state, eval2.state) else {
        panic!("expected Period states")
    };
    assert_eq!(p1.start, datetime!(2026-11-01 05:00:00 UTC));
    assert_eq!(p2.start, datetime!(2026-11-01 06:00:00 UTC));
    assert_ne!(
        p1.start, p2.start,
        "the two local 1:00 hours are separate periods"
    );
}

#[test]
fn daily_reset_in_a_zone_with_no_local_midnight() {
    let santiago = IanaTimeZone::new("America/Santiago").unwrap();
    let w = window(
        QuotaUnit::Tokens,
        WindowKind::FixedAligned {
            period: AlignedPeriod::Day {
                at: WallClockTime::new(0, 0).unwrap(),
            },
            time_zone: santiago,
            limit: 100,
        },
    );
    let now = datetime!(2026-09-06 12:00:00 UTC);
    let eval = w.evaluate(&empty_evidence(), now);
    let WindowState::Period(p) = eval.state else {
        panic!("expected Period state")
    };
    assert_eq!(p.start, datetime!(2026-09-06 04:00:00 UTC));
    assert_eq!(p.end, datetime!(2026-09-07 03:00:00 UTC));
    assert_eq!((p.end - p.start).whole_hours(), 23);
}

#[test]
fn hourly_window_in_a_half_hour_offset_zone() {
    let kolkata = IanaTimeZone::new("Asia/Kolkata").unwrap();
    let w = window(
        QuotaUnit::Tokens,
        WindowKind::FixedAligned {
            period: AlignedPeriod::Hour,
            time_zone: kolkata,
            limit: 100,
        },
    );
    let now = datetime!(2026-10-08 12:10:00 UTC);
    let eval = w.evaluate(&empty_evidence(), now);
    let WindowState::Period(p) = eval.state else {
        panic!("expected Period state")
    };
    assert_eq!(p.start, datetime!(2026-10-08 11:30:00 UTC));
}

#[test]
fn weekly_window_aligns_to_its_declared_start_weekday() {
    let w = window(
        QuotaUnit::Tokens,
        WindowKind::FixedAligned {
            period: AlignedPeriod::Week {
                starts_on: ResetWeekday::Monday,
                at: WallClockTime::new(0, 0).unwrap(),
            },
            time_zone: ny(),
            limit: 100,
        },
    );
    // 2026-10-08 is a Thursday.
    let now = datetime!(2026-10-08 12:00:00 UTC);
    let eval = w.evaluate(&empty_evidence(), now);
    let WindowState::Period(p) = eval.state else {
        panic!("expected Period state")
    };
    assert_eq!(p.start, datetime!(2026-10-05 04:00:00 UTC)); // Monday 2026-10-05 00:00 EDT
    assert_eq!(p.end, datetime!(2026-10-12 04:00:00 UTC));
}

// --- Fixtures 8-9: independent windows, one reset does not affect another

#[test]
fn hourly_resets_while_rolling_six_hour_stays_blocked() {
    let hourly = window(
        QuotaUnit::Tokens,
        WindowKind::FixedAligned {
            period: AlignedPeriod::Hour,
            time_zone: ny(),
            limit: 100,
        },
    );
    let sliding = window(
        QuotaUnit::Tokens,
        WindowKind::Sliding {
            length_secs: 6 * 3600,
            limit: 300,
        },
    );
    let spend = usage(datetime!(2026-10-08 10:30:00 UTC), 300);
    let evidence = QuotaEvidence {
        usage: &[spend],
        holds: &[],
        snapshots: &[],
    };
    let now = datetime!(2026-10-08 11:05:00 UTC);

    let hourly_eval = hourly.evaluate(&evidence, now);
    assert_eq!(hourly_eval.blocking, BlockingStatus::NotBlocking);

    let sliding_eval = sliding.evaluate(&evidence, now);
    assert_eq!(sliding_eval.blocking, BlockingStatus::Blocking);
    let WindowState::Period(p) = sliding_eval.state else {
        panic!("expected Period state")
    };
    assert_eq!(p.relief, Relief::At(datetime!(2026-10-08 16:30:00 UTC)));
}

#[test]
fn weekly_resets_while_daily_stays_blocked() {
    let weekly = window(
        QuotaUnit::Tokens,
        WindowKind::FixedAligned {
            period: AlignedPeriod::Week {
                starts_on: ResetWeekday::Monday,
                at: WallClockTime::new(0, 0).unwrap(),
            },
            time_zone: ny(),
            limit: 1000,
        },
    );
    let daily = window(
        QuotaUnit::Tokens,
        WindowKind::FixedAligned {
            period: AlignedPeriod::Day {
                at: WallClockTime::new(9, 0).unwrap(),
            },
            time_zone: ny(),
            limit: 100,
        },
    );
    // Sunday 2026-10-11 10:00 local (14:00Z) hits the daily limit (the
    // daily period running 10-11 09:00 -> 10-12 09:00 local).
    let spend = usage(datetime!(2026-10-11 14:00:00 UTC), 100);
    let evidence = QuotaEvidence {
        usage: &[spend],
        holds: &[],
        snapshots: &[],
    };
    // Monday 2026-10-12 00:30 local = 04:30Z.
    let now = datetime!(2026-10-12 04:30:00 UTC);

    let weekly_eval = weekly.evaluate(&evidence, now);
    assert_eq!(weekly_eval.blocking, BlockingStatus::NotBlocking);

    let daily_eval = daily.evaluate(&evidence, now);
    assert_eq!(daily_eval.blocking, BlockingStatus::Blocking);
}

// --- Fixture 10-11: Sliding edges, future usage excluded --------------

#[test]
fn sliding_window_edge_excludes_the_lower_bound_includes_now() {
    let w = window(
        QuotaUnit::Tokens,
        WindowKind::Sliding {
            length_secs: 3600,
            limit: 1000,
        },
    );
    let now = datetime!(2026-10-08 12:00:00 UTC);
    let at_lower_bound = usage(datetime!(2026-10-08 11:00:00 UTC), 50);
    let at_now = usage(now, 50);
    let evidence = QuotaEvidence {
        usage: &[at_lower_bound, at_now],
        holds: &[],
        snapshots: &[],
    };
    let eval = w.evaluate(&evidence, now);
    let WindowState::Period(p) = eval.state else {
        panic!("expected Period state")
    };
    assert_eq!(p.settled, 50, "only the record at `now` should count");
}

#[test]
fn usage_later_than_now_is_excluded_from_every_window_kind() {
    let now = datetime!(2026-10-08 12:00:00 UTC);
    let future = usage(now + time::Duration::seconds(1), 1_000_000);
    let evidence = QuotaEvidence {
        usage: &[future],
        holds: &[],
        snapshots: &[],
    };

    let fixed = window(
        QuotaUnit::Tokens,
        WindowKind::FixedAligned {
            period: AlignedPeriod::Hour,
            time_zone: ny(),
            limit: 10,
        },
    );
    assert_eq!(
        fixed.evaluate(&evidence, now).blocking,
        BlockingStatus::NotBlocking
    );

    let sliding = window(
        QuotaUnit::Tokens,
        WindowKind::Sliding {
            length_secs: 3600,
            limit: 10,
        },
    );
    assert_eq!(
        sliding.evaluate(&evidence, now).blocking,
        BlockingStatus::NotBlocking
    );

    let bucket = window(
        QuotaUnit::Tokens,
        WindowKind::RefillBucket {
            capacity: 10,
            refill_amount: 1,
            refill_period_secs: 1,
            anchored_at: now - time::Duration::seconds(10),
            level_at_anchor: 10,
        },
    );
    assert_eq!(
        bucket.evaluate(&evidence, now).blocking,
        BlockingStatus::NotBlocking
    );
}

// --- Fixtures 12-15: gauge (OpaqueProviderSnapshot) semantics ----------

fn gauge_window(unit: QuotaUnit, max_staleness_secs: u64) -> QuotaWindow {
    window(
        unit,
        WindowKind::OpaqueProviderSnapshot { max_staleness_secs },
    )
}

#[test]
fn stale_snapshot_past_valid_until_is_indeterminate_and_never_trusted() {
    let w = gauge_window(QuotaUnit::Percent, 3600);
    let snap = ProviderSnapshot::validated(
        w.id(),
        datetime!(2026-10-08 10:00:00 UTC),
        Some(datetime!(2026-10-08 10:30:00 UTC)),
        None,
        GaugeReading::Used {
            used: QuotaAmount::new(QuotaUnit::Percent, 5_000),
            limit: None,
        },
        Confidence::High,
    )
    .unwrap();
    let evidence = QuotaEvidence {
        usage: &[],
        holds: &[],
        snapshots: &[snap],
    };
    let now = datetime!(2026-10-08 11:00:00 UTC);
    let eval = w.evaluate(&evidence, now);
    assert_eq!(
        eval.blocking,
        BlockingStatus::Indeterminate(IndeterminateReason::SnapshotStale)
    );
    let WindowState::Gauge(g) = eval.state else {
        panic!("expected Gauge state")
    };
    assert_eq!(
        g.freshness,
        GaugeFreshness::Stale(StaleReason::PastValidUntil)
    );
    assert!(g.trusted_reading().is_none());
    assert!(g.latest.is_some(), "a stale snapshot stays visible");
}

#[test]
fn snapshot_older_than_max_staleness_with_no_valid_until_is_stale() {
    let w = gauge_window(QuotaUnit::Percent, 60);
    let snap = ProviderSnapshot::validated(
        w.id(),
        datetime!(2026-10-08 10:00:00 UTC),
        None,
        None,
        GaugeReading::Used {
            used: QuotaAmount::new(QuotaUnit::Percent, 1_000),
            limit: None,
        },
        Confidence::High,
    )
    .unwrap();
    let evidence = QuotaEvidence {
        usage: &[],
        holds: &[],
        snapshots: &[snap],
    };
    let now = datetime!(2026-10-08 10:02:00 UTC);
    let eval = w.evaluate(&evidence, now);
    let WindowState::Gauge(g) = eval.state else {
        panic!("expected Gauge state")
    };
    assert_eq!(
        g.freshness,
        GaugeFreshness::Stale(StaleReason::ExceedsMaxStaleness)
    );
}

#[test]
fn undisclosed_reading_is_not_zero_and_missing_snapshot_is_indeterminate() {
    let w = gauge_window(QuotaUnit::Percent, 3600);
    let now = datetime!(2026-10-08 10:00:00 UTC);

    let no_snapshot_eval = w.evaluate(&empty_evidence(), now);
    assert_eq!(
        no_snapshot_eval.blocking,
        BlockingStatus::Indeterminate(IndeterminateReason::NoSnapshot)
    );

    let undisclosed = ProviderSnapshot::validated(
        w.id(),
        now,
        None,
        None,
        GaugeReading::Undisclosed,
        Confidence::Low,
    )
    .unwrap();
    let evidence = QuotaEvidence {
        usage: &[],
        holds: &[],
        snapshots: &[undisclosed],
    };
    let eval = w.evaluate(&evidence, now);
    assert_eq!(
        eval.blocking,
        BlockingStatus::Indeterminate(IndeterminateReason::SnapshotUndisclosed),
        "undisclosed must never be conflated with a 0% reading"
    );
}

#[test]
fn future_snapshot_is_ignored() {
    let w = gauge_window(QuotaUnit::Percent, 3600);
    let now = datetime!(2026-10-08 10:00:00 UTC);
    let future_snap = ProviderSnapshot::validated(
        w.id(),
        now + time::Duration::seconds(1),
        None,
        None,
        GaugeReading::Used {
            used: QuotaAmount::new(QuotaUnit::Percent, 0),
            limit: None,
        },
        Confidence::High,
    )
    .unwrap();
    let evidence = QuotaEvidence {
        usage: &[],
        holds: &[],
        snapshots: &[future_snap],
    };
    let eval = w.evaluate(&evidence, now);
    assert_eq!(
        eval.blocking,
        BlockingStatus::Indeterminate(IndeterminateReason::NoSnapshot)
    );
}

#[test]
fn fresh_percent_snapshot_above_100_percent_blocks_and_stays_unclamped() {
    let w = gauge_window(QuotaUnit::Percent, 3600);
    let now = datetime!(2026-10-08 10:00:00 UTC);
    let reset = datetime!(2026-10-09 00:00:00 UTC);
    let snap = ProviderSnapshot::validated(
        w.id(),
        now,
        None,
        Some(reset),
        GaugeReading::Used {
            used: QuotaAmount::new(QuotaUnit::Percent, 10_500),
            limit: None,
        },
        Confidence::High,
    )
    .unwrap();
    let evidence = QuotaEvidence {
        usage: &[],
        holds: &[],
        snapshots: &[snap],
    };
    let eval = w.evaluate(&evidence, now);
    assert_eq!(eval.blocking, BlockingStatus::Blocking);
    let WindowState::Gauge(g) = eval.state else {
        panic!("expected Gauge state")
    };
    assert_eq!(g.relief, Relief::ProviderDeclared(reset));
    let GaugeReading::Used { used, .. } = g.latest.unwrap().reading().clone() else {
        panic!("expected Used reading")
    };
    assert_eq!(
        used.value, 10_500,
        "overage must stay visible, never clamped at 10_000"
    );
}

// --- Fixture 16: unit isolation -----------------------------------------

#[test]
fn units_are_never_silently_mixed() {
    let w = window(
        QuotaUnit::Tokens,
        WindowKind::Sliding {
            length_secs: 3600,
            limit: 100,
        },
    );
    let now = datetime!(2026-10-08 12:00:00 UTC);
    let usd_spend = QuotaUsage::new(
        EconomicEventId::new(),
        now,
        QuotaAmount::new(QuotaUnit::UsdCents, 1_000_000),
    )
    .unwrap();
    let evidence = QuotaEvidence {
        usage: &[usd_spend],
        holds: &[],
        snapshots: &[],
    };
    let eval = w.evaluate(&evidence, now);
    assert_eq!(
        eval.blocking,
        BlockingStatus::NotBlocking,
        "a USD event must not count toward a Tokens window"
    );
}

#[test]
fn percent_unit_is_refused_on_a_non_gauge_window() {
    let err = QuotaWindow::validated(
        QuotaWindowId::new(),
        scope_at(datetime!(2026-01-01 0:00:00 UTC)),
        QuotaUnit::Percent,
        WindowKind::Sliding {
            length_secs: 3600,
            limit: 100,
        },
    )
    .unwrap_err();
    assert_eq!(err, QuotaWindowError::PercentOnNonGaugeWindow);
}

#[test]
fn distinct_opaque_credit_namespaces_are_distinct_units() {
    let a = QuotaUnit::OpaqueCredit(CreditNamespace::new("acme-credits").unwrap());
    let b = QuotaUnit::OpaqueCredit(CreditNamespace::new("other-credits").unwrap());
    assert_ne!(a, b);
}

// --- Fixture 17-18: holds counted apart from settled spend -------------

#[test]
fn outstanding_holds_are_counted_apart_from_settled_spend_and_do_not_age_out() {
    let w = window(
        QuotaUnit::Tokens,
        WindowKind::Sliding {
            length_secs: 60,
            limit: 100,
        },
    );
    let now = datetime!(2026-10-08 12:00:00 UTC);
    let hold =
        OutstandingHold::from_reservation(&fixture_reservation(60, ResState::Active)).unwrap();
    let evidence = QuotaEvidence {
        usage: &[],
        holds: &[hold],
        snapshots: &[],
    };
    let eval = w.evaluate(&evidence, now);
    let WindowState::Period(p) = eval.state else {
        panic!("expected Period state")
    };
    assert_eq!(p.outstanding, 60);
    assert_eq!(p.settled, 0);
    assert_eq!(p.remaining, 40);
}

#[test]
fn holds_alone_reaching_the_limit_gives_relief_after_holds_settle() {
    let w = window(
        QuotaUnit::Tokens,
        WindowKind::Sliding {
            length_secs: 60,
            limit: 100,
        },
    );
    let now = datetime!(2026-10-08 12:00:00 UTC);
    let hold =
        OutstandingHold::from_reservation(&fixture_reservation(100, ResState::Active)).unwrap();
    let evidence = QuotaEvidence {
        usage: &[],
        holds: &[hold],
        snapshots: &[],
    };
    let eval = w.evaluate(&evidence, now);
    assert_eq!(eval.blocking, BlockingStatus::Blocking);
    let WindowState::Period(p) = eval.state else {
        panic!("expected Period state")
    };
    assert_eq!(p.relief, Relief::AfterOutstandingHoldsSettle);
}

#[test]
fn a_hold_past_its_own_expires_at_still_counts_if_passed_in() {
    // Ledger parity: crates/ledger/src/reservation.rs's `reserve` query
    // sums `state = 'active'` with no `expires_at` filter — expiry is a
    // separate sweep (`expire_stale_reservations`). This contract must
    // not invent a second expiry rule: it counts whatever the caller
    // passes as `Active`, period.
    let reservation = fixture_reservation(30, ResState::Active); // expires_at is not modeled on OutstandingHold at all
    let hold = OutstandingHold::from_reservation(&reservation).unwrap();
    assert_eq!(hold.amount().value, 30);
}

#[test]
fn only_active_reservations_become_outstanding_holds() {
    assert!(
        OutstandingHold::from_reservation(&fixture_reservation(10, ResState::Settled)).is_none()
    );
    assert!(
        OutstandingHold::from_reservation(&fixture_reservation(10, ResState::Released)).is_none()
    );
    assert!(
        OutstandingHold::from_reservation(&fixture_reservation(10, ResState::Expired)).is_none()
    );
    assert!(
        OutstandingHold::from_reservation(&fixture_reservation(10, ResState::Active)).is_some()
    );
}

fn fixture_reservation(tokens: u64, state: ResState) -> Reservation {
    let task_id = crate::TaskId::new();
    let now = datetime!(2026-10-08 12:00:00 UTC);
    Reservation {
        id: ReservationId::new(),
        task_id,
        session_id: "session-1".to_string(),
        plan_id: None,
        class: ReservationClass::RequiredWork,
        amount: ResourceAmount::Tokens(tokens),
        drawn_from_reserve: ResourceAmount::Tokens(0),
        state,
        settled_amount: None,
        usage_known: None,
        idempotency_key: "fixture-key".to_string(),
        created_at: now,
        expires_at: now + time::Duration::seconds(3600),
        settled_at: None,
        released_at: None,
        account_id: crate::AccountId::for_task(task_id),
        grants_account: None,
        lease_kind: crate::LeaseKind::WorkHold,
        settled_after_expiry: false,
        legacy_pre_0011: false,
    }
}

// --- Fixture 19: from_economic_event mapping table ---------------------

fn test_attribution() -> EconomicAttribution {
    let identity = ExecutionIdentityBuilder::new("host-1", "claude_code")
        .provider_session_id("sess-1")
        .root_lineage()
        .build_at(datetime!(2026-10-08 12:00:00 UTC))
        .unwrap();
    EconomicAttribution::from_execution(Attributed::known(identity))
}

fn event_with_basis(basis: ResourceBasis) -> EconomicEvent {
    EconomicEvent::validated(
        EconomicEventId::new(),
        datetime!(2026-10-08 12:00:00 UTC),
        ResourceFact {
            amount: ResourceAmount::Tokens(10),
            basis,
        },
        test_attribution(),
        EconomicScope::Session,
    )
    .unwrap()
}

#[test]
fn from_economic_event_only_maps_additive_spend_facts() {
    assert!(QuotaUsage::from_economic_event(&event_with_basis(
        ResourceBasis::GatewayMeteredActual
    ))
    .is_some());
    assert!(QuotaUsage::from_economic_event(&event_with_basis(
        ResourceBasis::ProviderReportedActual
    ))
    .is_some());
    assert!(
        QuotaUsage::from_economic_event(&event_with_basis(ResourceBasis::QuotaSnapshot)).is_none()
    );
    assert!(QuotaUsage::from_economic_event(&event_with_basis(
        ResourceBasis::ImportedAllocationSnapshot
    ))
    .is_none());
    assert!(QuotaUsage::from_economic_event(&event_with_basis(
        ResourceBasis::LibraReservationHold
    ))
    .is_none());
    assert!(
        QuotaUsage::from_economic_event(&event_with_basis(ResourceBasis::LibraForecast)).is_none()
    );
    assert!(
        QuotaUsage::from_economic_event(&event_with_basis(ResourceBasis::HostEstimatedCost))
            .is_none()
    );
    assert!(
        matches!(ResourceBasis::GatewayMeteredActual.role(), FactRole::Spend),
        "sanity: Spend role exists on the basis this test exercises"
    );
}

// --- Fixture 20: dedup --------------------------------------------------

#[test]
fn duplicated_usage_id_counts_once() {
    let id = EconomicEventId::new();
    let now = datetime!(2026-10-08 12:00:00 UTC);
    let a = QuotaUsage::new(id, now, QuotaAmount::new(QuotaUnit::Tokens, 50)).unwrap();
    let b = QuotaUsage::new(id, now, QuotaAmount::new(QuotaUnit::Tokens, 50)).unwrap();
    let w = window(
        QuotaUnit::Tokens,
        WindowKind::Sliding {
            length_secs: 60,
            limit: 1000,
        },
    );
    let evidence = QuotaEvidence {
        usage: &[a, b],
        holds: &[],
        snapshots: &[],
    };
    let eval = w.evaluate(&evidence, now);
    let WindowState::Period(p) = eval.state else {
        panic!("expected Period state")
    };
    assert_eq!(p.settled, 50, "the duplicated receipt must count only once");
}

// --- Fixture 21: backward compatibility --------------------------------

#[test]
fn a_pre_existing_economic_event_still_deserializes_and_maps_through_from_economic_event() {
    // This module must not require any change to EconomicEvent's own
    // wire format (acceptance criterion 4). Build a real event through
    // its own public constructor — the authoritative source of its wire
    // shape — serialize it, and pin the exact field names/values this
    // module's `from_economic_event` depends on, so a future accidental
    // rename of any of them fails loudly here rather than silently.
    let event = event_with_basis(ResourceBasis::GatewayMeteredActual);
    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(json["fact"]["amount"]["kind"], "tokens");
    assert_eq!(json["fact"]["amount"]["amount"], 10);
    assert_eq!(json["fact"]["basis"], "gateway_metered_actual");
    assert_eq!(json["occurred_at"], "2026-10-08T12:00:00.000Z");

    let round_tripped: EconomicEvent =
        serde_json::from_value(json).expect("EconomicEvent's own wire format must still parse");
    let usage =
        QuotaUsage::from_economic_event(&round_tripped).expect("additive spend must map to Some");
    assert_eq!(usage.amount().value, 10);
    assert_eq!(usage.amount().unit, QuotaUnit::Tokens);
}

// --- Fixture 22: RefillBucket -------------------------------------------

fn bucket_window(
    capacity: u64,
    refill_amount: u64,
    refill_period_secs: u64,
    anchored_at: OffsetDateTime,
    level_at_anchor: u64,
) -> QuotaWindow {
    window(
        QuotaUnit::Tokens,
        WindowKind::RefillBucket {
            capacity,
            refill_amount,
            refill_period_secs,
            anchored_at,
            level_at_anchor,
        },
    )
}

#[test]
fn refill_bucket_partial_refill_is_exact() {
    let anchor = datetime!(2026-10-08 12:00:00 UTC);
    let w = bucket_window(100, 10, 1, anchor, 0);
    // No usage; 5 seconds pass -> +50, exact.
    let now = anchor + time::Duration::seconds(5);
    let eval = w.evaluate(&empty_evidence(), now);
    let WindowState::Bucket(b) = eval.state else {
        panic!("expected Bucket state")
    };
    assert_eq!(b.level, 50);
}

#[test]
fn refill_bucket_caps_at_capacity() {
    let anchor = datetime!(2026-10-08 12:00:00 UTC);
    let w = bucket_window(100, 10, 1, anchor, 90);
    let now = anchor + time::Duration::seconds(100);
    let eval = w.evaluate(&empty_evidence(), now);
    let WindowState::Bucket(b) = eval.state else {
        panic!("expected Bucket state")
    };
    assert_eq!(b.level, 100);
}

#[test]
fn refill_bucket_overdraft_shows_as_negative_level() {
    let anchor = datetime!(2026-10-08 12:00:00 UTC);
    let w = bucket_window(100, 1, 1, anchor, 10);
    let spend = usage(anchor, 50);
    let evidence = QuotaEvidence {
        usage: &[spend],
        holds: &[],
        snapshots: &[],
    };
    let now = anchor;
    let eval = w.evaluate(&evidence, now);
    let WindowState::Bucket(b) = eval.state else {
        panic!("expected Bucket state")
    };
    assert_eq!(b.level, -40);
    assert_eq!(eval.blocking, BlockingStatus::Blocking);
}

#[test]
fn refill_bucket_relief_is_deficit_divided_by_rate() {
    let anchor = datetime!(2026-10-08 12:00:00 UTC);
    let w = bucket_window(100, 10, 1, anchor, 0);
    let now = anchor;
    let eval = w.evaluate(&empty_evidence(), now);
    let WindowState::Bucket(b) = eval.state else {
        panic!("expected Bucket state")
    };
    assert_eq!(b.level, 0);
    assert_eq!(eval.blocking, BlockingStatus::Blocking);
    // Need level >= 1 (outstanding=0, so deficit = 1 - 0 = 1); rate=10/s -> 100ms.
    assert_eq!(
        b.relief,
        Relief::At(now + time::Duration::milliseconds(100))
    );
}

#[test]
fn refill_bucket_ignores_usage_before_the_anchor() {
    let anchor = datetime!(2026-10-08 12:00:00 UTC);
    let w = bucket_window(100, 1, 1_000_000, anchor, 100);
    let before_anchor = usage(anchor - time::Duration::seconds(10), 1_000_000);
    let evidence = QuotaEvidence {
        usage: &[before_anchor],
        holds: &[],
        snapshots: &[],
    };
    let eval = w.evaluate(&evidence, anchor);
    let WindowState::Bucket(b) = eval.state else {
        panic!("expected Bucket state")
    };
    assert_eq!(
        b.level, 100,
        "usage recorded before anchored_at must be ignored"
    );
}

// --- Fixture 23: determinism across evidence permutations --------------

#[test]
fn evaluation_is_deterministic_regardless_of_evidence_order() {
    let w = window(
        QuotaUnit::Tokens,
        WindowKind::Sliding {
            length_secs: 3600,
            limit: 1000,
        },
    );
    let now = datetime!(2026-10-08 12:00:00 UTC);
    let u1 = usage(now - time::Duration::seconds(10), 10);
    let u2 = usage(now - time::Duration::seconds(20), 20);
    let u3 = usage(now - time::Duration::seconds(30), 30);

    let order_a = QuotaEvidence {
        usage: &[u1.clone(), u2.clone(), u3.clone()],
        holds: &[],
        snapshots: &[],
    };
    let order_b = QuotaEvidence {
        usage: &[u3, u1, u2],
        holds: &[],
        snapshots: &[],
    };
    assert_eq!(w.evaluate(&order_a, now), w.evaluate(&order_b, now));
}

// --- Fixture 24: millisecond normalization round trip -------------------

#[test]
fn sub_millisecond_timestamps_normalize_identically_across_a_json_round_trip() {
    let sub_ms = datetime!(2026-10-08 12:00:00.123_456_789 UTC);
    let w = QuotaWindow::validated(
        QuotaWindowId::new(),
        scope_at(sub_ms),
        QuotaUnit::Tokens,
        WindowKind::Sliding {
            length_secs: 60,
            limit: 10,
        },
    )
    .unwrap();
    let json = serde_json::to_value(&w).unwrap();
    let round_tripped: QuotaWindow = serde_json::from_value(json).unwrap();
    assert_eq!(w, round_tripped);
    assert_eq!(w.scope().observed_at.millisecond(), 123);
}

// --- Fixture 25: versioning ---------------------------------------------

#[test]
fn an_unknown_schema_version_decodes_as_unsupported_not_an_error() {
    let raw = serde_json::json!({
        "schema_version": "quota-window-v99-from-the-future",
        "something": "else",
    });
    let decoded = decode_quota_window(&raw).unwrap();
    assert_eq!(
        decoded.blocking(),
        BlockingStatus::Indeterminate(IndeterminateReason::UnsupportedSchemaVersion)
    );
    let DecodedQuotaWindow::Unsupported { raw: kept, .. } = decoded else {
        panic!("expected Unsupported")
    };
    assert_eq!(
        kept, raw,
        "the unparsed payload must be kept for re-writing"
    );
}

#[test]
fn a_missing_schema_version_is_refused() {
    let raw = serde_json::json!({ "not_a_schema_version_field": true });
    let err = decode_quota_window(&raw).unwrap_err();
    assert_eq!(err, QuotaWindowError::MissingSchemaVersion);
}

#[test]
fn an_unknown_kind_or_unit_under_the_current_schema_is_refused_not_guessed() {
    let raw = serde_json::json!({
        "schema_version": QUOTA_WINDOW_SCHEMA_VERSION,
        "id": QuotaWindowId::new().0,
        "scope": scope_at(datetime!(2026-01-01 0:00:00 UTC)),
        "unit": { "unit": "some_future_unit" },
        "kind": { "kind": "sliding", "length_secs": 60, "limit": 10 },
    });
    assert!(decode_quota_window(&raw).is_err());
}

#[test]
fn a_current_version_window_round_trips_through_json() {
    let w = window(
        QuotaUnit::Tokens,
        WindowKind::Sliding {
            length_secs: 60,
            limit: 10,
        },
    );
    let json = serde_json::to_value(&w).unwrap();
    assert_eq!(json["schema_version"], QUOTA_WINDOW_SCHEMA_VERSION);
    let decoded = decode_quota_window(&json).unwrap();
    assert!(matches!(decoded, DecodedQuotaWindow::Current(_)));
}

// --- Fixture 26: unknown zone -------------------------------------------

#[test]
fn an_unrecognized_time_zone_name_is_refused_never_falls_back_to_utc() {
    assert!(IanaTimeZone::new("Mars/Olympus").is_err());

    let raw = serde_json::json!("Mars/Olympus");
    let decoded: Result<IanaTimeZone, _> = serde_json::from_value(raw);
    assert!(decoded.is_err());
}

// --- Fixture 27: BlockingStatus ordering ---------------------------------

#[test]
fn most_restrictive_orders_blocking_above_indeterminate_above_not_blocking() {
    use BlockingStatus::*;
    use IndeterminateReason::NoSnapshot;

    assert_eq!(Blocking.most_restrictive(NotBlocking), Blocking);
    assert_eq!(NotBlocking.most_restrictive(Blocking), Blocking);
    assert_eq!(
        Indeterminate(NoSnapshot).most_restrictive(NotBlocking),
        Indeterminate(NoSnapshot)
    );
    assert_eq!(
        Blocking.most_restrictive(Indeterminate(NoSnapshot)),
        Blocking
    );
    assert_eq!(NotBlocking.most_restrictive(NotBlocking), NotBlocking);
}

// --- Fixture 28: entitlement expiry --------------------------------------

#[test]
fn an_expired_entitlement_is_indeterminate_but_numbers_still_reported() {
    let now = datetime!(2026-10-08 12:00:00 UTC);
    let scope = QuotaScope {
        subject: QuotaSubject::SharedPool(PoolId("team-a".to_string())),
        source: EntitlementSource::OperatorConfigured,
        confidence: Confidence::High,
        observed_at: datetime!(2026-01-01 0:00:00 UTC),
        valid_until: Some(now - time::Duration::seconds(1)),
    };
    let w = QuotaWindow::validated(
        QuotaWindowId::new(),
        scope,
        QuotaUnit::Tokens,
        WindowKind::Sliding {
            length_secs: 60,
            limit: 100,
        },
    )
    .unwrap();
    let eval = w.evaluate(&empty_evidence(), now);
    assert_eq!(
        eval.blocking,
        BlockingStatus::Indeterminate(IndeterminateReason::EntitlementExpired)
    );
    let WindowState::Period(p) = eval.state else {
        panic!("expected Period state")
    };
    assert_eq!(p.remaining, 100, "the computed numbers are still present");
}
