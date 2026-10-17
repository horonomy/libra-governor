//! Metered shared-pool IPC (HORO-1915): reserve/settle/release/status over
//! a provider-denominated (USD cents / tokens / requests) shared pool.
//!
//! Structurally independent of [`crate::launch`]'s opaque launch-slot
//! pools — a metered pool's id is namespaced `metered:<owner>/<alias>`
//! (disjoint at the first byte from `launch:<alias>`, see
//! `libra_governor_domain::MeteredPoolKey`), and its unit is always one a
//! provider snapshot can be compared against directly (USD cents, tokens,
//! requests), never the launch gate's opaque `OpaqueCredit("libra-launch-slot")`.
//!
//! # Production feed (deliberately absent in this phase)
//!
//! Nothing in this crate, or in `libra-governor-daemon`'s
//! `metered_pool` handlers, ever calls `ensure_pool` or
//! `ingest_pool_provider_snapshot` for a metered pool — every request
//! against an unprovisioned pool refuses with
//! [`MeteredRefusal::NoPool`]. Provisioning and feeding a metered pool in
//! this phase is exclusively a test-owned synthetic fixture
//! (`crates/cli/tests/metered_pool_fixture`), never
//! `libra-quota-observation`'s production registration/consent pipeline —
//! see `docs/adr/0023-metered-shared-pool.md` for why promoting a real
//! provider observation into this admission path is a separate,
//! not-yet-authorized trust-boundary decision.
use serde::{Deserialize, Serialize};

use libra_governor_domain::{
    QuotaUnit, ReservationState, SharedPoolReservationId, SnapshotUnusable, TaskId,
};

/// The fixed scope every [`MeteredPoolStatus`] reports: this ledger only
/// ever enforces holds *this host's* daemon reserved against its own
/// SQLite file — it makes no claim about, and has no visibility into,
/// spend anywhere else. See `docs/adr/0023-metered-shared-pool.md`.
pub const METERED_POOL_ENFORCEMENT_SCOPE: &str = "libra_reserved_holds_on_this_host_only";

/// One request a client may send over the metered-pool IPC (embedded in
/// [`crate::Request::MeteredPool`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum MeteredPoolRequest {
    /// Reserve `amount` of `unit` against `owner`/`alias`'s pool on behalf
    /// of `session_id`. `idempotency_key` is scoped to this session (see
    /// `libra_governor_daemon::metered_pool` for the exact encoding) —
    /// two different sessions reusing the same literal key never collide.
    Reserve {
        owner: String,
        alias: String,
        unit: QuotaUnit,
        amount: u64,
        session_id: String,
        idempotency_key: String,
        /// Optional cooperative per-session cap, layered on top of the
        /// pool's own capacity — never a required allocation.
        session_cap: Option<u64>,
    },
    /// Settle a held reservation. `actual = None` is the conservative
    /// fallback (settle at the full reserved amount); `Some` reports a
    /// real usage receipt, which may refund unspent capacity or record an
    /// overrun.
    Settle {
        reservation_id: SharedPoolReservationId,
        owner: String,
        session_id: String,
        actual: Option<u64>,
    },
    /// Give back a held reservation unspent, before any usage occurred.
    Release {
        reservation_id: SharedPoolReservationId,
        owner: String,
        session_id: String,
    },
    /// Read-only: `owner`/`alias`'s pool's current posture.
    Status { owner: String, alias: String },
}

/// Why a [`MeteredPoolRequest`] was refused outright rather than
/// admitted/applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum MeteredRefusal {
    /// `owner`/`alias` is not a valid metered-pool key — either slug
    /// failed validation, or `owner` named the launch gate's own reserved
    /// principal.
    InvalidPoolKey,
    /// `amount` was zero.
    InvalidAmount,
    /// `unit` is not one of the provider-denominated units a metered pool
    /// may use (opaque launch-slot credit and basis-point percentages are
    /// refused).
    UnitNotAllowed,
    /// The pool already exists under a different unit than the one this
    /// request named.
    UnitMismatch {
        pool_unit: QuotaUnit,
        requested_unit: QuotaUnit,
    },
    /// No `quota_pools` row exists for this `owner`/`alias` — this phase
    /// has no production feed to provision one; see module docs.
    NoPool,
    /// `session_id` has never been established via the normal
    /// Preflight/hook path (no `session_tasks` mapping exists for it) —
    /// a metered reserve never accepts a client-asserted task identity.
    UnknownSession,
    /// `session_id` names a reserved identity scope (e.g. an
    /// execution-owner-v1 session id), never a legitimate metered-pool
    /// caller.
    ReservedSessionScope,
    /// The pool's provider snapshot could not supply a trusted declared
    /// limit — see [`SnapshotUnusable`].
    Snapshot { state: SnapshotUnusable },
    /// The optional per-session cap would be exceeded.
    SessionCapExceeded {
        committed: u64,
        cap: u64,
        requested: u64,
    },
    /// The pool-wide effective capacity would be exceeded.
    Insufficient { remaining: i64, requested: u64 },
    /// A `Settle`/`Release` named a `reservation_id` whose recorded
    /// `(pool owner, principal, session)` identity did not match the
    /// caller's — refused; the hold stays exactly as it was (never
    /// settled/released by someone who does not hold it).
    NotHolder,
    /// A `Settle`/`Release`/`Status` named a reservation/pool that does
    /// not exist.
    NotFound,
}

/// The response to a [`MeteredPoolRequest`] (embedded in
/// [`crate::Response::MeteredPool`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum MeteredPoolOutcome {
    Reserved {
        reservation_id: SharedPoolReservationId,
        pool_id: String,
        /// The task this session was already attributed to via the
        /// normal Preflight path — never a caller-asserted identity.
        task_id: TaskId,
        amount: u64,
        unit: QuotaUnit,
        /// RFC3339, UTC.
        expires_at: String,
    },
    Settled {
        reservation_id: SharedPoolReservationId,
        settled_amount: u64,
        usage_known: bool,
        refunded: Option<u64>,
        overrun: Option<u64>,
    },
    Released {
        reservation_id: SharedPoolReservationId,
    },
    /// A replayed `Reserve` (same idempotency key) whose hold is no
    /// longer active, or a `Settle`/`Release` retry against an
    /// already-terminal hold — mirrors `crate::LaunchOutcome::Lost`'s own
    /// replay-safety discipline: never silently re-synthesized as a fresh
    /// grant.
    Final {
        state: ReservationState,
        settled_amount: Option<u64>,
    },
    Refused {
        reason: MeteredRefusal,
    },
    StatusReport(Box<MeteredPoolStatus>),
}

/// One session's reservation/settlement totals against a metered pool, as
/// reported by [`MeteredPoolRequest::Status`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeteredPoolSessionUsage {
    pub session_id: String,
    pub task_id: Option<TaskId>,
    pub active: u64,
    pub settled: u64,
}

/// A metered pool's current posture, as reported by
/// [`MeteredPoolRequest::Status`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeteredPoolStatus {
    pub pool_id: String,
    pub unit: QuotaUnit,
    /// The pool's own stored capacity (fixed at first creation).
    pub capacity: u64,
    /// `min(capacity, declared snapshot limit)` when the snapshot is
    /// usable; `None` when it is not (never silently treated as
    /// unlimited).
    pub effective_capacity: Option<u64>,
    pub settled: u64,
    pub active: u64,
    pub external_overhang: u64,
    /// `None` whenever the snapshot is unusable — never shown as
    /// available capacity in that case.
    pub remaining: Option<i64>,
    pub snapshot: MeteredPoolSnapshotStatus,
    pub sessions: Vec<MeteredPoolSessionUsage>,
    /// Always [`METERED_POOL_ENFORCEMENT_SCOPE`] today — carried as data,
    /// not just a doc comment, so a future broader scope is a visible
    /// wire change.
    pub enforcement_scope: String,
}

/// Whether a metered pool's provider snapshot is currently usable for
/// admission, and if not, why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum MeteredPoolSnapshotStatus {
    Usable {
        observed_at: String,
        valid_until: String,
        declared_limit: u64,
        used: u64,
    },
    Unusable {
        state: SnapshotUnusable,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserve_request_round_trips() {
        let req = MeteredPoolRequest::Reserve {
            owner: "acme".to_string(),
            alias: "usd-2026-10".to_string(),
            unit: QuotaUnit::UsdCents,
            amount: 20_000,
            session_id: "sess-a".to_string(),
            idempotency_key: "a1".to_string(),
            session_cap: Some(25_000),
        };
        let json = serde_json::to_string(&req).unwrap();
        let round_tripped: MeteredPoolRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, round_tripped);
    }

    #[test]
    fn status_request_rejects_unknown_fields() {
        let json = serde_json::to_string(&MeteredPoolRequest::Status {
            owner: "acme".to_string(),
            alias: "usd-2026-10".to_string(),
        })
        .unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value["extra"] = serde_json::json!("nope");
        assert!(serde_json::from_value::<MeteredPoolRequest>(value).is_err());
    }

    #[test]
    fn full_request_response_round_trips_through_the_top_level_envelope() {
        let envelope = crate::RequestEnvelope {
            protocol_version: crate::PROTOCOL_VERSION,
            request: crate::Request::MeteredPool {
                metered_pool: MeteredPoolRequest::Status {
                    owner: "acme".to_string(),
                    alias: "usd-2026-10".to_string(),
                },
            },
        };
        let json = serde_json::to_string(&envelope).unwrap();
        let round_tripped: crate::RequestEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(envelope, round_tripped);

        let response_envelope = crate::ResponseEnvelope {
            protocol_version: crate::PROTOCOL_VERSION,
            response: crate::Response::MeteredPool(Box::new(MeteredPoolOutcome::Refused {
                reason: MeteredRefusal::NoPool,
            })),
        };
        let json = serde_json::to_string(&response_envelope).unwrap();
        let round_tripped: crate::ResponseEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(response_envelope, round_tripped);
    }

    #[test]
    fn status_report_carries_the_fixed_enforcement_scope() {
        let report = MeteredPoolOutcome::StatusReport(Box::new(MeteredPoolStatus {
            pool_id: "metered:acme/usd-2026-10".to_string(),
            unit: QuotaUnit::UsdCents,
            capacity: 50_000,
            effective_capacity: Some(50_000),
            settled: 0,
            active: 0,
            external_overhang: 0,
            remaining: Some(50_000),
            snapshot: MeteredPoolSnapshotStatus::Usable {
                observed_at: "2026-10-17T00:00:00Z".to_string(),
                valid_until: "2026-10-17T01:00:00Z".to_string(),
                declared_limit: 50_000,
                used: 0,
            },
            sessions: vec![],
            enforcement_scope: METERED_POOL_ENFORCEMENT_SCOPE.to_string(),
        }));
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["outcome"], serde_json::json!("status_report"));
        assert_eq!(
            json["enforcement_scope"],
            serde_json::json!(METERED_POOL_ENFORCEMENT_SCOPE)
        );
    }
}
