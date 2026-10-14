//! The launch-gate admission primitive (HORO-1766): admit/renew/release/
//! status handlers over an opt-in shared slot pool. See
//! `docs/adr/0020-launch-gate-actuator.md` for the design rationale this
//! module implements.
//!
//! # What this is — and is not
//!
//! An occupancy gate ("capacity 2" means at most 2 Libra-launched jobs
//! run at once), built entirely on `libra_governor_ledger::shared_pool`'s
//! atomic `reserve_shared`/`release_shared`/`renew_shared`/
//! `expire_stale_shared_pool_reservations` (HORO-1763). This module adds
//! no new locking or correctness mechanism of its own — every AC1-style
//! guarantee ("concurrent launches never collectively exceed capacity")
//! comes from the ledger's own `BEGIN IMMEDIATE` transaction discipline,
//! reused as-is.
//!
//! This module (and every other file in `daemon/src`, `gateway/src`,
//! `ledger/src`) must never reference the SUSTAIN/BURST simulator module
//! documented in ADR-0016 — see that crate's own zero-live-wiring static
//! guard test (named after the module it protects, in
//! `crates/domain/tests/`) and ADR-0020's own "why this does not consume
//! it" section. HORO-1727 has not yet settled the live-admission
//! architecture question; that is a decision for a later, separate
//! ticket (tracked there as "PR3"), not this one.
//!
//! # Lazy, idempotent pool creation
//!
//! There is no separate "wire this up once at daemon startup" step.
//! `handle_admit` calls `ensure_pool` on every `Admit` for the pool it
//! names; `ensure_pool` is itself an `INSERT ... ON CONFLICT DO NOTHING`
//! (see `libra_governor_ledger::shared_pool::LedgerStore::ensure_pool`),
//! so this is a cheap, correct no-op on every call after the first. This
//! also means the gate works identically whether pools happen to get
//! created via `serve`'s full accept loop or via a test harness that
//! drives `dispatch`/`handle_connection` directly against a single
//! connection (see `crates/daemon/tests/preflight_integration.rs`'s own
//! pattern, which this ticket's own integration test follows) — there is
//! no daemon-startup-only code path whose behavior the tests would
//! otherwise have to fake.
use time::OffsetDateTime;
use uuid::Uuid;

use libra_governor_domain::{CreditNamespace, PoolId, PrincipalId, QuotaUnit, ReservationState};
use libra_governor_ledger::{
    LedgerError, LedgerStore, SharedPoolReleaseOutcome, SharedPoolRenewOutcome,
    SharedPoolReserveOutcome, SharedPoolReserveRequest,
};
use libra_governor_protocol::{LaunchOutcome, PoolStatus, UnavailableReason, WaitReason};

/// Default `[launcher].hold_ttl_secs` when a `config.json` `[launcher]`
/// section omits it — generous enough to cover a slow job start without
/// requiring an unreasonably tight renew cadence, mirroring this crate's
/// own `DEFAULT_RESERVATION_TTL_SECS` precedent in spirit (a different
/// constant because the two TTLs govern unrelated holds).
pub const DEFAULT_HOLD_TTL_SECS: u64 = 300;

/// Every launch-gate hold is attributed to this one fixed principal —
/// the launcher itself, not the many different jobs it starts. The
/// real "who" that matters for this gate is the ledger `session_id`
/// (always the caller's `launch_id`), which is what the identity checks
/// in [`handle_release`]/[`handle_renew`] actually compare.
pub const LAUNCH_PRINCIPAL: &str = "libra-launcher";

/// The fixed, opaque credit namespace every launch-gate pool is created
/// under — a slot is not tokens, cents, or requests, so none of
/// `QuotaUnit`'s other variants fit; see `QuotaUnit::OpaqueCredit`'s own
/// docs for why a namespaced opaque unit exists for exactly this case.
const LAUNCH_CREDIT_NAMESPACE_SLUG: &str = "libra-launch-slot";

/// A heuristic, fixed retry hint for a `Wait` outcome — not derived from
/// the actual remaining TTL of whichever hold is currently occupying the
/// pool (the ledger does not expose "soonest expiry across active holds"
/// as a cheap single query, and guessing it from an interior `SELECT`
/// here would add a second scan to every throttled `Admit`). A caller is
/// free to poll sooner or later; this is a suggestion, not a contract.
pub const DEFAULT_RETRY_AFTER_SECS: u64 = 5;

/// One `[launcher].pools[]` entry, resolved from `config.json` (HORO-1766).
#[derive(Debug, Clone, PartialEq)]
pub struct LaunchPoolConfig {
    pub alias: String,
    pub capacity: u64,
    pub enabled: bool,
}

/// The resolved `[launcher]` section (HORO-1766). `None` on
/// [`crate::server::DaemonConfig::launch_gate`] means the gate is off —
/// every `Admit` answers `Unavailable { reason: not_enabled }` (ADR-0020:
/// off by default).
#[derive(Debug, Clone, PartialEq)]
pub struct LaunchGateConfig {
    pub pools: Vec<LaunchPoolConfig>,
    pub hold_ttl_secs: u64,
}

impl LaunchGateConfig {
    fn pool(&self, alias: &str) -> Option<&LaunchPoolConfig> {
        self.pools.iter().find(|p| p.alias == alias)
    }
}

/// The fixed unit every launch-gate pool is created under. `CreditNamespace::new`
/// only fails on an invalid slug (see `is_valid_slug` in
/// `libra_governor_domain::quota_window`); `LAUNCH_CREDIT_NAMESPACE_SLUG`
/// is a hardcoded, known-valid ASCII-lowercase-and-hyphens literal, so
/// this can never actually fail — the `expect` documents that invariant
/// rather than hiding a real fallible path.
fn launch_unit() -> QuotaUnit {
    QuotaUnit::OpaqueCredit(
        CreditNamespace::new(LAUNCH_CREDIT_NAMESPACE_SLUG)
            .expect("LAUNCH_CREDIT_NAMESPACE_SLUG is a hardcoded valid slug"),
    )
}

/// `alias` -> the ledger `PoolId` this gate stores it under. Namespaced
/// with a `launch:` prefix so a launch-gate pool can never collide with
/// a pool some other quota-window subject created under the same bare
/// alias (`config_file::resolve_launcher` already refuses an alias
/// containing `/`, so this prefix is the only mechanism providing that
/// separation, not a belt-and-suspenders one).
fn pool_id_for_alias(alias: &str) -> PoolId {
    PoolId(format!("launch:{alias}"))
}

/// Every timestamp this module ever formats originates from the ledger's
/// own `shared_pool` module, which already round-trips every stored
/// instant through this exact RFC3339 formatter (see that module's own
/// `rfc3339`/`parse_time` pair) — so this can never actually fail in
/// practice. `expect` documents that invariant rather than threading a
/// `Result` through every handler here for a formatting error path that
/// is, in effect, dead.
fn rfc3339(t: OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .expect("timestamps sourced from the ledger always format as RFC3339")
}

/// Handles `LaunchRequest::Admit`. See module docs for why pool creation
/// is lazy rather than a separate startup step.
pub fn handle_admit(
    ledger: &mut LedgerStore,
    launcher: Option<&LaunchGateConfig>,
    pool_alias: &str,
    launch_id: Uuid,
    now: OffsetDateTime,
) -> Result<LaunchOutcome, LedgerError> {
    let Some(launcher) = launcher else {
        return Ok(LaunchOutcome::Unavailable {
            reason: UnavailableReason::NotEnabled,
        });
    };
    let Some(pool_cfg) = launcher.pool(pool_alias) else {
        return Ok(LaunchOutcome::Unavailable {
            reason: UnavailableReason::UnknownPool,
        });
    };
    if !pool_cfg.enabled {
        return Ok(LaunchOutcome::Unavailable {
            reason: UnavailableReason::AdminDisabled,
        });
    }

    let pool_id = pool_id_for_alias(&pool_cfg.alias);
    let unit = launch_unit();
    let pool = ledger.ensure_pool(&pool_id, &unit, pool_cfg.capacity, now)?;
    if pool.unit != unit {
        // A pool already exists at this exact PoolId under a DIFFERENT
        // unit than the one this gate always uses — `ensure_pool` never
        // overwrites, so this is a real config/ledger collision, not a
        // transient state. Refused rather than coerced; see
        // `config_file::resolve_launcher`'s own docs for why this check
        // cannot happen at config-parse time instead (no ledger handle
        // there).
        return Ok(LaunchOutcome::Unavailable {
            reason: UnavailableReason::UnknownPool,
        });
    }

    // Reclaim any holds that lapsed before this request, so a stale hold
    // from a crashed job never makes a genuinely available slot look
    // throttled.
    ledger.expire_stale_shared_pool_reservations(now)?;

    let principal = PrincipalId(LAUNCH_PRINCIPAL.to_string());
    let idempotency_key = launch_id.to_string();
    let reserve_outcome = ledger.reserve_shared(SharedPoolReserveRequest {
        pool_id: &pool_id,
        principal_id: &principal,
        session_id: &idempotency_key,
        amount: 1,
        unit: &unit,
        idempotency_key: &idempotency_key,
        now,
        ttl_secs: launcher.hold_ttl_secs,
    })?;

    Ok(match reserve_outcome {
        SharedPoolReserveOutcome::Granted(reservation) => LaunchOutcome::Start {
            reservation_id: reservation.id,
            expires_at: rfc3339(reservation.expires_at),
            renew_every_secs: launcher.hold_ttl_secs,
        },
        SharedPoolReserveOutcome::AlreadyGranted(reservation) => {
            // THE critical correctness trap this ticket exists to close:
            // `reserve_shared`'s idempotency lookup is scoped to
            // `(pool_id, principal_id, idempotency_key)` with NO state
            // filter — a `launch_id` whose original hold already expired
            // or was released still matches this lookup and comes back
            // here as `AlreadyGranted`, carrying the now-stale row. Only
            // a row that is BOTH still `Active` AND not yet past its own
            // `expires_at` may ever resolve to `Start`; anything else
            // must surface as `Lost` so a caller that retried a dead
            // launch_id after its hold already lapsed can never silently
            // get an unguarded start against a pool that may since be
            // fully resubscribed by someone else.
            if reservation.state == ReservationState::Active && reservation.expires_at > now {
                LaunchOutcome::Start {
                    reservation_id: reservation.id,
                    expires_at: rfc3339(reservation.expires_at),
                    renew_every_secs: launcher.hold_ttl_secs,
                }
            } else {
                LaunchOutcome::Lost {
                    state: reservation.state,
                }
            }
        }
        SharedPoolReserveOutcome::Insufficient { admission, .. } => {
            let known_spend_full =
                admission.active.saturating_add(admission.settled) >= admission.capacity;
            let reason = if !known_spend_full && admission.external_overhang > 0 {
                WaitReason::ExternalOverhang
            } else {
                WaitReason::Throttled {
                    active: admission.active,
                    capacity: admission.capacity,
                }
            };
            LaunchOutcome::Wait {
                reason,
                retry_after_secs: DEFAULT_RETRY_AFTER_SECS,
            }
        }
        // Can only happen if `ensure_pool` above somehow did not leave a
        // row behind (it always does) — defensive, not reachable in
        // practice.
        SharedPoolReserveOutcome::NoPool => LaunchOutcome::Unavailable {
            reason: UnavailableReason::UnknownPool,
        },
    })
}

/// Handles `LaunchRequest::Renew`.
pub fn handle_renew(
    ledger: &mut LedgerStore,
    launcher: Option<&LaunchGateConfig>,
    reservation_id: libra_governor_domain::SharedPoolReservationId,
    launch_id: Uuid,
    now: OffsetDateTime,
) -> Result<LaunchOutcome, LedgerError> {
    let Some(launcher) = launcher else {
        return Ok(LaunchOutcome::Unavailable {
            reason: UnavailableReason::NotEnabled,
        });
    };
    ledger.expire_stale_shared_pool_reservations(now)?;

    let principal = PrincipalId(LAUNCH_PRINCIPAL.to_string());
    let session_id = launch_id.to_string();
    let outcome = ledger.renew_shared(
        reservation_id,
        &principal,
        &session_id,
        now,
        launcher.hold_ttl_secs,
    )?;
    Ok(match outcome {
        SharedPoolRenewOutcome::Renewed(reservation) => LaunchOutcome::Renewed {
            expires_at: rfc3339(reservation.expires_at),
        },
        SharedPoolRenewOutcome::NotFound => LaunchOutcome::Unavailable {
            reason: UnavailableReason::HoldExpired,
        },
        SharedPoolRenewOutcome::Refused(current) => {
            if current.state != ReservationState::Active {
                LaunchOutcome::Lost {
                    state: current.state,
                }
            } else {
                // Still active, but refused — a wrong principal/session
                // (not this caller's hold) or a stale-but-not-yet-swept
                // row. Either way, this caller does not get to extend
                // it.
                LaunchOutcome::Unavailable {
                    reason: UnavailableReason::RenewFailed,
                }
            }
        }
    })
}

/// Handles `LaunchRequest::Release`. The identity check happens here,
/// BEFORE calling `release_shared`, rather than leaning on the ledger
/// alone to refuse a mismatch — `release_shared` itself only checks
/// `id`/state, not caller identity (see that method's own docs). This
/// read-then-conditionally-write sequence is safe against interleaving
/// because `crates/daemon/src/server.rs`'s accept loop is strictly serial
/// (one connection fully handled before the next is accepted) — see that
/// module's own concurrency-model docs. A future move to a
/// thread-per-connection model would need to push this identity check
/// into the ledger's own atomic transaction instead.
pub fn handle_release(
    ledger: &mut LedgerStore,
    reservation_id: libra_governor_domain::SharedPoolReservationId,
    launch_id: Uuid,
    now: OffsetDateTime,
) -> Result<LaunchOutcome, LedgerError> {
    let Some(current) = ledger.get_shared_reservation(reservation_id)? else {
        return Ok(LaunchOutcome::Unavailable {
            reason: UnavailableReason::HoldExpired,
        });
    };
    let principal = PrincipalId(LAUNCH_PRINCIPAL.to_string());
    let session_id = launch_id.to_string();
    let is_launch_pool = current.pool_id.0.starts_with("launch:");
    if current.principal_id != principal || current.session_id != session_id || !is_launch_pool {
        // AC1: a release must never free a slot it doesn't actually hold
        // — refused, the slot (if any) stays held by whoever really
        // holds it.
        return Ok(LaunchOutcome::Unavailable {
            reason: UnavailableReason::ReleaseRefused,
        });
    }

    Ok(match ledger.release_shared(reservation_id, now)? {
        // A successful release leaves the row `Released` — exactly the
        // state `Lost` already exists to describe ("you no longer hold
        // this"), so a dedicated "released OK" variant would duplicate
        // it. The caller asked to give this hold up; `Lost { state:
        // Released }` confirms it is in fact gone.
        SharedPoolReleaseOutcome::Released(r) => LaunchOutcome::Lost { state: r.state },
        SharedPoolReleaseOutcome::AlreadyFinal(r) => LaunchOutcome::Lost { state: r.state },
        SharedPoolReleaseOutcome::NotFound => LaunchOutcome::Unavailable {
            reason: UnavailableReason::HoldExpired,
        },
    })
}

/// Handles `LaunchRequest::Status`. Read-only by construction: never
/// calls `ensure_pool` (which would be a write) — a pool that has never
/// been admitted against simply reports `effective_capacity: 0, active:
/// 0` until the first `Admit` creates it.
pub fn handle_status(
    ledger: &LedgerStore,
    launcher: Option<&LaunchGateConfig>,
    now: OffsetDateTime,
) -> Result<LaunchOutcome, LedgerError> {
    let Some(launcher) = launcher else {
        return Ok(LaunchOutcome::StatusReport { pools: Vec::new() });
    };
    let mut pools = Vec::with_capacity(launcher.pools.len());
    for pool_cfg in &launcher.pools {
        let pool_id = pool_id_for_alias(&pool_cfg.alias);
        let admission = ledger.pool_admission(&pool_id, now)?;
        let (effective_capacity, active) = match admission {
            Some(a) => (a.capacity, a.active),
            None => (0, 0),
        };
        pools.push(PoolStatus {
            alias: pool_cfg.alias.clone(),
            configured_capacity: pool_cfg.capacity,
            effective_capacity,
            active,
            enabled: pool_cfg.enabled,
            enforcement_scope: libra_governor_protocol::LAUNCH_GATE_ENFORCEMENT_SCOPE.to_string(),
        });
    }
    Ok(LaunchOutcome::StatusReport { pools })
}
