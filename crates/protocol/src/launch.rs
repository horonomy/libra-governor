//! Launch-gate IPC (HORO-1766): wire types for the opt-in, off-by-default
//! admission primitive over shared **slot** pools (occupancy — "capacity
//! 2" means at most 2 Libra-launched jobs run at once). Structurally
//! mirrors `crate::execution_owner`'s request/response shaping for
//! consistency within this crate, but is its own, independent request
//! family — a launch admission decision has nothing to do with execution
//! ownership/identity.
//!
//! This is the daemon-side admission primitive only (PR1 of HORO-1766's
//! two-PR stack). PR2 (`libra-governor run`, a CLI wrapper) is the one
//! real consumer of this IPC; nothing here assumes a particular caller.
//!
//! # Why this does not reuse `libra_governor_domain::pacing`
//!
//! See `crates/domain/tests/pacing_not_wired_live.rs` and
//! `docs/adr/0020-launch-gate-actuator.md`. `pacing` stays serialize-only
//! data with no live consumer until HORO-1727 settles the live-admission
//! architecture question — a decision this ticket does not make. The
//! ledger's own atomic `reserve_shared` (HORO-1763) is the correctness
//! guarantee here; it needs no pacing involvement.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use libra_governor_domain::{ReservationState, SharedPoolReservationId};

/// The fixed scope every [`PoolStatus`] reports (ADR-0020): this
/// admission gate only ever throttles direct children the launcher
/// itself started. It has no visibility into, and makes no claim about,
/// work started outside the launcher — see ADR-0020 for why a broader
/// claim would be dishonest given what this primitive can actually
/// observe.
pub const LAUNCH_GATE_ENFORCEMENT_SCOPE: &str = "launcher_owned_direct_children_only";

/// One request a client may send over the launch-gate IPC (embedded in
/// [`crate::Request::Launch`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum LaunchRequest {
    /// Ask to start one job against `pool_alias`'s slot pool.
    /// `launch_id` doubles as the ledger `session_id` AND the
    /// idempotency key for this attempt — a caller that lost a response
    /// (e.g. a timed-out socket read) retries with the exact same
    /// `launch_id` to safely discover what actually happened rather than
    /// risking a double-admit. See [`LaunchOutcome`]'s docs on the
    /// `Lost` variant for why a *new* launch must always mint a fresh
    /// `launch_id`.
    Admit { pool_alias: String, launch_id: Uuid },
    /// Extend an already-granted hold's TTL. Refused (never silently a
    /// no-op) unless `launch_id` matches the exact identity that holds
    /// `reservation_id` and that hold is still active.
    Renew {
        reservation_id: SharedPoolReservationId,
        launch_id: Uuid,
    },
    /// Give back a held slot early (the job finished before its hold
    /// expired). Refused unless `launch_id` matches the exact identity
    /// that holds `reservation_id` — a release can never free a slot
    /// still legitimately held by someone else (AC1).
    Release {
        reservation_id: SharedPoolReservationId,
        launch_id: Uuid,
    },
    /// Read-only: every configured pool's current admission posture.
    /// An empty struct variant, not a bare unit variant — serde's
    /// `deny_unknown_fields` does not reject an extra field smuggled
    /// alongside a unit variant's internal tag, only alongside a struct
    /// variant's (even an empty one). Same precedent as
    /// `crate::ExecutionOperation::Query {}`.
    Status {},
}

/// Why an [`LaunchOutcome::Wait`] was returned instead of `Start`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum WaitReason {
    /// The pool's own slot occupancy is fully subscribed right now.
    Throttled { active: u64, capacity: u64 },
    /// A fresh, disclosed provider gauge snapshot reports external
    /// activity Libra never observed, conservatively consuming the
    /// pool's remaining headroom — see
    /// `libra_governor_domain::PoolAdmission::external_overhang`'s own
    /// docs for why this is never zeroed out just because Libra's own
    /// ledger looks empty.
    ExternalOverhang,
}

/// Why an [`LaunchOutcome::Unavailable`] was returned instead of any kind
/// of admission decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    /// No `launcher` section exists in `config.json` at all — the gate
    /// is off by default (ADR-0020).
    NotEnabled,
    /// A `launcher` section exists, but `pool_alias`'s own entry has
    /// `enabled: false`.
    AdminDisabled,
    /// `pool_alias` does not name any pool in the configured `launcher`
    /// section, OR it names one whose underlying ledger pool was found
    /// to already exist under a different [`libra_governor_domain::QuotaUnit`]
    /// than the fixed unit the launch gate always uses — a config/ledger
    /// collision this gate refuses to paper over (ADR-0020 §"Why a unit
    /// mismatch refuses rather than coerces"). Either way, there is no
    /// pool this request can be evaluated against.
    UnknownPool,
    /// Deliberately a daemon-side-constructible variant that the daemon
    /// itself never actually constructs: every `Response` the daemon
    /// sends by definition proves it was reachable. This exists purely
    /// so PR2's CLI client has one `UnavailableReason` shape to report
    /// whether the failure was "the daemon answered `Unavailable`" or
    /// "there was no daemon to ask at all" — it maps its own connect
    /// failure onto this variant locally rather than inventing a parallel
    /// client-only error enum. See PR2 for that mapping.
    DaemonUnreachable,
    /// The request's protocol version did not match — surfaced as this
    /// reason rather than a bare [`crate::Response::Error`] so a launch
    /// client can treat "stale binary talking to a fresher daemon (or
    /// vice versa)" as a `Wait`-and-retry-after-upgrade condition instead
    /// of a hard failure.
    ProtocolMismatch,
    /// A `Renew`/`Release` named a `reservation_id` whose hold has
    /// already lapsed — see [`LaunchOutcome::Lost`] for the richer,
    /// `Admit`-side version of this same fact.
    HoldExpired,
    /// A `Renew` was refused by the ledger (wrong identity, not active,
    /// or already past its own `expires_at`) — see
    /// `libra_governor_ledger::shared_pool::SharedPoolRenewOutcome::Refused`.
    RenewFailed,
    /// A `Release` named a `reservation_id`/`launch_id` pair that did not
    /// match the hold's actual recorded identity (or named a
    /// `reservation_id` outside the launch gate's own `launch:`-prefixed
    /// pool namespace entirely) — refused; the slot stays held by
    /// whoever actually holds it (AC1). Not one of this ticket's
    /// original four illustrative reasons, added because `Release` needs
    /// its own distinct "that wasn't yours to free" signal, structurally
    /// different from every other `UnavailableReason`.
    ReleaseRefused,
}

/// The response to a [`LaunchRequest`] (embedded in
/// [`crate::Response::Launch`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum LaunchOutcome {
    /// Admitted: a slot is held. The caller must `Renew` at least every
    /// `renew_every_secs` or risk the hold being swept as stale out from
    /// under it.
    Start {
        reservation_id: SharedPoolReservationId,
        /// RFC3339, UTC. A `String` rather than a native timestamp type —
        /// this crate's wire types carry no time-library dependency; see
        /// `crate::host_event`'s `observed_at: String` fields for the
        /// same convention.
        expires_at: String,
        renew_every_secs: u64,
    },
    /// Not admitted right now; retry after `retry_after_secs`.
    Wait {
        reason: WaitReason,
        retry_after_secs: u64,
    },
    /// No admission decision could be made at all — see
    /// [`UnavailableReason`].
    Unavailable { reason: UnavailableReason },
    /// A `Renew` succeeded; the hold's new expiry.
    Renewed { expires_at: String },
    /// An `Admit` retry (same `launch_id`) found its prior hold, but that
    /// hold is no longer active (expired or released) — the idempotency
    /// lookup inside `reserve_shared` is scoped to `(pool_id,
    /// principal_id, idempotency_key)` with **no state filter**, so a
    /// replayed `launch_id` whose original hold already lapsed must
    /// never silently map back to `Start` (that would be an unguarded
    /// start against a pool that may since be fully subscribed again —
    /// the single most important correctness property in this file; see
    /// the regression test for it). It instead surfaces here, and the
    /// caller is expected to mint a **fresh** `launch_id` for a genuinely
    /// new launch attempt — retrying the same stale `launch_id` forever
    /// finds this same answer, never `Start`.
    Lost { state: ReservationState },
    /// Answers `LaunchRequest::Status`.
    StatusReport { pools: Vec<PoolStatus> },
}

/// One configured pool's admission posture, as reported by
/// `LaunchRequest::Status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolStatus {
    pub alias: String,
    /// What `config.json`'s `[launcher]` section currently declares for
    /// this alias.
    pub configured_capacity: u64,
    /// What the ledger's `quota_pools` row actually holds — `ensure_pool`
    /// never overwrites an existing pool's capacity, so a capacity edit
    /// in config is a no-op against an already-created pool. This field
    /// is what makes that silent divergence visible rather than hidden.
    pub effective_capacity: u64,
    pub active: u64,
    pub enabled: bool,
    /// Always [`LAUNCH_GATE_ENFORCEMENT_SCOPE`] today — carried as data
    /// (not just a doc comment) so a future broader scope is a visible
    /// wire change, not a silent reinterpretation of an unchanged field.
    pub enforcement_scope: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admit_request_round_trips() {
        let launch_id = Uuid::nil();
        let req = LaunchRequest::Admit {
            pool_alias: "agents".to_string(),
            launch_id,
        };
        let json = serde_json::to_string(&req).unwrap();
        let round_tripped: LaunchRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, round_tripped);
    }

    #[test]
    fn status_request_has_no_extra_fields_and_rejects_unknown_ones() {
        let json = serde_json::to_string(&LaunchRequest::Status {}).unwrap();
        assert_eq!(json, r#"{"op":"status"}"#);
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value["extra"] = serde_json::json!("nope");
        assert!(serde_json::from_value::<LaunchRequest>(value).is_err());
    }

    #[test]
    fn start_outcome_round_trips() {
        let outcome = LaunchOutcome::Start {
            reservation_id: SharedPoolReservationId::new(),
            expires_at: "2026-10-14T00:00:00Z".to_string(),
            renew_every_secs: 60,
        };
        let json = serde_json::to_string(&outcome).unwrap();
        let round_tripped: LaunchOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(outcome, round_tripped);
    }

    #[test]
    fn full_request_response_round_trips_through_the_top_level_envelope() {
        let launch_id = Uuid::new_v4();
        let envelope = crate::RequestEnvelope {
            protocol_version: crate::PROTOCOL_VERSION,
            request: crate::Request::Launch {
                launch: LaunchRequest::Admit {
                    pool_alias: "agents".to_string(),
                    launch_id,
                },
            },
        };
        let json = serde_json::to_string(&envelope).unwrap();
        let round_tripped: crate::RequestEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(envelope, round_tripped);

        let response_envelope = crate::ResponseEnvelope {
            protocol_version: crate::PROTOCOL_VERSION,
            response: crate::Response::Launch(Box::new(LaunchOutcome::Wait {
                reason: WaitReason::Throttled {
                    active: 2,
                    capacity: 2,
                },
                retry_after_secs: 5,
            })),
        };
        let json = serde_json::to_string(&response_envelope).unwrap();
        let round_tripped: crate::ResponseEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(response_envelope, round_tripped);
    }

    #[test]
    fn status_report_carries_the_fixed_enforcement_scope() {
        let report = LaunchOutcome::StatusReport {
            pools: vec![PoolStatus {
                alias: "agents".to_string(),
                configured_capacity: 2,
                effective_capacity: 2,
                active: 0,
                enabled: true,
                enforcement_scope: LAUNCH_GATE_ENFORCEMENT_SCOPE.to_string(),
            }],
        };
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(
            json["pools"][0]["enforcement_scope"],
            serde_json::json!(LAUNCH_GATE_ENFORCEMENT_SCOPE)
        );
    }
}
