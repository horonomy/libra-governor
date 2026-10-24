//! Metered shared-pool RPC handlers (HORO-1915): `reserve`/`settle`/
//! `release`/`status` over a provider-denominated shared pool, structurally
//! independent of [`crate::launch_gate`]'s opaque launch-slot pools.
//!
//! # No production feed in this phase
//!
//! Deliberately, nothing in this module ever calls `ensure_pool` or
//! `ingest_pool_provider_snapshot` — every request against an
//! unprovisioned metered pool refuses with
//! [`libra_governor_protocol::MeteredRefusal::NoPool`]. Provisioning and
//! feeding a metered pool today is exclusively a test-owned synthetic
//! fixture (`crates/cli/tests/metered_pool_fixture`); see
//! `docs/adr/0023-metered-shared-pool.md` for why wiring a real provider
//! observation into this admission path is a separate, not-yet-authorized
//! trust-boundary decision (HORO-1915 Jira comment 27743).
use time::OffsetDateTime;

use libra_governor_domain::{
    is_metered_unit, MeteredPoolKey, PrincipalId, QuotaUnit, ReservationState,
    SharedPoolReservationId,
};
use libra_governor_ledger::{
    LedgerError, LedgerStore, MeteredReserveOutcome, MeteredReserveRequest,
    SharedPoolReleaseOutcome, SharedPoolReserveRequest, SharedPoolSettleOutcome,
};
use libra_governor_protocol::{
    MeteredPoolOutcome, MeteredPoolSessionUsage, MeteredPoolSnapshotStatus, MeteredPoolStatus,
    MeteredRefusal, METERED_POOL_ENFORCEMENT_SCOPE,
};

/// Default hold TTL for a metered reservation — generous enough to cover
/// one real step of agent work without requiring a renew primitive (there
/// is none for metered holds; see module docs on why a late settle after
/// expiry is still the correct, fail-closed outcome rather than a reason
/// to add one).
pub const DEFAULT_METERED_HOLD_TTL_SECS: u64 = 3600;

/// A session id naming the reserved execution-owner identity scope — never
/// a legitimate metered-pool caller. Mirrors
/// `crate::execution_owner`'s own session-id convention; a metered
/// reserve must never accept a session id from that namespace.
const RESERVED_SESSION_PREFIX: &str = "execution-owner-v1:";

fn rfc3339(t: OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .expect("timestamps sourced from the ledger always format as RFC3339")
}

/// The local monthly shared budget's pool owner (ADR-0026) is mutated only
/// through `Request::SharedBudget`. Every generic metered handler refuses
/// it with the same reason the launch gate's reserved principal gets, so
/// no `Reserve`, `Settle`, `Release` or `Status` can reach its pools.
fn is_reserved_owner(owner: &str) -> bool {
    owner == libra_governor_domain::LOCAL_BUDGET_OWNER
}

fn reserved_owner_refusal() -> MeteredPoolOutcome {
    MeteredPoolOutcome::Refused {
        reason: MeteredRefusal::InvalidPoolKey,
    }
}

/// Arguments for [`handle_reserve`].
pub struct MeteredReserveArgs<'a> {
    pub owner: &'a str,
    pub alias: &'a str,
    pub unit: QuotaUnit,
    pub amount: u64,
    pub session_id: &'a str,
    pub idempotency_key: &'a str,
    pub session_cap: Option<u64>,
}

/// Length-prefixed, injective encoding of `(session_id, idempotency_key)`
/// into one ledger idempotency key. Every metered hold against a pool
/// shares the same ledger principal (the pool's owner) — see module docs
/// — so the ledger's own `(pool, principal, key)` replay scope would
/// otherwise let one session's key collide with a different session's,
/// handing back someone else's hold as if it were this caller's replay. A
/// length prefix (rather than a plain separator) makes the encoding
/// unambiguous regardless of what characters `session_id` or
/// `idempotency_key` themselves contain.
fn scoped_idempotency_key(session_id: &str, idempotency_key: &str) -> String {
    format!("{}:{session_id}{idempotency_key}", session_id.len())
}

/// Handles `MeteredPoolRequest::Reserve`.
pub fn handle_reserve(
    ledger: &mut LedgerStore,
    args: MeteredReserveArgs<'_>,
    now: OffsetDateTime,
) -> Result<MeteredPoolOutcome, LedgerError> {
    if is_reserved_owner(args.owner) {
        return Ok(reserved_owner_refusal());
    }
    let owner_principal = PrincipalId(args.owner.to_string());
    let Ok(key) = MeteredPoolKey::new(&owner_principal, args.alias) else {
        return Ok(MeteredPoolOutcome::Refused {
            reason: MeteredRefusal::InvalidPoolKey,
        });
    };
    if key.owner().0 == crate::launch_gate::LAUNCH_PRINCIPAL {
        return Ok(MeteredPoolOutcome::Refused {
            reason: MeteredRefusal::InvalidPoolKey,
        });
    }
    if !is_metered_unit(&args.unit) {
        return Ok(MeteredPoolOutcome::Refused {
            reason: MeteredRefusal::UnitNotAllowed,
        });
    }
    if args.amount == 0 {
        return Ok(MeteredPoolOutcome::Refused {
            reason: MeteredRefusal::InvalidAmount,
        });
    }
    if args.session_id.is_empty() || args.session_id.starts_with(RESERVED_SESSION_PREFIX) {
        return Ok(MeteredPoolOutcome::Refused {
            reason: MeteredRefusal::ReservedSessionScope,
        });
    }
    let Some(task_id) = ledger.task_id_for_session(args.session_id)? else {
        return Ok(MeteredPoolOutcome::Refused {
            reason: MeteredRefusal::UnknownSession,
        });
    };

    ledger.expire_stale_shared_pool_reservations(now)?;

    let pool_id = key.pool_id();
    let scoped_key = scoped_idempotency_key(args.session_id, args.idempotency_key);
    let outcome = ledger.reserve_shared_metered(MeteredReserveRequest {
        base: SharedPoolReserveRequest {
            pool_id: &pool_id,
            principal_id: &owner_principal,
            session_id: args.session_id,
            amount: args.amount,
            unit: &args.unit,
            idempotency_key: &scoped_key,
            now,
            ttl_secs: DEFAULT_METERED_HOLD_TTL_SECS,
            attribution: Default::default(),
        },
        session_cap: args.session_cap,
        capacity_source: Default::default(),
    });

    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(LedgerError::QuotaUnitMismatch { expected, actual }) => {
            return Ok(MeteredPoolOutcome::Refused {
                reason: MeteredRefusal::UnitMismatch {
                    pool_unit: expected,
                    requested_unit: actual,
                },
            });
        }
        Err(e) => return Err(e),
    };

    Ok(match outcome {
        MeteredReserveOutcome::Granted(r) => MeteredPoolOutcome::Reserved {
            reservation_id: r.id,
            pool_id: r.pool_id.0.clone(),
            task_id,
            amount: r.amount,
            unit: args.unit,
            expires_at: rfc3339(r.expires_at),
        },
        MeteredReserveOutcome::AlreadyGranted(r) => {
            // Same replay-safety discipline as
            // `crate::launch_gate::handle_admit`'s own `AlreadyGranted`
            // handling: only a hold that is still genuinely active, held
            // by this exact session, and not yet past its own
            // `expires_at` may resolve back to `Reserved`.
            let fresh = r.state == ReservationState::Active
                && r.expires_at > now
                && r.session_id == args.session_id;
            if fresh {
                MeteredPoolOutcome::Reserved {
                    reservation_id: r.id,
                    pool_id: r.pool_id.0.clone(),
                    task_id,
                    amount: r.amount,
                    unit: args.unit,
                    expires_at: rfc3339(r.expires_at),
                }
            } else {
                MeteredPoolOutcome::Final {
                    state: r.state,
                    settled_amount: r.settled_amount,
                }
            }
        }
        MeteredReserveOutcome::Insufficient {
            admission,
            requested,
        } => MeteredPoolOutcome::Refused {
            reason: MeteredRefusal::Insufficient {
                remaining: admission.remaining(),
                requested,
            },
        },
        MeteredReserveOutcome::NoPool => MeteredPoolOutcome::Refused {
            reason: MeteredRefusal::NoPool,
        },
        MeteredReserveOutcome::SnapshotUnusable(state) => MeteredPoolOutcome::Refused {
            reason: MeteredRefusal::Snapshot { state },
        },
        MeteredReserveOutcome::SessionCapExceeded {
            committed,
            cap,
            requested,
        } => MeteredPoolOutcome::Refused {
            reason: MeteredRefusal::SessionCapExceeded {
                committed,
                cap,
                requested,
            },
        },
    })
}

/// Verifies `reservation_id` both belongs to a metered pool owned by
/// `owner` and is currently held by `session_id` — the identity check
/// every settle/release performs before mutating anything. Returns the
/// reservation unchanged; never mutates on a failed check.
fn require_metered_holder(
    ledger: &LedgerStore,
    reservation_id: SharedPoolReservationId,
    owner: &str,
    session_id: &str,
) -> Result<Result<libra_governor_domain::SharedPoolReservation, MeteredPoolOutcome>, LedgerError> {
    let Some(reservation) = ledger.get_shared_reservation(reservation_id)? else {
        return Ok(Err(MeteredPoolOutcome::Refused {
            reason: MeteredRefusal::NotFound,
        }));
    };
    let Some(key) = MeteredPoolKey::parse(&reservation.pool_id) else {
        return Ok(Err(MeteredPoolOutcome::Refused {
            reason: MeteredRefusal::NotHolder,
        }));
    };
    let identity_matches = key.owner().0 == owner
        && reservation.principal_id.0 == owner
        && reservation.session_id == session_id;
    if !identity_matches {
        return Ok(Err(MeteredPoolOutcome::Refused {
            reason: MeteredRefusal::NotHolder,
        }));
    }
    Ok(Ok(reservation))
}

/// Handles `MeteredPoolRequest::Settle`.
pub fn handle_settle(
    ledger: &mut LedgerStore,
    reservation_id: SharedPoolReservationId,
    owner: &str,
    session_id: &str,
    actual: Option<u64>,
    now: OffsetDateTime,
) -> Result<MeteredPoolOutcome, LedgerError> {
    if is_reserved_owner(owner) {
        return Ok(reserved_owner_refusal());
    }
    if let Err(refused) = require_metered_holder(ledger, reservation_id, owner, session_id)? {
        return Ok(refused);
    }
    Ok(match ledger.settle_shared(reservation_id, actual, now)? {
        SharedPoolSettleOutcome::Settled {
            reservation,
            refunded,
            overrun,
        } => MeteredPoolOutcome::Settled {
            reservation_id,
            settled_amount: reservation
                .settled_amount
                .expect("settle_shared's Settled outcome always carries settled_amount"),
            usage_known: reservation.usage_known.unwrap_or(false),
            refunded,
            overrun,
        },
        SharedPoolSettleOutcome::AlreadyFinal(r) => MeteredPoolOutcome::Final {
            state: r.state,
            settled_amount: r.settled_amount,
        },
        SharedPoolSettleOutcome::NotFound => MeteredPoolOutcome::Refused {
            reason: MeteredRefusal::NotFound,
        },
    })
}

/// Handles `MeteredPoolRequest::Release`.
pub fn handle_release(
    ledger: &mut LedgerStore,
    reservation_id: SharedPoolReservationId,
    owner: &str,
    session_id: &str,
    now: OffsetDateTime,
) -> Result<MeteredPoolOutcome, LedgerError> {
    if is_reserved_owner(owner) {
        return Ok(reserved_owner_refusal());
    }
    if let Err(refused) = require_metered_holder(ledger, reservation_id, owner, session_id)? {
        return Ok(refused);
    }
    Ok(match ledger.release_shared(reservation_id, now)? {
        SharedPoolReleaseOutcome::Released(_) => MeteredPoolOutcome::Released { reservation_id },
        SharedPoolReleaseOutcome::AlreadyFinal(r) => MeteredPoolOutcome::Final {
            state: r.state,
            settled_amount: r.settled_amount,
        },
        SharedPoolReleaseOutcome::NotFound => MeteredPoolOutcome::Refused {
            reason: MeteredRefusal::NotFound,
        },
    })
}

/// Handles `MeteredPoolRequest::Status`. Read-only by construction: never
/// calls `ensure_pool` — an unprovisioned pool simply refuses `NoPool`,
/// mirroring every other handler in this module rather than fabricating a
/// zero-capacity report for a pool that does not exist.
pub fn handle_status(
    ledger: &LedgerStore,
    owner: &str,
    alias: &str,
    now: OffsetDateTime,
) -> Result<MeteredPoolOutcome, LedgerError> {
    if is_reserved_owner(owner) {
        return Ok(reserved_owner_refusal());
    }
    let Ok(key) = MeteredPoolKey::new(&PrincipalId(owner.to_string()), alias) else {
        return Ok(MeteredPoolOutcome::Refused {
            reason: MeteredRefusal::InvalidPoolKey,
        });
    };
    let pool_id = key.pool_id();
    let Some(pool) = ledger.quota_pool(&pool_id)? else {
        return Ok(MeteredPoolOutcome::Refused {
            reason: MeteredRefusal::NoPool,
        });
    };
    let admission = ledger
        .pool_admission(&pool_id, now)?
        .expect("quota_pool just confirmed this pool exists");
    let snapshot = ledger.pool_provider_snapshot(&pool_id)?;

    let (effective_capacity, remaining, snapshot_status) =
        match libra_governor_domain::metered_snapshot_limit(snapshot.as_ref(), now) {
            Ok(declared_limit) => {
                let effective = pool.capacity.min(declared_limit);
                let effective_admission = libra_governor_domain::PoolAdmission {
                    capacity: effective,
                    ..admission
                };
                let snapshot = snapshot
                    .expect("Ok(..) from metered_snapshot_limit implies a snapshot was read");
                let libra_governor_domain::GaugeReading::Used { used, .. } = &snapshot.reading
                else {
                    unreachable!(
                        "Ok(..) from metered_snapshot_limit implies a disclosed Used reading"
                    )
                };
                (
                    Some(effective),
                    Some(effective_admission.remaining()),
                    MeteredPoolSnapshotStatus::Usable {
                        observed_at: rfc3339(snapshot.observed_at),
                        valid_until: rfc3339(
                            snapshot
                                .valid_until
                                .expect("Ok(..) from metered_snapshot_limit implies a valid_until"),
                        ),
                        declared_limit,
                        used: used.value,
                    },
                )
            }
            Err(state) => (None, None, MeteredPoolSnapshotStatus::Unusable { state }),
        };

    let mut sessions = Vec::new();
    for usage in ledger.shared_pool_session_usage(&pool_id)? {
        sessions.push(MeteredPoolSessionUsage {
            task_id: ledger.task_id_for_session(&usage.session_id)?,
            session_id: usage.session_id,
            active: usage.active,
            settled: usage.settled,
        });
    }

    Ok(MeteredPoolOutcome::StatusReport(Box::new(
        MeteredPoolStatus {
            pool_id: pool_id.0,
            unit: pool.unit,
            capacity: pool.capacity,
            effective_capacity,
            settled: admission.settled,
            active: admission.active,
            external_overhang: admission.external_overhang,
            remaining,
            snapshot: snapshot_status,
            sessions,
            enforcement_scope: METERED_POOL_ENFORCEMENT_SCOPE.to_string(),
        },
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use libra_governor_domain::{Confidence, GaugeReading, PoolId, QuotaAmount};

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()
    }

    fn establish_session(
        ledger: &mut LedgerStore,
        session_id: &str,
    ) -> libra_governor_domain::TaskId {
        ledger
            .resolve_or_create_task_for_session(session_id, now())
            .unwrap()
    }

    fn reserve_args<'a>(
        owner: &'a str,
        alias: &'a str,
        session_id: &'a str,
        key: &'a str,
    ) -> MeteredReserveArgs<'a> {
        MeteredReserveArgs {
            owner,
            alias,
            unit: QuotaUnit::UsdCents,
            amount: 20_000,
            session_id,
            idempotency_key: key,
            session_cap: None,
        }
    }

    fn ingest(ledger: &mut LedgerStore, pool_id: &PoolId, used: u64, limit: Option<u64>) {
        ledger
            .ingest_pool_provider_snapshot(
                pool_id,
                now(),
                Some(now() + time::Duration::hours(1)),
                &GaugeReading::Used {
                    used: QuotaAmount::new(QuotaUnit::UsdCents, used),
                    limit,
                },
                Confidence::Medium,
                now(),
            )
            .unwrap();
    }

    #[test]
    fn reserve_refuses_unknown_session() {
        let mut ledger = LedgerStore::open_in_memory().unwrap();
        let outcome = handle_reserve(
            &mut ledger,
            reserve_args("acme", "usd-2026-10", "never-preflighted", "k1"),
            now(),
        )
        .unwrap();
        assert!(matches!(
            outcome,
            MeteredPoolOutcome::Refused {
                reason: MeteredRefusal::UnknownSession
            }
        ));
    }

    #[test]
    fn reserve_refuses_reserved_session_scope() {
        let mut ledger = LedgerStore::open_in_memory().unwrap();
        let outcome = handle_reserve(
            &mut ledger,
            reserve_args("acme", "usd-2026-10", "execution-owner-v1:abc", "k1"),
            now(),
        )
        .unwrap();
        assert!(matches!(
            outcome,
            MeteredPoolOutcome::Refused {
                reason: MeteredRefusal::ReservedSessionScope
            }
        ));
    }

    #[test]
    fn reserve_refuses_launch_principal_as_owner() {
        let mut ledger = LedgerStore::open_in_memory().unwrap();
        establish_session(&mut ledger, "s1");
        let outcome = handle_reserve(
            &mut ledger,
            reserve_args(
                crate::launch_gate::LAUNCH_PRINCIPAL,
                "usd-2026-10",
                "s1",
                "k1",
            ),
            now(),
        )
        .unwrap();
        assert!(matches!(
            outcome,
            MeteredPoolOutcome::Refused {
                reason: MeteredRefusal::InvalidPoolKey
            }
        ));
    }

    #[test]
    fn reserve_refuses_no_pool_then_succeeds_once_fixture_provisions_and_feeds_it() {
        let mut ledger = LedgerStore::open_in_memory().unwrap();
        let task_id = establish_session(&mut ledger, "s1");
        let outcome = handle_reserve(
            &mut ledger,
            reserve_args("acme", "usd-2026-10", "s1", "k1"),
            now(),
        )
        .unwrap();
        assert!(matches!(
            outcome,
            MeteredPoolOutcome::Refused {
                reason: MeteredRefusal::NoPool
            }
        ));

        // The pool only comes to exist via a test-owned fixture directly
        // calling `ensure_pool`/`ingest_pool_provider_snapshot` — never
        // through this module.
        let pool_id = PoolId("metered:acme/usd-2026-10".to_string());
        ledger
            .ensure_pool(&pool_id, &QuotaUnit::UsdCents, 50_000, now())
            .unwrap();
        ingest(&mut ledger, &pool_id, 0, Some(50_000));

        match handle_reserve(
            &mut ledger,
            reserve_args("acme", "usd-2026-10", "s1", "k1"),
            now(),
        )
        .unwrap()
        {
            MeteredPoolOutcome::Reserved {
                amount,
                task_id: got,
                ..
            } => {
                assert_eq!(amount, 20_000);
                assert_eq!(got, task_id);
            }
            other => panic!("expected Reserved, got {other:?}"),
        }
    }

    #[test]
    fn reserve_replay_with_same_key_returns_reserved_again() {
        let mut ledger = LedgerStore::open_in_memory().unwrap();
        establish_session(&mut ledger, "s1");
        let pool_id = PoolId("metered:acme/usd-2026-10".to_string());
        ledger
            .ensure_pool(&pool_id, &QuotaUnit::UsdCents, 50_000, now())
            .unwrap();
        ingest(&mut ledger, &pool_id, 0, Some(50_000));

        let first = handle_reserve(
            &mut ledger,
            reserve_args("acme", "usd-2026-10", "s1", "k1"),
            now(),
        )
        .unwrap();
        let second = handle_reserve(
            &mut ledger,
            reserve_args("acme", "usd-2026-10", "s1", "k1"),
            now(),
        )
        .unwrap();
        match (first, second) {
            (
                MeteredPoolOutcome::Reserved {
                    reservation_id: a, ..
                },
                MeteredPoolOutcome::Reserved {
                    reservation_id: b, ..
                },
            ) => assert_eq!(a, b),
            other => panic!("expected both to be Reserved with the same id, got {other:?}"),
        }
    }

    #[test]
    fn two_sessions_reusing_the_same_literal_idempotency_key_get_distinct_holds() {
        // The scoped-key encoding must prevent session B's key "k1" from
        // ever resolving to session A's hold.
        let mut ledger = LedgerStore::open_in_memory().unwrap();
        establish_session(&mut ledger, "s1");
        establish_session(&mut ledger, "s2");
        let pool_id = PoolId("metered:acme/usd-2026-10".to_string());
        ledger
            .ensure_pool(&pool_id, &QuotaUnit::UsdCents, 50_000, now())
            .unwrap();
        ingest(&mut ledger, &pool_id, 0, Some(50_000));

        let a = handle_reserve(
            &mut ledger,
            reserve_args("acme", "usd-2026-10", "s1", "k1"),
            now(),
        )
        .unwrap();
        let b = handle_reserve(
            &mut ledger,
            reserve_args("acme", "usd-2026-10", "s2", "k1"),
            now(),
        )
        .unwrap();
        match (a, b) {
            (
                MeteredPoolOutcome::Reserved {
                    reservation_id: a, ..
                },
                MeteredPoolOutcome::Reserved {
                    reservation_id: b, ..
                },
            ) => assert_ne!(a, b),
            other => panic!("expected both Reserved with distinct ids, got {other:?}"),
        }
    }

    #[test]
    fn settle_refuses_non_holder() {
        let mut ledger = LedgerStore::open_in_memory().unwrap();
        establish_session(&mut ledger, "s1");
        let pool_id = PoolId("metered:acme/usd-2026-10".to_string());
        ledger
            .ensure_pool(&pool_id, &QuotaUnit::UsdCents, 50_000, now())
            .unwrap();
        ingest(&mut ledger, &pool_id, 0, Some(50_000));
        let MeteredPoolOutcome::Reserved { reservation_id, .. } = handle_reserve(
            &mut ledger,
            reserve_args("acme", "usd-2026-10", "s1", "k1"),
            now(),
        )
        .unwrap() else {
            panic!("expected Reserved");
        };

        let wrong_owner =
            handle_settle(&mut ledger, reservation_id, "globex", "s1", None, now()).unwrap();
        assert!(matches!(
            wrong_owner,
            MeteredPoolOutcome::Refused {
                reason: MeteredRefusal::NotHolder
            }
        ));
        let wrong_session =
            handle_settle(&mut ledger, reservation_id, "acme", "s2", None, now()).unwrap();
        assert!(matches!(
            wrong_session,
            MeteredPoolOutcome::Refused {
                reason: MeteredRefusal::NotHolder
            }
        ));

        let ok = handle_settle(
            &mut ledger,
            reservation_id,
            "acme",
            "s1",
            Some(12_000),
            now(),
        )
        .unwrap();
        match ok {
            MeteredPoolOutcome::Settled {
                settled_amount,
                refunded,
                ..
            } => {
                assert_eq!(settled_amount, 12_000);
                assert_eq!(refunded, Some(8_000));
            }
            other => panic!("expected Settled, got {other:?}"),
        }
    }

    #[test]
    fn status_refuses_no_pool_then_reports_after_fixture_provisions_it() {
        let mut ledger = LedgerStore::open_in_memory().unwrap();
        assert!(matches!(
            handle_status(&ledger, "acme", "usd-2026-10", now()).unwrap(),
            MeteredPoolOutcome::Refused {
                reason: MeteredRefusal::NoPool
            }
        ));

        let pool_id = PoolId("metered:acme/usd-2026-10".to_string());
        ledger
            .ensure_pool(&pool_id, &QuotaUnit::UsdCents, 50_000, now())
            .unwrap();
        ingest(&mut ledger, &pool_id, 12_000, Some(50_000));

        match handle_status(&ledger, "acme", "usd-2026-10", now()).unwrap() {
            MeteredPoolOutcome::StatusReport(status) => {
                assert_eq!(status.capacity, 50_000);
                assert_eq!(status.effective_capacity, Some(50_000));
                assert_eq!(status.remaining, Some(38_000));
                assert_eq!(status.enforcement_scope, METERED_POOL_ENFORCEMENT_SCOPE);
            }
            other => panic!("expected StatusReport, got {other:?}"),
        }
    }
}
