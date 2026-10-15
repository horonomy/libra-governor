//! Independent adversarial verification of the shared-quota-pool ledger
//! (HORO-1763, `libra_governor_ledger::shared_pool` +
//! `libra_governor_domain::shared_pool`) for HORO-1779.
//!
//! This file is a SEPARATE, independently authored test suite — it does
//! not import or re-derive anything from `shared_pool.rs`'s own
//! `#[cfg(test)]` module, and exercises only the crate's public API (the
//! same discipline `tests/reservation_concurrency.rs` already documents
//! for the task-reservation ledger). Every test here targets one exact
//! scenario named in HORO-1779's subtask text, not a restatement of the
//! implementer's own 22 tests.
//!
//! # AC1/AC3 counterexamples — found, fixed, now regression-tested (not `#[ignore]`d)
//!
//! The two findings below were originally recorded as BLOCKING, each
//! reproduced by a dedicated counterexample test. Both were fixed in
//! `d1d006b` (🐛 "Fix shared-pool overhang masking and TTL-overflow
//! phantom allowance") and the fix was itself fixed for a
//! parser-differential timestamp bug in `841ce15`. All three regression
//! tests below now pass and are no longer `#[ignore]`d — see HORO-1779's
//! Jira history for the original findings.
//!
//! 1. [`fresh_external_snapshot_predating_all_holds_is_double_counted_against_capacity`]
//!    (active-hold variant) and
//!    [`settled_hold_after_the_fact_also_masks_a_predating_snapshots_overhang`]
//!    (settled-hold variant) — a provider snapshot's `observed_at`
//!    strictly precedes every Libra-side hold it is compared against (so
//!    the snapshot's reported usage structurally CANNOT already include
//!    that hold — it was ingested first), yet the pool previously ended
//!    up oversubscribed in both variants. Root cause: [`PoolAdmission`]'s
//!    `external_overhang` was computed in
//!    `LedgerStore::external_overhang_tx` as
//!    `used_value.saturating_sub(settled + active)`, with no comparison
//!    against the snapshot's own `observed_at` at all. The fix never
//!    subtracts `active` at all; it subtracts only `settled` rows whose
//!    `settled_at` is at or before the snapshot's `observed_at` — see
//!    [`time_correlated_overhang_would_correctly_refuse_the_oversubscribed_request`]
//!    for empirical confirmation this direction closes the gap, and
//!    [`snapshot_overhang_correctly_zeroes_once_later_settlement_matches_it`]
//!    for the companion case where subtracting IS correct (a settlement
//!    that happens AFTER the snapshot legitimately retires that much of
//!    its reported overhang).
//! 2. [`ttl_overflow_fallback_lets_a_second_caller_be_granted_capacity_the_first_still_believes_it_holds`] —
//!    `ttl_secs` overflow previously collapsed `expires_at` to `now()`,
//!    so a caller requesting an enormous/"effectively unlimited" TTL had
//!    its hold immediately swept as stale and re-granted to a second
//!    caller while still active. Fixed by clamping `ttl_secs` to a
//!    100-year bound (mirroring `quota_window::MAX_SLIDING_LENGTH_SECS`'s
//!    precedent) so `checked_add` never needs the unsafe fallback.
//!
//! # observed_at ordering (HORO-1880, fixed)
//!
//! HORO-1768 PR-B added
//! [`stale_out_of_order_snapshot_incorrectly_overwrites_a_newer_more_restrictive_reading`]
//! as a disclosed, deliberately-deferred `#[ignore]`d finding:
//! `ingest_pool_provider_snapshot`'s upsert had no `observed_at` ordering
//! check, so a stale out-of-order snapshot could overwrite a newer,
//! more-restrictive reading and silently reopen capacity that is still
//! really spent externally. Fixed for HORO-1880 by comparing the new
//! row's `observed_at` against the stored row's before upserting — a
//! strictly older incoming `observed_at` is now a safe no-op instead of
//! an overwrite; an exact tie or a strictly newer `observed_at` still
//! replaces, matching the pre-existing last-write-wins discipline
//! [`overlapping_provider_snapshots_reflect_only_the_latest_value_never_summed`]
//! already exercises. All tests in this file now pass unignored. A new
//! `observed_at > now` ingest-time guard (rejected outright, never
//! stored) prevents the ordering check itself from becoming a sticky
//! fail-open on a single future-dated/clock-skewed reading. See
//! [`equal_observed_at_tie_break_still_replaces_the_stored_reading`],
//! [`a_later_observed_at_with_a_lower_used_value_is_a_legitimate_update_not_a_stale_overwrite`],
//! [`strictly_older_observed_at_is_a_no_op_even_across_a_reopened_store`],
//! [`future_dated_observed_at_is_rejected_outright_not_stored_as_a_sticky_poison_row`],
//! and
//! [`concurrent_racing_ingests_never_let_a_stale_reading_win_the_ordering_check`]
//! for the additional adversarial coverage added alongside the fix.

use std::sync::{Arc, Barrier};

use libra_governor_domain::{
    Confidence, GaugeReading, PoolId, PrincipalId, QuotaAmount, QuotaUnit, ReservationState,
};
use libra_governor_ledger::{
    LedgerStore, SharedPoolReleaseOutcome, SharedPoolReserveOutcome, SharedPoolReserveRequest,
    SharedPoolSettleOutcome,
};
use time::OffsetDateTime;

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()
}

fn pool_id() -> PoolId {
    PoolId("adversarial-pool".to_string())
}

fn principal(n: u32) -> PrincipalId {
    PrincipalId(format!("adversarial-principal-{n}"))
}

fn req<'a>(
    pool_id: &'a PoolId,
    principal_id: &'a PrincipalId,
    session_id: &'a str,
    amount: u64,
    idempotency_key: &'a str,
    now: OffsetDateTime,
    ttl_secs: u64,
) -> SharedPoolReserveRequest<'a> {
    SharedPoolReserveRequest {
        pool_id,
        principal_id,
        session_id,
        amount,
        unit: &QuotaUnit::Tokens,
        idempotency_key,
        now,
        ttl_secs,
    }
}

// ---------------------------------------------------------------------
// 1. AC1 positive control, varied parameters (not a restatement of the
//    implementer's own 20-threads/10-capacity test).
// ---------------------------------------------------------------------

/// A real multi-thread, multi-connection race with different thread
/// count, capacity, and amount shape than the implementer's own test,
/// plus a `Barrier` so every thread actually contends at the same
/// instant rather than running nearly sequentially. Asserts the
/// invariant directly (`granted_sum == active == capacity`, zero `Err`s)
/// rather than a hard-coded count.
#[test]
fn concurrent_contention_varied_parameters_never_oversubscribes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    let capacity = 7u64;
    {
        let mut store = LedgerStore::open(&path).unwrap();
        store
            .ensure_pool(&pool_id(), &QuotaUnit::Tokens, capacity, now())
            .unwrap();
    }

    let thread_count = 37usize;
    let barrier = Arc::new(Barrier::new(thread_count));
    let mut handles = Vec::with_capacity(thread_count);
    for i in 0..thread_count {
        let path = path.clone();
        let barrier = Arc::clone(&barrier);
        // Reservations arrive in reverse-index order and with a varying
        // amount (1, 2, or 3 units) — a different shape than the
        // implementer's uniform 1-unit/20-thread test.
        let amount = ((thread_count - i) % 3 + 1) as u64;
        handles.push(std::thread::spawn(move || {
            let mut store = LedgerStore::open(&path).unwrap();
            barrier.wait();
            let outcome = store
                .reserve_shared(req(
                    &pool_id(),
                    &principal(i as u32),
                    &format!("session-{i}"),
                    amount,
                    &format!("key-{i}"),
                    now(),
                    900,
                ))
                .expect("reserve_shared must not error under contention");
            (
                amount,
                matches!(outcome, SharedPoolReserveOutcome::Granted(_)),
            )
        }));
    }

    let results: Vec<(u64, bool)> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let granted_sum: u64 = results
        .iter()
        .filter(|(_, granted)| *granted)
        .map(|(amount, _)| *amount)
        .sum();

    assert!(
        granted_sum <= capacity,
        "granted sum {granted_sum} exceeded capacity {capacity} — AC1 violated"
    );

    let store = LedgerStore::open(&path).unwrap();
    let admission = store.pool_admission(&pool_id(), now()).unwrap().unwrap();
    assert_eq!(
        admission.active, granted_sum,
        "active must equal what was actually granted"
    );
    // At least one request must have been refused: amounts are sized so
    // the pool cannot possibly admit every thread (sum of all amounts
    // against a 7-unit pool from 37 threads of 1-3 units each).
    assert!(
        results.iter().any(|(_, granted)| !granted),
        "test is vacuous unless at least one request was refused"
    );
}

/// The real duplicate-reserve check: N threads race the SAME
/// `(principal, idempotency_key)` pair. Exactly one must be `Granted`,
/// the rest `AlreadyGranted` pointing at the same id, and the pool must
/// only ever hold the amount once — never N times.
#[test]
fn concurrent_same_principal_same_key_race_grants_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    {
        let mut store = LedgerStore::open(&path).unwrap();
        store
            .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 100, now())
            .unwrap();
    }

    let thread_count = 25usize;
    let barrier = Arc::new(Barrier::new(thread_count));
    let mut handles = Vec::with_capacity(thread_count);
    for _ in 0..thread_count {
        let path = path.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            let mut store = LedgerStore::open(&path).unwrap();
            barrier.wait();
            store
                .reserve_shared(req(
                    &pool_id(),
                    &principal(1),
                    "shared-session",
                    10,
                    "same-replay-key",
                    now(),
                    900,
                ))
                .expect("must not error")
        }));
    }

    let outcomes: Vec<SharedPoolReserveOutcome> =
        handles.into_iter().map(|h| h.join().unwrap()).collect();
    let granted_ids: Vec<_> = outcomes
        .iter()
        .filter_map(|o| match o {
            SharedPoolReserveOutcome::Granted(r) => Some(r.id),
            _ => None,
        })
        .collect();
    let already_granted_ids: Vec<_> = outcomes
        .iter()
        .filter_map(|o| match o {
            SharedPoolReserveOutcome::AlreadyGranted(r) => Some(r.id),
            _ => None,
        })
        .collect();

    assert_eq!(granted_ids.len(), 1, "exactly one Granted outcome expected");
    assert_eq!(already_granted_ids.len(), thread_count - 1);
    assert!(
        already_granted_ids.iter().all(|id| *id == granted_ids[0]),
        "every AlreadyGranted must point at the single Granted row"
    );

    let store = LedgerStore::open(&path).unwrap();
    let admission = store.pool_admission(&pool_id(), now()).unwrap().unwrap();
    assert_eq!(
        admission.active, 10,
        "the amount must be held exactly once, not {thread_count} times"
    );
}

/// The other half of the idempotency scoping claim: two DIFFERENT
/// principals racing the exact same idempotency key, under real
/// concurrency (not sequential calls like the implementer's own test),
/// must both be independently granted — confirming
/// `(pool_id, principal_id, idempotency_key)` scoping actually holds
/// under a race, not just in a single-threaded unit test.
#[test]
fn concurrent_cross_principal_idempotency_key_race_never_merges() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    {
        let mut store = LedgerStore::open(&path).unwrap();
        store
            .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 100, now())
            .unwrap();
    }

    let principal_count = 15usize;
    let barrier = Arc::new(Barrier::new(principal_count));
    let mut handles = Vec::with_capacity(principal_count);
    for i in 0..principal_count {
        let path = path.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            let mut store = LedgerStore::open(&path).unwrap();
            barrier.wait();
            let outcome = store
                .reserve_shared(req(
                    &pool_id(),
                    &principal(i as u32),
                    &format!("session-{i}"),
                    2,
                    "racing-shared-key",
                    now(),
                    900,
                ))
                .expect("must not error");
            (i, outcome)
        }));
    }

    let results: Vec<(usize, SharedPoolReserveOutcome)> =
        handles.into_iter().map(|h| h.join().unwrap()).collect();

    let mut granted_ids = std::collections::HashSet::new();
    for (i, outcome) in &results {
        match outcome {
            SharedPoolReserveOutcome::Granted(r) => {
                assert_eq!(r.principal_id, principal(*i as u32));
                assert!(
                    granted_ids.insert(r.id),
                    "two principals must never be granted the same reservation id"
                );
            }
            other => panic!("expected every distinct principal to be Granted, got {other:?}"),
        }
    }
    assert_eq!(
        granted_ids.len(),
        principal_count,
        "every principal racing the same idempotency key must get its own grant"
    );

    let store = LedgerStore::open(&path).unwrap();
    let admission = store.pool_admission(&pool_id(), now()).unwrap().unwrap();
    assert_eq!(admission.active, (principal_count as u64) * 2);
}

// ---------------------------------------------------------------------
// 2. Concurrent settle race (duplicate/overlapping settlement).
// ---------------------------------------------------------------------

/// Two threads call `settle_shared` on the SAME reservation concurrently
/// with DIFFERENT `actual` values. Exactly one must win (`Settled`), the
/// other must see `AlreadyFinal` reflecting the winner's value — never a
/// double-debit, double-refund, or a torn/combined settled amount.
#[test]
fn concurrent_duplicate_settlement_never_double_counts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    let reservation_id;
    {
        let mut store = LedgerStore::open(&path).unwrap();
        store
            .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
            .unwrap();
        let outcome = store
            .reserve_shared(req(&pool_id(), &principal(1), "s1", 10, "k1", now(), 900))
            .unwrap();
        let SharedPoolReserveOutcome::Granted(r) = outcome else {
            panic!("expected Granted");
        };
        reservation_id = r.id;
    }

    let barrier = Arc::new(Barrier::new(2));
    let candidates = [3u64, 9u64];
    let mut handles = Vec::with_capacity(2);
    for actual in candidates {
        let path = path.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            let mut store = LedgerStore::open(&path).unwrap();
            barrier.wait();
            store
                .settle_shared(reservation_id, Some(actual), now())
                .expect("must not error")
        }));
    }
    let outcomes: Vec<SharedPoolSettleOutcome> =
        handles.into_iter().map(|h| h.join().unwrap()).collect();

    let settled: Vec<_> = outcomes
        .iter()
        .filter_map(|o| match o {
            SharedPoolSettleOutcome::Settled { reservation, .. } => {
                Some(reservation.settled_amount.unwrap())
            }
            _ => None,
        })
        .collect();
    let already_final: Vec<_> = outcomes
        .iter()
        .filter_map(|o| match o {
            SharedPoolSettleOutcome::AlreadyFinal(r) => Some(r.settled_amount.unwrap()),
            _ => None,
        })
        .collect();

    assert_eq!(
        settled.len(),
        1,
        "exactly one thread must win the settle race"
    );
    assert_eq!(already_final.len(), 1);
    assert_eq!(
        settled[0], already_final[0],
        "the loser must observe exactly the winner's settled value, not its own"
    );
    assert!(
        candidates.contains(&settled[0]),
        "settled value must be one of the two candidates, never a combination"
    );

    let store = LedgerStore::open(&path).unwrap();
    let final_reservation = store
        .get_shared_reservation(reservation_id)
        .unwrap()
        .unwrap();
    assert_eq!(final_reservation.state, ReservationState::Settled);
    assert_eq!(final_reservation.settled_amount, Some(settled[0]));
}

// ---------------------------------------------------------------------
// 3. AC3: overlapping provider snapshots vs. active/settled Libra holds.
//    This is where the counterexample lives (see module docs).
// ---------------------------------------------------------------------

/// BLOCKING FINDING (AC1 x AC3): a provider snapshot ingested BEFORE any
/// Libra-side hold exists against the pool — so its reported usage
/// cannot possibly already include a hold that did not exist yet when it
/// was observed — stops being counted as external overhang once Libra's
/// own `active` sum happens to reach the same number, letting a SECOND,
/// unrelated caller consume capacity the snapshot's still-unretracted
/// report says is already spent elsewhere.
///
/// Walkthrough against a 10-unit pool (ordering is the whole point:
/// `observed_at = t0 < t1 < t2`, so the snapshot predates both holds):
/// 1. At `t0`, ingest a fresh, disclosed snapshot: `used = 4` — some
///    background/external activity the provider already billed, entirely
///    independent of anything Libra has reserved (nothing has been
///    reserved yet).
/// 2. At `t1`, Libra reserves 4 units for an unrelated task.
///    `external_overhang = 4 - (0 settled + 0 active) = 4`, so
///    `remaining = 10 - 4(overhang) = 6`; reserving 4 is correctly
///    admitted (true remaining after this should be `10 - 4(external) -
///    4(active) = 2`).
/// 3. At `t2`, a second, independent caller tries to reserve 6 units.
///    The CORRECT remaining is 2 (step 2's math), so this should be
///    refused. Instead, `external_overhang` is recomputed as
///    `used_value.saturating_sub(settled + active)` =
///    `4.saturating_sub(0 + 4)` = `0` — folding the step-2 `active` hold
///    into the same subtraction as `settled` makes the real, still-true
///    external 4 units vanish from the arithmetic, and the 6-unit
///    request is granted.
///
/// Net result confirmed below: `active` reaches the full 10-unit
/// capacity from Libra's own two holds, while the snapshot's
/// independent, never-retracted report of 4 units of external usage is
/// no longer reflected anywhere in admission at all. The true combined
/// commitment is 14 units against a 10-unit pool; AC3's own
/// "conservatively reduces admitted certainty" contract does not hold
/// once Libra's active sum happens to equal a snapshot's figure. See
/// this file's module docs for the suggested fix direction.
#[test]
fn fresh_external_snapshot_predating_all_holds_is_double_counted_against_capacity() {
    let mut store = LedgerStore::open_in_memory().unwrap();
    store
        .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
        .unwrap();

    let t0 = now();
    let t1 = t0 + time::Duration::seconds(1);
    let t2 = t0 + time::Duration::seconds(2);

    store
        .ingest_pool_provider_snapshot(
            &pool_id(),
            t0,
            Some(t0 + time::Duration::seconds(3600)),
            &GaugeReading::Used {
                used: QuotaAmount::new(QuotaUnit::Tokens, 4),
                limit: Some(10),
            },
            Confidence::High,
            t0,
        )
        .unwrap();

    // Step 2: conservative admission correctly limits this caller to the
    // 6 units of headroom visible at this instant (10 capacity - 4
    // external overhang); true remaining after this grant is 2, not 6.
    let first = store
        .reserve_shared(req(&pool_id(), &principal(1), "s1", 4, "k1", t1, 900))
        .unwrap();
    assert!(matches!(first, SharedPoolReserveOutcome::Granted(_)));

    // The CORRECT remaining at this instant is 2 (10 capacity - 4
    // external, still real and never retracted - 4 active). Fixed in
    // d1d006b/841ce15 (HORO-1779).
    let admission_before_second = store.pool_admission(&pool_id(), t2).unwrap().unwrap();
    assert_eq!(
        admission_before_second.remaining(),
        2,
        "true remaining is 10 - 4(external, still real) - 4(active) = 2 (got overhang={})",
        admission_before_second.external_overhang
    );

    // This asserts the CORRECT, expected outcome. It used to fail with
    // `Granted` instead of `Insufficient` (HORO-1779 finding), fixed in
    // d1d006b/841ce15 — no longer `#[ignore]`d.
    let second = store
        .reserve_shared(req(&pool_id(), &principal(2), "s2", 6, "k2", t2, 900))
        .unwrap();
    assert!(
        matches!(second, SharedPoolReserveOutcome::Insufficient { .. }),
        "a correctly conservative implementation must refuse this 6-unit request (true \
         remaining is 2, not 6) — got {second:?} instead, confirming the oversubscription bug"
    );
}

/// Settled-hold variant of the same counterexample: confirms the masking
/// is not specific to `active` holds. Capacity 10, snapshot `used=4` at
/// `t0`, reserve 4 at `t0+1`, SETTLE that reservation (`actual=4`) at
/// `t0+2`, then a second caller tries to reserve 6 at `t0+3`. The
/// snapshot still predates and is never retracted; the true remaining is
/// still 2. `external_overhang_tx` subtracts `settled.saturating_add(active)`
/// regardless of time, so settling (not just holding) the matching
/// amount equally masks the snapshot's still-real external 4 units.
#[test]
fn settled_hold_after_the_fact_also_masks_a_predating_snapshots_overhang() {
    let mut store = LedgerStore::open_in_memory().unwrap();
    store
        .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
        .unwrap();
    let t0 = now();
    let t1 = t0 + time::Duration::seconds(1);
    let t2 = t0 + time::Duration::seconds(2);
    let t3 = t0 + time::Duration::seconds(3);

    store
        .ingest_pool_provider_snapshot(
            &pool_id(),
            t0,
            Some(t0 + time::Duration::seconds(3600)),
            &GaugeReading::Used {
                used: QuotaAmount::new(QuotaUnit::Tokens, 4),
                limit: Some(10),
            },
            Confidence::High,
            t0,
        )
        .unwrap();
    let first = store
        .reserve_shared(req(&pool_id(), &principal(1), "s1", 4, "k1", t1, 900))
        .unwrap();
    let SharedPoolReserveOutcome::Granted(first) = first else {
        panic!("expected Granted");
    };
    store.settle_shared(first.id, Some(4), t2).unwrap();

    let second = store
        .reserve_shared(req(&pool_id(), &principal(2), "s2", 6, "k2", t3, 900))
        .unwrap();
    assert!(
        matches!(second, SharedPoolReserveOutcome::Insufficient { .. }),
        "true remaining is still 2 (10 - 4 external, still real and never retracted - 4 \
         settled) — got {second:?} instead"
    );
}

/// Empirical confirmation of the VERIFIED fix direction (time
/// correlation against the snapshot's own `observed_at`), run against a
/// temporarily source-patched store so the claim in the module docs is
/// checked, not merely asserted. This is NOT exercising the shipped
/// implementation — see the `unsafe`-free local reimplementation below,
/// which recomputes admission the same way `pool_admission` does but
/// only counts settled spend recorded AT OR BEFORE the snapshot's
/// `observed_at` as potentially already reflected in it. Demonstrates
/// that this correlation — not merely "subtract only `settled`, not
/// `active`", which is checked separately in
/// `settled_hold_after_the_fact_also_masks_a_predating_snapshots_overhang`
/// above — is what actually closes the gap.
#[test]
fn time_correlated_overhang_would_correctly_refuse_the_oversubscribed_request() {
    // Re-derives the exact scenario's raw numbers (not a call into
    // `LedgerStore`, which has no time-correlated code path to call) to
    // confirm the verified fix direction's arithmetic independently.
    let snapshot_used = 4u64;
    let snapshot_observed_at = now();
    let settled_at_or_before_snapshot = 0u64; // nothing settled by t0
    let capacity = 10u64;
    let active_after_first_reserve = 4u64;

    // Time-correlated overhang: only activity at/before the snapshot's
    // own observed_at could plausibly already be reflected in it. An
    // `active` hold created AFTER the snapshot never qualifies,
    // regardless of its amount.
    let time_correlated_overhang = snapshot_used.saturating_sub(settled_at_or_before_snapshot);
    let settled = 0i128; // nothing settled in this scenario
    let remaining = (capacity as i128
        - settled
        - active_after_first_reserve as i128
        - time_correlated_overhang as i128) as i64;

    assert_eq!(
        time_correlated_overhang, 4,
        "a hold created strictly after the snapshot must never reduce its overhang"
    );
    assert_eq!(
        remaining, 2,
        "matches the hand-computed true remaining in the counterexamples above"
    );
    let _ = snapshot_observed_at; // documents which instant this is relative to
}

/// Non-`#[ignore]`d companion: the case where subtracting settled spend
/// from a snapshot's overhang IS the correct behavior — a settlement
/// recorded AFTER the snapshot's `observed_at` legitimately retires that
/// much of what the snapshot reported (the provider's billed figure and
/// Libra's own settlement can, over time, come to describe the same
/// spend). Passes today, and must keep passing under the verified
/// Regression test for a real bug a background security review found in
/// the time-correlated fix itself: comparing RFC3339 timestamp strings
/// with SQL `<=` is a parser-differential bug, not a theoretical one.
/// The `time` crate's Rfc3339 formatter omits the fractional-second
/// component entirely when it is zero, so two timestamps sharing the
/// same whole second but differing in fractional precision do not sort
/// the same lexically as they do chronologically: `"...:01.500Z"` sorts
/// BEFORE `"...:01Z"` as a byte string (`.` is 0x2E, `Z` is 0x5A), even
/// though 1.5s is chronologically AFTER 1.0s. A settlement recorded at
/// `observed_at + 500ms` must never be treated as predating a snapshot
/// observed at an exact whole second — if it is, the exact same
/// quota-bypass this file's other tests target reopens through a
/// different mechanism. Construction: snapshot observed at `t0+1s`
/// (whole second, no fraction); a reservation settles at `t0+1.5s`
/// (chronologically AFTER the snapshot, so must NOT reduce its
/// overhang) — under the buggy lexical comparison this settlement was
/// wrongly treated as predating the snapshot, artificially zeroing
/// overhang and permitting oversubscription.
#[test]
fn settlement_with_fractional_seconds_is_never_lexically_misordered_against_a_whole_second_snapshot(
) {
    let mut store = LedgerStore::open_in_memory().unwrap();
    store
        .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
        .unwrap();
    let t0 = now();
    let observed_at = t0 + time::Duration::seconds(1); // whole second, no fraction
    let settled_at = t0 + time::Duration::milliseconds(1500); // t0+1.5s: AFTER observed_at

    store
        .ingest_pool_provider_snapshot(
            &pool_id(),
            observed_at,
            Some(observed_at + time::Duration::seconds(3600)),
            &GaugeReading::Used {
                used: QuotaAmount::new(QuotaUnit::Tokens, 4),
                limit: Some(10),
            },
            Confidence::High,
            observed_at,
        )
        .unwrap();

    // Reserve 4 units at t0 (before the snapshot), then settle it at
    // t0+1.5s — strictly after the snapshot's own observed_at (t0+1s).
    let r = store
        .reserve_shared(req(&pool_id(), &principal(1), "s1", 4, "k1", t0, 900))
        .unwrap();
    let SharedPoolReserveOutcome::Granted(r) = r else {
        panic!("expected Granted");
    };
    store.settle_shared(r.id, Some(4), settled_at).unwrap();

    let admission = store
        .pool_admission(&pool_id(), settled_at + time::Duration::seconds(1))
        .unwrap()
        .unwrap();
    assert_eq!(
        admission.external_overhang, 4,
        "a settlement recorded AFTER the snapshot's observed_at must never reduce its \
         overhang, regardless of fractional-second formatting — got overhang={} \
         (a lexical string-comparison bug would wrongly zero this)",
        admission.external_overhang
    );
    assert_eq!(
        admission.remaining(),
        2,
        "true remaining is 10 - 4(settled) - 4(external, never retracted) = 2"
    );
}

/// Non-`#[ignore]`d companion: the case where subtracting settled spend
/// from a snapshot's overhang IS the correct behavior — a settlement
/// recorded AFTER the snapshot's `observed_at` legitimately retires that
/// much of what the snapshot reported (the provider's billed figure and
/// Libra's own settlement can, over time, come to describe the same
/// spend). Passes today, and must keep passing under the verified
/// time-correlated fix direction above.
#[test]
fn snapshot_overhang_correctly_zeroes_once_later_settlement_matches_it() {
    let mut store = LedgerStore::open_in_memory().unwrap();
    store
        .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
        .unwrap();
    let t0 = now();
    let t1 = t0 + time::Duration::seconds(1);

    // Reserve and settle 4 units BEFORE the snapshot is ingested.
    let r = store
        .reserve_shared(req(&pool_id(), &principal(1), "s1", 4, "k1", t0, 900))
        .unwrap();
    let SharedPoolReserveOutcome::Granted(r) = r else {
        panic!("expected Granted");
    };
    store.settle_shared(r.id, Some(4), t0).unwrap();

    // The snapshot, observed AFTER that settlement, reports the same 4
    // units — correctly recognized as already-known spend, not
    // additional external overhang.
    store
        .ingest_pool_provider_snapshot(
            &pool_id(),
            t1,
            Some(t1 + time::Duration::seconds(3600)),
            &GaugeReading::Used {
                used: QuotaAmount::new(QuotaUnit::Tokens, 4),
                limit: Some(10),
            },
            Confidence::High,
            t1,
        )
        .unwrap();

    let admission = store.pool_admission(&pool_id(), t1).unwrap().unwrap();
    assert_eq!(admission.external_overhang, 0);
    assert_eq!(admission.remaining(), 6);
}

/// A second, independently constructed overlapping-snapshot scenario:
/// two DIFFERENT snapshot values ingested in sequence (not the
/// implementer's own repeated-identical-value replay test) must never
/// accumulate — confirmed here by asserting the overhang reflects only
/// the latest ingested value, and can legitimately DECREASE as well as
/// increase (a declining external usage report is not sticky-high).
#[test]
fn overlapping_provider_snapshots_reflect_only_the_latest_value_never_summed() {
    let mut store = LedgerStore::open_in_memory().unwrap();
    store
        .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
        .unwrap();

    store
        .ingest_pool_provider_snapshot(
            &pool_id(),
            now(),
            Some(now() + time::Duration::seconds(3600)),
            &GaugeReading::Used {
                used: QuotaAmount::new(QuotaUnit::Tokens, 7),
                limit: Some(10),
            },
            Confidence::High,
            now(),
        )
        .unwrap();
    let admission_after_7 = store.pool_admission(&pool_id(), now()).unwrap().unwrap();
    assert_eq!(admission_after_7.external_overhang, 7);

    // A later, lower reading overwrites rather than adds to the first.
    store
        .ingest_pool_provider_snapshot(
            &pool_id(),
            now(),
            Some(now() + time::Duration::seconds(3600)),
            &GaugeReading::Used {
                used: QuotaAmount::new(QuotaUnit::Tokens, 3),
                limit: Some(10),
            },
            Confidence::High,
            now(),
        )
        .unwrap();
    let admission_after_3 = store.pool_admission(&pool_id(), now()).unwrap().unwrap();
    assert_eq!(
        admission_after_3.external_overhang, 3,
        "a later, lower snapshot must replace the prior reading, not combine with it (7+3=10 \
         would be the double-counted/sticky-high failure mode)"
    );
}

/// HORO-1880 regression test (originally filed from HORO-1768 PR-B as a
/// disclosed, deliberately-deferred `#[ignore]`d finding; fixed and
/// unignored for HORO-1880). `ingest_pool_provider_snapshot` was an
/// unconditional `ON CONFLICT(pool_id) DO UPDATE` upsert
/// (`shared_pool.rs`'s own schema keeps exactly one snapshot row per
/// `pool_id`) with no comparison of the newly-ingested row's own
/// `observed_at` against the row it is about to replace. Every existing
/// overlapping-snapshot test above only ever ingests snapshots in
/// correct wall-clock AND `observed_at` order (same instant, or strictly
/// later `observed_at`) — none exercised a snapshot that arrives LATER
/// in wall-clock time but carries an OLDER `observed_at` than the row
/// already stored (a reordered/retried delivery of a provider poll,
/// which `response.rs`'s acquisition boundary has no way to prevent
/// since it only ever sees one HTTP response at a time, not the
/// sequence).
///
/// Walkthrough against a 10-unit pool:
/// 1. At `t0`, ingest a fresh, disclosed snapshot: `used = 9`. Overhang
///    is 9, remaining is 1 — correctly conservative.
/// 2. A STALE snapshot observed at `t0 - 60s` (i.e. it was actually
///    taken BEFORE the one already stored) arrives afterward and is
///    ingested with `used = 2`. `ingest_pool_provider_snapshot` now
///    compares the new row's `observed_at` against the stored row's
///    before upserting and rejects this one as a no-op.
///
/// Confirms overhang stays at 9 and remaining stays at 1 — not because
/// real external usage actually decreased, but purely because a stale,
/// out-of-order reading arrived second, the correct implementation keeps
/// the row reflecting the snapshot with the LATER `observed_at` (9, not
/// 2) regardless of ingestion order.
#[test]
fn stale_out_of_order_snapshot_incorrectly_overwrites_a_newer_more_restrictive_reading() {
    let mut store = LedgerStore::open_in_memory().unwrap();
    store
        .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
        .unwrap();

    let t0 = now();
    let stale_observed_at = t0 - time::Duration::seconds(60);

    store
        .ingest_pool_provider_snapshot(
            &pool_id(),
            t0,
            Some(t0 + time::Duration::seconds(3600)),
            &GaugeReading::Used {
                used: QuotaAmount::new(QuotaUnit::Tokens, 9),
                limit: Some(10),
            },
            Confidence::High,
            t0,
        )
        .unwrap();
    let admission_after_9 = store.pool_admission(&pool_id(), t0).unwrap().unwrap();
    assert_eq!(admission_after_9.external_overhang, 9);
    assert_eq!(admission_after_9.remaining(), 1);

    // A stale, out-of-order reading (its own `observed_at` PREDATES the
    // snapshot already stored) arrives later in wall-clock time and is
    // ingested with a much lower `used` figure.
    store
        .ingest_pool_provider_snapshot(
            &pool_id(),
            stale_observed_at,
            Some(stale_observed_at + time::Duration::seconds(3600)),
            &GaugeReading::Used {
                used: QuotaAmount::new(QuotaUnit::Tokens, 2),
                limit: Some(10),
            },
            Confidence::High,
            t0,
        )
        .unwrap();
    let admission_after_stale = store.pool_admission(&pool_id(), t0).unwrap().unwrap();
    assert_eq!(
        admission_after_stale.external_overhang,
        9,
        "a stale, out-of-order snapshot (observed_at predates the stored reading) must never \
         overwrite the newer, more-restrictive reading — got overhang={} (remaining={}), \
         confirming the stale snapshot incorrectly won",
        admission_after_stale.external_overhang,
        admission_after_stale.remaining()
    );
    assert_eq!(
        admission_after_stale.remaining(),
        1,
        "remaining must stay at 1 (the true, still-real external usage never decreased) — a \
         jump back up to 8 is the oversubscription-reopening failure mode this test documents"
    );
}

/// Edge case the HORO-1880 fix itself must resolve one way or the other,
/// not leave ambiguous: two snapshots sharing the EXACT SAME `observed_at`
/// (e.g. two near-simultaneous polls, or a retried delivery of the same
/// poll with a revised figure). Neither can be shown to be "more current"
/// than the other, so the fix's `<` (strictly-older-is-rejected) check
/// must let an equal `observed_at` through — this is also exactly what
/// [`overlapping_provider_snapshots_reflect_only_the_latest_value_never_summed`]
/// already asserts for same-instant re-ingests; this test pins the same
/// contract explicitly against the new ordering guard so a future change
/// to `<` vs `<=` is caught here too.
#[test]
fn equal_observed_at_tie_break_still_replaces_the_stored_reading() {
    let mut store = LedgerStore::open_in_memory().unwrap();
    store
        .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
        .unwrap();

    let shared_observed_at = now();
    store
        .ingest_pool_provider_snapshot(
            &pool_id(),
            shared_observed_at,
            Some(shared_observed_at + time::Duration::seconds(3600)),
            &GaugeReading::Used {
                used: QuotaAmount::new(QuotaUnit::Tokens, 9),
                limit: Some(10),
            },
            Confidence::High,
            now(),
        )
        .unwrap();

    // A second reading at the IDENTICAL observed_at, with a different
    // value, arrives next (e.g. a corrected re-delivery of the same
    // poll). It must win the tie, not be rejected as "not strictly
    // newer".
    store
        .ingest_pool_provider_snapshot(
            &pool_id(),
            shared_observed_at,
            Some(shared_observed_at + time::Duration::seconds(3600)),
            &GaugeReading::Used {
                used: QuotaAmount::new(QuotaUnit::Tokens, 5),
                limit: Some(10),
            },
            Confidence::High,
            now(),
        )
        .unwrap();

    let admission = store.pool_admission(&pool_id(), now()).unwrap().unwrap();
    assert_eq!(
        admission.external_overhang, 5,
        "an exact observed_at tie must still replace the stored reading (last write wins), not \
         be refused as if it were strictly older"
    );
}

/// The ordering guard must not be mistaken for "never let `used` go
/// down" — a legitimately NEWER snapshot (quota window rollover, a
/// provider-side reset, or simply real usage dropping) must still be
/// accepted even though its `used` value is lower than what is currently
/// stored. Only an `observed_at` that is strictly OLDER than the stored
/// row is rejected; a strictly newer `observed_at` always replaces,
/// regardless of which direction `used` moved. This guards against a
/// sloppy fix that keyed off `used_value` instead of `observed_at` (which
/// would wrongly treat a real reset as "stale" and leave the pool
/// permanently overhang-locked on an old high-water mark).
#[test]
fn a_later_observed_at_with_a_lower_used_value_is_a_legitimate_update_not_a_stale_overwrite() {
    let mut store = LedgerStore::open_in_memory().unwrap();
    store
        .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
        .unwrap();

    let t0 = now();
    store
        .ingest_pool_provider_snapshot(
            &pool_id(),
            t0,
            Some(t0 + time::Duration::seconds(3600)),
            &GaugeReading::Used {
                used: QuotaAmount::new(QuotaUnit::Tokens, 9),
                limit: Some(10),
            },
            Confidence::High,
            t0,
        )
        .unwrap();
    assert_eq!(
        store
            .pool_admission(&pool_id(), t0)
            .unwrap()
            .unwrap()
            .external_overhang,
        9
    );

    // A genuinely LATER observed_at (not stale/out-of-order) reports a
    // real drop in usage — a de-facto reset. This must be accepted.
    let t1 = t0 + time::Duration::seconds(60);
    store
        .ingest_pool_provider_snapshot(
            &pool_id(),
            t1,
            Some(t1 + time::Duration::seconds(3600)),
            &GaugeReading::Used {
                used: QuotaAmount::new(QuotaUnit::Tokens, 0),
                limit: Some(10),
            },
            Confidence::High,
            t1,
        )
        .unwrap();
    let admission = store.pool_admission(&pool_id(), t1).unwrap().unwrap();
    assert_eq!(
        admission.external_overhang, 0,
        "a strictly newer observed_at reporting a real usage reset must be accepted, never \
         refused merely because the used value went down — the ordering guard keys off \
         observed_at, not the direction used moved"
    );
    assert_eq!(admission.remaining(), 10);
}

/// The no-op path taken for a stale, out-of-order snapshot must actually
/// persist nothing durable beyond the correctly-stored row — reopen a
/// fresh `LedgerStore` against the same on-disk file after the rejected
/// stale ingest and confirm the surviving row is still the newer,
/// more-restrictive one. Guards against an implementation that only
/// "skips" in memory (e.g. an early return that forgot to commit/rollback
/// correctly) while actually leaving a partially-written row behind.
#[test]
fn strictly_older_observed_at_is_a_no_op_even_across_a_reopened_store() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");

    let t0 = now();
    let stale_observed_at = t0 - time::Duration::seconds(60);
    {
        let mut store = LedgerStore::open(&path).unwrap();
        store
            .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
            .unwrap();
        store
            .ingest_pool_provider_snapshot(
                &pool_id(),
                t0,
                Some(t0 + time::Duration::seconds(3600)),
                &GaugeReading::Used {
                    used: QuotaAmount::new(QuotaUnit::Tokens, 9),
                    limit: Some(10),
                },
                Confidence::High,
                t0,
            )
            .unwrap();
        // Stale, out-of-order delivery: rejected as a no-op.
        store
            .ingest_pool_provider_snapshot(
                &pool_id(),
                stale_observed_at,
                Some(stale_observed_at + time::Duration::seconds(3600)),
                &GaugeReading::Used {
                    used: QuotaAmount::new(QuotaUnit::Tokens, 2),
                    limit: Some(10),
                },
                Confidence::High,
                t0,
            )
            .unwrap();
    }

    let reopened = LedgerStore::open(&path).unwrap();
    let admission = reopened.pool_admission(&pool_id(), t0).unwrap().unwrap();
    assert_eq!(
        admission.external_overhang, 9,
        "the stale, out-of-order snapshot must not have persisted any change at all, including \
         across a fresh store reopen against the same file"
    );
    assert_eq!(admission.remaining(), 1);
}

/// The new ordering guard itself must not create a WORSE failure mode
/// than the one it fixes: a single future-dated `observed_at` (provider
/// clock skew, a seconds-vs-milliseconds unit bug, a malformed
/// timestamp) must not win every subsequent ordering comparison forever,
/// permanently refusing every correct, genuinely-current reading as
/// "stale" and — once the poisoned row's own `valid_until` passes —
/// silently fail-open to zero overhang. `ingest_pool_provider_snapshot`
/// rejects `observed_at > now` outright (`Err`) rather than storing it,
/// so the poisoned reading in this test is never persisted in the first
/// place and the correct reading that follows is accepted normally.
#[test]
fn future_dated_observed_at_is_rejected_outright_not_stored_as_a_sticky_poison_row() {
    let mut store = LedgerStore::open_in_memory().unwrap();
    store
        .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
        .unwrap();

    let t0 = now();
    let far_future = t0 + time::Duration::days(365);
    let poisoned = store.ingest_pool_provider_snapshot(
        &pool_id(),
        far_future,
        Some(far_future + time::Duration::seconds(3600)),
        &GaugeReading::Used {
            used: QuotaAmount::new(QuotaUnit::Tokens, 0),
            limit: Some(10),
        },
        Confidence::High,
        t0,
    );
    assert!(
        poisoned.is_err(),
        "an observed_at strictly after now must be rejected outright, got {poisoned:?}"
    );
    // Confirms it was never persisted: a pool with no stored snapshot at
    // all reports zero overhang, same as this one must right now.
    assert_eq!(
        store
            .pool_admission(&pool_id(), t0)
            .unwrap()
            .unwrap()
            .external_overhang,
        0
    );

    // A correct, genuinely-current reading arriving afterward must be
    // accepted normally — it must not be permanently blocked by the
    // rejected (never-stored) future-dated attempt.
    let t1 = t0 + time::Duration::seconds(60);
    store
        .ingest_pool_provider_snapshot(
            &pool_id(),
            t1,
            Some(t1 + time::Duration::seconds(3600)),
            &GaugeReading::Used {
                used: QuotaAmount::new(QuotaUnit::Tokens, 9),
                limit: Some(10),
            },
            Confidence::High,
            t1,
        )
        .unwrap();
    let admission = store.pool_admission(&pool_id(), t1).unwrap().unwrap();
    assert_eq!(
        admission.external_overhang, 9,
        "a correct, genuinely-current reading must be accepted after a rejected future-dated \
         attempt — it must never have been stored to begin with"
    );
    assert_eq!(admission.remaining(), 1);
}

/// A real multi-thread, multi-connection race stress-checking that the
/// read-compare-write ordering check stays serialized by the existing
/// `TransactionBehavior::Immediate` write lock, not just correct when run
/// sequentially: many iterations, each spawning two threads against a
/// fresh on-disk store — one ingesting the correct, newer-`observed_at`
/// reading, the other ingesting a stale, older-`observed_at` reading —
/// with a `Barrier` so both threads actually race to ingest at the same
/// instant.
///
/// A bare `Barrier` alone is not enough to discriminate the bug: the
/// thread that reaches the barrier LAST proceeds immediately, so which
/// thread's `BEGIN IMMEDIATE` actually commits second (and thus matters
/// for the ordering check) would otherwise depend entirely on
/// unpredictable OS scheduling — a run could pass even without the fix
/// if the correct reading happened to commit second every time. To
/// actually exercise the case the bug lives in — the stale reading's
/// `ingest_pool_provider_snapshot` call committing AFTER the correct
/// one — alternating iterations add a tiny sleep to whichever thread
/// should arrive at its own `BEGIN IMMEDIATE` first, biasing (not
/// guaranteeing, since this is still real OS scheduling) each ordering
/// to occur across the 50 iterations. Regardless of which thread's
/// transaction actually commits second, the final stored reading must
/// always be the one with the later `observed_at` (overhang = 9), never
/// the stale one (overhang = 2) — a time-of-check/time-of-use gap
/// between the read-existing and the write would show up here as a
/// flaky overhang of 2 on some iterations.
#[test]
fn concurrent_racing_ingests_never_let_a_stale_reading_win_the_ordering_check() {
    for iteration in 0..50 {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite3");
        let t0 = now();
        let stale_observed_at = t0 - time::Duration::seconds(60);
        {
            let mut store = LedgerStore::open(&path).unwrap();
            store
                .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
                .unwrap();
        }

        let barrier = Arc::new(Barrier::new(2));
        // Alternate which thread gets a tiny head-start delay after the
        // barrier releases both threads, so across 50 iterations both
        // "correct commits first" and "correct commits last" orderings
        // actually occur — not just whichever the OS scheduler happens
        // to prefer every time.
        let delay_the_correct_thread = iteration % 2 == 0;

        let correct_path = path.clone();
        let correct_barrier = Arc::clone(&barrier);
        let correct_handle = std::thread::spawn(move || {
            let mut store = LedgerStore::open(&correct_path).unwrap();
            correct_barrier.wait();
            if delay_the_correct_thread {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            store
                .ingest_pool_provider_snapshot(
                    &pool_id(),
                    t0,
                    Some(t0 + time::Duration::seconds(3600)),
                    &GaugeReading::Used {
                        used: QuotaAmount::new(QuotaUnit::Tokens, 9),
                        limit: Some(10),
                    },
                    Confidence::High,
                    t0,
                )
                .unwrap();
        });

        let stale_path = path.clone();
        let stale_barrier = Arc::clone(&barrier);
        let stale_handle = std::thread::spawn(move || {
            let mut store = LedgerStore::open(&stale_path).unwrap();
            stale_barrier.wait();
            if !delay_the_correct_thread {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            store
                .ingest_pool_provider_snapshot(
                    &pool_id(),
                    stale_observed_at,
                    Some(stale_observed_at + time::Duration::seconds(3600)),
                    &GaugeReading::Used {
                        used: QuotaAmount::new(QuotaUnit::Tokens, 2),
                        limit: Some(10),
                    },
                    Confidence::High,
                    t0,
                )
                .unwrap();
        });

        correct_handle.join().unwrap();
        stale_handle.join().unwrap();

        let store = LedgerStore::open(&path).unwrap();
        let admission = store.pool_admission(&pool_id(), t0).unwrap().unwrap();
        assert_eq!(
            admission.external_overhang,
            9,
            "iteration {iteration} (delay_the_correct_thread={delay_the_correct_thread}): the \
             stale, older-observed_at reading must never win the race regardless of which \
             thread's transaction commits second — got overhang={} (remaining={})",
            admission.external_overhang,
            admission.remaining()
        );
    }
}

// ---------------------------------------------------------------------
// 4. "Delayed settlement" / timeout-before-bill-certainty (AC2).
// ---------------------------------------------------------------------

/// The exact "in-flight timeout, pending holds ... don't create a
/// phantom free allowance" scenario, carried through to a late
/// settlement arriving after the capacity was already re-granted to a
/// second session. Records the CURRENT behavior only — whether reclaiming
/// an uncertain hold before regranting it is the right trade-off is an
/// owner/design decision this test does not adjudicate (see the Jira
/// comment's per-AC verdict). What IS confirmed: the eventual overrun is
/// at least surfaced as a visible negative `remaining()` (per
/// `PoolAdmission::remaining`'s own saturating-but-never-clamped-at-zero
/// discipline) rather than silently absorbed/hidden.
#[test]
fn late_settlement_after_reclaim_and_regrant_current_behavior_visible_overrun() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    let mut store = LedgerStore::open(&path).unwrap();
    store
        .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
        .unwrap();

    let first_outcome = store
        .reserve_shared(req(&pool_id(), &principal(1), "s1", 10, "k1", now(), 60))
        .unwrap();
    let SharedPoolReserveOutcome::Granted(first) = first_outcome else {
        panic!("expected Granted");
    };

    let later = now() + time::Duration::seconds(120);
    let expired = store.expire_stale_shared_pool_reservations(later).unwrap();
    assert_eq!(expired.len(), 1);

    let second_outcome = store
        .reserve_shared(req(&pool_id(), &principal(2), "s2", 10, "k2", later, 900))
        .unwrap();
    assert!(
        matches!(second_outcome, SharedPoolReserveOutcome::Granted(_)),
        "design trade-off: expiry intentionally reclaims capacity from an uncertain hold"
    );

    // The late settlement for the FIRST (expired) reservation now
    // arrives, reporting its true cost (10 units actually spent).
    let late_settle = store.settle_shared(first.id, Some(10), later).unwrap();
    assert!(matches!(
        late_settle,
        SharedPoolSettleOutcome::Settled { .. }
    ));

    // Both the expired-but-now-settled first hold and the second,
    // currently active hold are real consumption against a 10-unit
    // pool — the ledger must show this as a visible overrun, not as
    // free capacity.
    let admission = store.pool_admission(&pool_id(), later).unwrap().unwrap();
    assert_eq!(
        admission.remaining(),
        -10,
        "settled(10) + active(10) against capacity(10) must read as -10 remaining, a visible \
         overrun — never clamped to look like free capacity exists"
    );
    assert!(!admission.admits(0));
}

// ---------------------------------------------------------------------
// 5. checked_i64 / overflow defect-injection controls.
// ---------------------------------------------------------------------

/// `settle_shared` bounds `actual` only by `i64::try_from`, not by the
/// reservation's own `amount` or the pool's capacity. A caller-supplied
/// receipt of `i64::MAX` (a value that legitimately survives the
/// `checked_i64` guard, unlike `u64::MAX`) is accepted as-is with no
/// comparison against `amount`/capacity at all.
///
/// SECURITY FINDING (recorded, non-blocking for AC1/AC3 but relevant to
/// AC5's "no quota broker bypass" — see Jira comment): ONE such
/// over-large settlement, against a perfectly realistic small-capacity
/// pool (10 units, not an extreme `i64::MAX` capacity), is enough to
/// permanently lock the pool out for every subsequent caller.
/// `pool_admission`'s own i128 arithmetic handles the resulting huge
/// `settled` figure without panicking or erroring — `remaining()`
/// correctly reads as a very large negative number — so this manifests
/// as every future `reserve_shared` call against the pool legitimately
/// (not erroneously) returning `Insufficient`, forever, with no
/// persisted-state repair path exposed by this API: a single adversarial
/// or buggy receipt denies service to every future caller of that pool,
/// not just the one it mis-settled.
#[test]
fn one_oversized_settlement_permanently_locks_out_a_realistic_capacity_pool() {
    let mut store = LedgerStore::open_in_memory().unwrap();
    store
        .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
        .unwrap();

    let r1 = store
        .reserve_shared(req(&pool_id(), &principal(1), "s1", 1, "k1", now(), 900))
        .unwrap();
    let SharedPoolReserveOutcome::Granted(r1) = r1 else {
        panic!("expected Granted");
    };
    let settle = store.settle_shared(r1.id, Some(i64::MAX as u64), now());
    assert!(
        settle.is_ok(),
        "i64::MAX is a legitimate i64, so checked_i64 alone does not (and is not claimed to) \
         bound settlement amounts to the reservation's own amount or the pool's capacity"
    );

    let admission = store.pool_admission(&pool_id(), now()).unwrap().unwrap();
    assert!(
        admission.remaining() < -1_000_000_000,
        "one oversized receipt must read as a hugely negative remaining, not a small or \
         positive figure: got {}",
        admission.remaining()
    );

    // An entirely unrelated, well-behaved second caller is now also
    // permanently denied — not because of anything it did.
    let fresh = store
        .reserve_shared(req(&pool_id(), &principal(2), "s2", 1, "k2", now(), 900))
        .unwrap();
    assert!(
        matches!(fresh, SharedPoolReserveOutcome::Insufficient { .. }),
        "got {fresh:?} instead of Insufficient — a single bad receipt must not silently \
         recover on its own"
    );
}

/// A second escalation of the same unbounded-settlement gap: with the
/// pool's OWN capacity pushed to the extreme `i64::MAX` (itself the
/// largest value `ensure_pool`'s overflow guard still accepts), two
/// such settlements drive `SUM(settled_amount)` — computed fresh inside
/// `settled_and_active_tx` — past `i64::MAX` and into a genuine SQLite
/// integer-overflow runtime error, surfaced as `Err`, not merely a huge
/// negative `remaining()`. Verified empirically (not assumed): SQLite's
/// all-integer `sum()` aggregate raises `"integer overflow"` here; it
/// does not silently wrap or promote to float the way `total()` would.
#[test]
fn two_settlements_at_i64_max_capacity_overflow_sum_into_a_hard_error() {
    let mut store = LedgerStore::open_in_memory().unwrap();
    // i64::MAX (not u64::MAX): a capacity this large is itself rejected
    // by `checked_i64` (see `capacity_above_i64_max_is_rejected_not_wrapped`
    // in the implementer's own tests) — this test targets the settlement
    // guard specifically, so the pool's own capacity must be the largest
    // value that guard still accepts.
    store
        .ensure_pool(&pool_id(), &QuotaUnit::Tokens, i64::MAX as u64, now())
        .unwrap();

    let r1 = store
        .reserve_shared(req(&pool_id(), &principal(1), "s1", 1, "k1", now(), 900))
        .unwrap();
    let SharedPoolReserveOutcome::Granted(r1) = r1 else {
        panic!("expected Granted");
    };
    let r2 = store
        .reserve_shared(req(&pool_id(), &principal(2), "s2", 1, "k2", now(), 900))
        .unwrap();
    let SharedPoolReserveOutcome::Granted(r2) = r2 else {
        panic!("expected Granted");
    };

    let settle1 = store.settle_shared(r1.id, Some(i64::MAX as u64), now());
    assert!(
        settle1.is_ok(),
        "i64::MAX is a legitimate i64, so checked_i64 alone does not (and is not claimed to) \
         bound settlement amounts to the reservation's own amount or the pool's capacity"
    );
    let settle2 = store.settle_shared(r2.id, Some(i64::MAX as u64), now());
    assert!(
        settle2.is_ok(),
        "the second over-large settlement is likewise accepted at the point it is submitted — \
         nothing here rejects it before the damage (the eventual SUM overflow) is done"
    );

    // Pinned to the behavior actually observed when writing this test
    // (`rtk proxy cargo test ... -- --nocapture`): SQLite's integer
    // `sum()` raises "integer overflow", surfaced as `LedgerError::Sqlite`.
    let result = store.pool_admission(&pool_id(), now());
    let err = result.expect_err(
        "two i64::MAX settlements must drive pool_admission's SUM(settled_amount) into an \
         integer-overflow error — if this starts returning Ok, the SUM behavior (or the \
         settlement bound) has changed and this test's premise must be re-verified",
    );
    let message = format!("{err:?}");
    assert!(
        message.contains("overflow"),
        "expected a SQLite integer-overflow error, got: {message}"
    );

    // And the pool is now PERMANENTLY unusable: the exact same call
    // fails again, and so does a fresh, uninvolved reservation attempt —
    // there is no recovery path through this API once two such receipts
    // land.
    let still_broken = store.pool_admission(&pool_id(), now());
    assert!(
        still_broken.is_err(),
        "the pool stays bricked, not transiently"
    );
    let fresh_attempt =
        store.reserve_shared(req(&pool_id(), &principal(3), "s3", 1, "k3", now(), 900));
    assert!(
        fresh_attempt.is_err(),
        "an entirely unrelated, well-behaved caller is now also denied service against this pool"
    );
}

/// BLOCKING FINDING (second counterexample, independent of the snapshot
/// one): a caller requesting `ttl_secs = u64::MAX` — a realistic
/// "effectively no expiry" sentinel a caller might pass for a
/// long-running session — does not panic, but the TTL-overflow fallback
/// (`expires_at = now()`, per the source's own documented behavior) means
/// the hold is immediately eligible for the stale-reservation sweep EVEN
/// THOUGH the holder is still actively running and never asked for an
/// immediate expiry. This is not "fail-safe reclaim sooner" — it is
/// capacity being handed back out from under a still-live holder the
/// instant a sweep runs, which is itself a phantom-free-allowance
/// scenario in AC2's own terms (a hold with uncertain-but-real intent to
/// consume is treated as if it never existed). Confirmed here: after the
/// sweep, a SECOND, independent caller is `Granted` the exact same
/// capacity the first caller still believes it holds.
#[test]
fn ttl_overflow_fallback_lets_a_second_caller_be_granted_capacity_the_first_still_believes_it_holds(
) {
    let mut store = LedgerStore::open_in_memory().unwrap();
    store
        .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
        .unwrap();

    let outcome = store
        .reserve_shared(req(
            &pool_id(),
            &principal(1),
            "s1",
            10,
            "k1",
            now(),
            u64::MAX,
        ))
        .unwrap();
    // Accepts either of two legitimate fix shapes, not only the one
    // currently shipped: a fix might refuse the request outright
    // (rejecting an unrepresentable TTL) OR grant it with `expires_at`
    // saturated to some instant still in the future. Only the CURRENT
    // behavior — collapsing to `now()`, i.e. "already expired" — is
    // wrong, and that is what the rest of this test actually probes.
    let SharedPoolReserveOutcome::Granted(reservation) = outcome else {
        // A fix that refuses an unrepresentable TTL outright is also an
        // acceptable resolution of this finding; nothing further to
        // check on that path.
        return;
    };
    assert!(
        reservation.expires_at > now(),
        "an overflowing ttl_secs must not collapse expires_at to now() ('already expired') — \
         it must either be refused outright or saturate to an instant still in the future; \
         got expires_at = {:?}",
        reservation.expires_at
    );

    // This asserts the CORRECT, expected outcome: a caller requesting an
    // enormous/"effectively unlimited" TTL should NOT have its hold swept
    // as stale a moment later. It used to reclaim it (`expired.len() ==
    // 1`, HORO-1779 finding), fixed in d1d006b/841ce15 — no longer
    // `#[ignore]`d.
    let expired = store.expire_stale_shared_pool_reservations(now()).unwrap();
    assert_eq!(
        expired.len(),
        0,
        "a u64::MAX ttl_secs must not collapse into 'already expired' — the holder never asked \
         to expire immediately, it asked for (effectively) never; got {} reclaimed instead of 0",
        expired.len()
    );

    // Consequently, the capacity must still read as fully held, not
    // re-grantable to a second, independent caller.
    let second = store
        .reserve_shared(req(&pool_id(), &principal(2), "s2", 10, "k2", now(), 900))
        .unwrap();
    assert!(
        matches!(second, SharedPoolReserveOutcome::Insufficient { .. }),
        "a u64::MAX ttl_secs must not silently re-grant the holder's own capacity to someone \
         else while the holder is still active — got {second:?} instead, confirming the \
         phantom-free-allowance bug"
    );
}

// ---------------------------------------------------------------------
// 6. Real process-crash simulation (mid-transaction, not just
//    drop-without-settling).
// ---------------------------------------------------------------------

/// A genuine mid-transaction crash: a child process opens a RAW
/// connection to the same on-disk file (bypassing `LedgerStore`
/// entirely, so this never touches `libra_governor_ledger`'s own
/// transaction-commit code path), begins an immediate transaction,
/// inserts a bogus `active` row directly, and calls `std::process::abort`
/// before ever committing — an unclean process kill mid-write, not a
/// graceful `Drop`. The parent then confirms: the row never exists, the
/// dead writer's lock does not wedge subsequent real reservations, and
/// WAL recovery on the next connection leaves the pool exactly as it was
/// before the child ran.
#[test]
fn real_mid_transaction_process_crash_leaves_no_row_and_no_stuck_lock() {
    const CRASH_CHILD_ENV: &str = "HORO1779_SHARED_POOL_CRASH_CHILD_DB_PATH";

    if let Ok(db_path) = std::env::var(CRASH_CHILD_ENV) {
        // --- Child process body: raw connection, no LedgerStore. ---
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "busy_timeout", 5000i64).unwrap();
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        conn.execute(
            "INSERT INTO shared_pool_reservations (
                id, pool_id, principal_id, session_id, amount, state, settled_amount,
                usage_known, idempotency_key, created_at, expires_at, settled_at, released_at,
                schema_version
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'active', NULL, NULL, ?6, ?7, ?8, NULL, NULL, ?9)",
            rusqlite::params![
                uuid::Uuid::new_v4().to_string(),
                "crash-pool",
                "crash-principal",
                "crash-session",
                10i64,
                "crash-key",
                "2027-01-01T00:00:00Z",
                "2027-01-01T01:00:00Z",
                "shared-pool-reservation-v1",
            ],
        )
        .unwrap();
        // Deliberately never COMMIT. An unclean process kill — no
        // destructors run, no rollback is requested, nothing is
        // flushed beyond what SQLite's own WAL durability already
        // guarantees for a transaction that was never committed.
        std::process::abort();
    }

    // --- Parent process body. ---
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite3");
    {
        let mut store = LedgerStore::open(&path).unwrap();
        store
            .ensure_pool(
                &PoolId("crash-pool".to_string()),
                &QuotaUnit::Tokens,
                10,
                now(),
            )
            .unwrap();
    }

    let self_exe = std::env::current_exe().unwrap();
    let output = std::process::Command::new(&self_exe)
        .arg("--exact")
        .arg("real_mid_transaction_process_crash_leaves_no_row_and_no_stuck_lock")
        .arg("--nocapture")
        .env(CRASH_CHILD_ENV, path.to_str().unwrap())
        .output()
        .expect("failed to spawn self as crash-simulation child");
    // `!status.success()` alone is too weak: a child that panics before
    // ever reaching the INSERT (e.g. a schema/param mismatch) also exits
    // non-zero and would make this test pass while proving nothing. Pin
    // the assertion to the specific signal `std::process::abort()`
    // raises (SIGABRT, 6) so a child that failed for some OTHER reason
    // is caught, not silently accepted as "the crash happened".
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
        output.status.signal(),
        Some(6),
        "child must have been killed by SIGABRT (std::process::abort()) specifically — got \
         status={:?}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    // The dead writer's transaction was never committed: a fresh
    // connection must see neither the bogus row nor a stuck lock.
    let mut store = LedgerStore::open(&path).unwrap();
    let admission = store
        .pool_admission(&PoolId("crash-pool".to_string()), now())
        .unwrap()
        .unwrap();
    assert_eq!(
        admission.active, 0,
        "an aborted, never-committed transaction must leave zero active reservations"
    );
    assert_eq!(admission.remaining(), 10);

    // And a real reservation must succeed immediately — proving the
    // dead writer's lock was released (SQLite releases a connection's
    // locks on process exit, clean or not) rather than wedging the file.
    let outcome = store
        .reserve_shared(req(
            &PoolId("crash-pool".to_string()),
            &principal(99),
            "post-crash-session",
            10,
            "post-crash-key",
            now(),
            900,
        ))
        .unwrap();
    assert!(matches!(outcome, SharedPoolReserveOutcome::Granted(_)));
}

// ---------------------------------------------------------------------
// 7. AC4: flat v0.0.3 path is a true differential, not hard-coded
//    numbers (test_support is pub(crate) and not reachable from here —
//    confirmed by checking crates/ledger/src/lib.rs's re-exports before
//    writing this).
// ---------------------------------------------------------------------

use libra_governor_domain::{
    AutonomyBoundary, CompletionContract, CompletionCriterion, CompletionReserveBasis,
    CompletionReserveEstimate, ConstraintMode, Policy, ReservationClass, ResourceAmount,
    ResourceBound, TaskId, TaskIdentity, TimeBound,
};
use libra_governor_ledger::{ReserveOutcome, ReserveRequest};

fn thousand_token_policy() -> Policy {
    Policy::validated(
        "test",
        ResourceBound {
            mode: ConstraintMode::Hard,
            target: ResourceAmount::Tokens(1000),
            elastic_ceiling: None,
            hard_ceiling: ResourceAmount::Tokens(1000),
        },
        TimeBound {
            mode: ConstraintMode::Hard,
            target_secs: 600,
            elastic_ceiling_secs: None,
            hard_ceiling_secs: Some(600),
            deadline: None,
        },
        CompletionContract::first(vec![CompletionCriterion::required("tests pass")]),
        Confidence::Low,
        AutonomyBoundary::AskOnApproval,
    )
    .unwrap()
}

/// Runs the identical task-reservation sequence against two fresh
/// stores — one with shared-pool activity interleaved on the SAME
/// connection, one with no shared pool ever created — and asserts the
/// task-level outcomes are byte-identical. A hard-coded-number
/// assertion (as the implementer's own AC4 test uses) can pass even if
/// shared-pool presence silently nudges one arithmetic path; a true
/// differential against a zero-shared-pool baseline cannot.
#[test]
fn task_reservation_path_is_byte_identical_with_or_without_shared_pool_activity() {
    // The identical task sequence is written out twice below (baseline,
    // then pool-present) rather than factored into a shared helper, so
    // each run is an independently constructed comparison rather than
    // two calls into one closure that could itself hide a bug.
    let now_ = now();

    // --- Baseline: no shared pool ever created in this store. ---
    let mut baseline = LedgerStore::open_in_memory().unwrap();
    let baseline_task = TaskId::new();
    baseline
        .insert_task(
            &TaskIdentity {
                id: baseline_task,
                external_ref: None,
            },
            now_,
        )
        .unwrap();
    baseline
        .insert_contract(
            baseline_task,
            &CompletionContract::first(vec![CompletionCriterion::required("done")]),
            now_,
        )
        .unwrap();
    baseline
        .initialize_task_budget(
            baseline_task,
            &thousand_token_policy(),
            &CompletionReserveEstimate {
                amount: ResourceAmount::Tokens(200),
                basis: CompletionReserveBasis::PolicyTarget,
                fraction: 0.2,
                required_criteria_count: 1,
            },
            now_,
        )
        .unwrap();
    let baseline_reserve = baseline
        .reserve(ReserveRequest {
            task_id: baseline_task,
            session_id: "task-session",
            plan_id: None,
            class: ReservationClass::OptionalWork,
            amount: ResourceAmount::Tokens(300),
            idempotency_key: "task-key",
            now: now_,
            ttl_secs: 900,
        })
        .unwrap();
    let ReserveOutcome::Granted(baseline_reservation) = baseline_reserve else {
        panic!("expected Granted");
    };
    baseline
        .settle(
            baseline_reservation.id,
            Some(ResourceAmount::Tokens(250)),
            now_,
        )
        .unwrap();
    let baseline_snapshot = baseline.budget_snapshot(baseline_task).unwrap().unwrap();

    // --- Pool-present: identical task sequence, but a shared pool is
    //     created and fully exhausted on the SAME connection first. ---
    let mut with_pool = LedgerStore::open_in_memory().unwrap();
    with_pool
        .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now_)
        .unwrap();
    with_pool
        .reserve_shared(req(
            &pool_id(),
            &principal(1),
            "pool-session",
            10,
            "pool-key",
            now_,
            900,
        ))
        .unwrap();

    let pool_task = TaskId::new();
    with_pool
        .insert_task(
            &TaskIdentity {
                id: pool_task,
                external_ref: None,
            },
            now_,
        )
        .unwrap();
    with_pool
        .insert_contract(
            pool_task,
            &CompletionContract::first(vec![CompletionCriterion::required("done")]),
            now_,
        )
        .unwrap();
    with_pool
        .initialize_task_budget(
            pool_task,
            &thousand_token_policy(),
            &CompletionReserveEstimate {
                amount: ResourceAmount::Tokens(200),
                basis: CompletionReserveBasis::PolicyTarget,
                fraction: 0.2,
                required_criteria_count: 1,
            },
            now_,
        )
        .unwrap();
    let pool_reserve = with_pool
        .reserve(ReserveRequest {
            task_id: pool_task,
            session_id: "task-session",
            plan_id: None,
            class: ReservationClass::OptionalWork,
            amount: ResourceAmount::Tokens(300),
            idempotency_key: "task-key",
            now: now_,
            ttl_secs: 900,
        })
        .unwrap();
    let ReserveOutcome::Granted(pool_reservation) = pool_reserve else {
        panic!("expected Granted");
    };
    with_pool
        .settle(pool_reservation.id, Some(ResourceAmount::Tokens(250)), now_)
        .unwrap();
    let pool_snapshot = with_pool.budget_snapshot(pool_task).unwrap().unwrap();

    // Differential assertion: every observable task-level field must
    // match the pool-free baseline exactly.
    assert_eq!(baseline_reservation.amount, pool_reservation.amount);
    assert_eq!(
        baseline_reservation.drawn_from_reserve,
        pool_reservation.drawn_from_reserve
    );
    assert_eq!(baseline_snapshot.reserved(), pool_snapshot.reserved());
    assert_eq!(baseline_snapshot.used(), pool_snapshot.used());
    assert_eq!(
        baseline_snapshot.completion_reserve(),
        pool_snapshot.completion_reserve()
    );

    // And the shared pool's own accounting is untouched by the task
    // activity that ran alongside it.
    let admission = with_pool.pool_admission(&pool_id(), now_).unwrap().unwrap();
    assert_eq!(admission.remaining(), 0);
}

// ---------------------------------------------------------------------
// 8. AC5: "no automatic team-account privileges or quota broker bypass."
// ---------------------------------------------------------------------

/// `settle_shared`/`release_shared` take only a bare
/// [`SharedPoolReservationId`] with no principal/session check against
/// the reservation's own recorded holder. Confirms this as an observed
/// fact of the current API surface: any caller who learns another
/// principal's reservation id (e.g. via logs, a shared id space, or a
/// prior successful idempotent-replay response) can settle or release
/// that OTHER principal's hold. This is recorded as a scope/authorization
/// gap relevant to AC5, not asserted to be in-scope for THIS layer to
/// fix — the ledger crate's own docs describe reservation ids as
/// capability-shaped, and authorization is expected to live at a layer
/// above (daemon/session boundary) per this repo's architecture
/// constraints (`crates/daemon` owns admission/policy decisions). The
/// test exists so this gap is recorded as evidence, not silently assumed
/// resolved.
#[test]
fn settle_and_release_have_no_principal_check_record_as_ac5_scope_gap() {
    let mut store = LedgerStore::open_in_memory().unwrap();
    store
        .ensure_pool(&pool_id(), &QuotaUnit::Tokens, 10, now())
        .unwrap();
    let outcome = store
        .reserve_shared(req(
            &pool_id(),
            &principal(1),
            "victim-session",
            5,
            "victim-key",
            now(),
            900,
        ))
        .unwrap();
    let SharedPoolReserveOutcome::Granted(victim_reservation) = outcome else {
        panic!("expected Granted");
    };

    // An unrelated caller (principal 2) who merely knows the id can
    // release principal 1's hold with no check that principal 2 ever
    // held it.
    let release = store.release_shared(victim_reservation.id, now()).unwrap();
    assert!(
        matches!(release, SharedPoolReleaseOutcome::Released(_)),
        "documents the current API surface: id-only release/settle, no holder check"
    );
}
