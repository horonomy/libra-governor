//! Local monthly shared-budget IPC (HORO-2195, ADR-0026).
//!
//! A locally configured monthly USD hard cap shared by every participating
//! session and agent on this host. Admission is refuse-before-spend at
//! prompt admission and cooperative: in-turn spend is visible only after
//! the fact, so spend can overshoot the hold. The labels on
//! [`SharedBudgetStatus`] say exactly that and must not be strengthened.
//!
//! Amounts are Libra-accounted, list-price-equivalent USD cents at the
//! pinned pricing version -- never the provider's bill.
//!
//! The generic `MeteredPool` request refuses the `local-budget` owner;
//! only [`SharedBudgetRequest`] may mutate that pool.
use serde::{Deserialize, Serialize};

use libra_governor_domain::{ChargeSource, ExecutorKind, SharedPoolReservationId};

/// What a [`SharedBudgetStatus`] amount means.
pub const SHARED_BUDGET_BASIS: &str = "libra-accounted-usd-list-price-equivalent";
/// What kind of limit this is: refused before spend, but not a hard cap.
pub const SHARED_BUDGET_GUARANTEE: &str = "admission-bounded-cooperative";
/// Whose spend the budget can see.
pub const SHARED_BUDGET_SCOPE: &str = "this-host-this-os-user";

/// One request a client may send over the shared-budget IPC (embedded in
/// [`crate::Request::SharedBudget`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum SharedBudgetRequest {
    /// Admit one turn of an executor lane `(session_id, agent_id | none)`.
    /// A lane holds at most one active hold: a second `Admit` for a lane
    /// with an unexpired hold reuses (renews) it rather than reserving
    /// again, because `Stop` does not fire on interrupt. A missing
    /// `agent_id` means the session's main executor, never a root agent.
    Admit {
        session_id: String,
        agent_id: Option<String>,
        /// Makes a retried `Admit` idempotent. When absent a fresh nonce
        /// is used.
        prompt_id: Option<String>,
    },
    /// Cheap read for tool-time checks: remaining budget and state only.
    /// Never creates a pool and never reserves.
    Check {
        session_id: String,
        agent_id: Option<String>,
    },
    /// Record spend that already happened (a subagent's transcript usage,
    /// for example) as settled spend attributed to an executor. Never
    /// refused for lack of capacity: the money is already spent.
    ///
    /// Booked to the period of `event_time` (RFC 3339) when given,
    /// otherwise to the pool of the session's latest hold, otherwise to
    /// the current period. With no `agent_id` the row is `unattributed`
    /// for a `subagent_transcript` charge (a subagent whose id is unknown)
    /// and the session's main executor otherwise.
    ChargeAgent {
        session_id: String,
        agent_id: Option<String>,
        amount_usd_cents: u64,
        idempotency_key: String,
        charge_source: ChargeSource,
        event_time: Option<String>,
    },
    /// Charge a subagent's own transcript (HORO-2196). The daemon reads
    /// `agent_transcript_path` itself (the caller supplies a path, never an
    /// amount), prices the file's usage cumulatively and books only the
    /// part not yet charged for this `(session, agent)`, so a repeated
    /// `SubagentStop` never double-charges and a later, larger transcript
    /// charges only the difference. An unreadable, over-cap, unpriced or
    /// unverifiable transcript is never silently omitted: a conservative
    /// fallback amount is charged once instead, flagged `unmeasured`.
    ///
    /// With no `agent_id` the row is `unattributed` and the daemon never
    /// invents an id.
    ChargeAgentTranscript {
        session_id: String,
        agent_id: Option<String>,
        agent_transcript_path: String,
    },
    /// Settle the tail of a session's main transcript (HORO-2196): the
    /// records past the lane's watermark that no `Stop` has counted yet
    /// (the host flushes its transcript asynchronously, so the final
    /// message of a session is often not in the file at the last `Stop`).
    /// Meant for `SessionEnd`. Charged as a settled `agent_absent` row
    /// (`charge_source = turn_hold`, `usage_known = true`) with the same
    /// session binding, de-duplication and watermark as a `Stop`
    /// settlement; idempotent. Nothing is charged when nothing is
    /// measurable: a tail has no hold to fall back to.
    SettleTail {
        session_id: String,
        transcript_path: String,
    },
    /// Read-only budget posture.
    Status {},
}

/// Why an `Admit` or `Check` was denied (nothing is charged).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SharedBudgetDenyReason {
    /// A shared-budget file exists but is invalid.
    ConfigInvalid,
    /// A pool of a later period than the clock's current one exists.
    ClockAnomaly,
    /// Remaining budget is below the turn hold.
    CapExhausted,
}

/// Why a request was refused as malformed or inconsistent (nothing is
/// charged).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SharedBudgetRefusal {
    EmptySessionId,
    /// The session id uses the reserved `execution-owner-v1:` prefix.
    ReservedSessionScope,
    /// `agent_id` is present but empty.
    InvalidAgentId,
    InvalidAmount,
    /// One charge above ten times the monthly cap, or one that would push
    /// the pool's total past the range the ledger can add safely.
    AmountTooLarge,
    InvalidIdempotencyKey,
    /// `event_time` is not RFC 3339.
    InvalidEventTime,
    /// The idempotency key already names a different charge or a hold.
    KeyConflict,
    /// The period's pool exists in a unit other than USD cents.
    PoolUnitMismatch,
}

/// State reported by `Check`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SharedBudgetCheckState {
    /// The lane holds an unexpired admission for the current period.
    Admitted,
    /// No lane hold, but budget remains.
    Available,
    /// No lane hold and less than a turn hold remains, so a new `Admit`
    /// would be denied.
    Exhausted,
}

/// Which soft limit a warning is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SoftLimitScope {
    PerSession,
    PerAgent,
}

/// A warn-only soft-limit notice. Never changes an admission decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SoftLimitWarning {
    pub scope: SoftLimitScope,
    pub session_id: String,
    pub agent_id: Option<String>,
    pub used_plus_reserved_usd_cents: u64,
    pub threshold_usd_cents: u64,
}

/// Whether the shared-budget file in force is present and usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SharedBudgetConfigState {
    Absent,
    Valid,
    Invalid,
}

/// How the file on disk compares with the configuration the daemon runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SharedBudgetConfigDrift {
    None,
    Changed,
    Appeared,
    Removed,
    Unreadable,
}

/// The running configuration, as validated at daemon start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedBudgetConfigView {
    pub monthly_cap_usd_cents: u64,
    pub time_zone: String,
    pub turn_hold_usd_cents: u64,
    pub soft_limit_per_session_usd_cents: Option<u64>,
    pub soft_limit_per_agent_usd_cents: Option<u64>,
    pub subagent_unmeasured_charge_usd_cents: Option<u64>,
    /// How many models the file's `pricing_overrides` price (HORO-2196).
    #[serde(default)]
    pub pricing_override_count: u64,
}

/// The current budget period.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedBudgetPeriod {
    pub pool_id: String,
    pub start: String,
    pub end: String,
}

/// One executor's spend in the current period.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedBudgetExecutorRow {
    pub session_id: String,
    pub agent_id: Option<String>,
    pub executor_kind: ExecutorKind,
    pub used_usd_cents: u64,
    pub reserved_usd_cents: u64,
}

/// The `Status` report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedBudgetStatus {
    pub basis: String,
    pub guarantee: String,
    pub scope: String,
    pub config_state: SharedBudgetConfigState,
    pub config_invalid_reason: Option<String>,
    /// Drift of the file on disk against what the daemon read at start.
    /// Informational: the daemon re-reads the file on every request, so
    /// `config_state` and `config` already describe the file in force.
    pub config_drift: SharedBudgetConfigDrift,
    /// The file in force differs from what was read at daemon start
    /// (created, edited, broken or deleted since).
    pub config_changed_since_start: bool,
    pub config_path: Option<String>,
    pub config_permissions_note: Option<String>,
    pub config: Option<SharedBudgetConfigView>,
    /// A pool of a later period than the clock's current one exists;
    /// admission denies until that is resolved.
    pub clock_anomaly: bool,
    /// Alias (`monthly-YYYY-MM`) of the newest pool when it is of a later
    /// period than the clock's, which is what causes `clock_anomaly`.
    pub future_pool_detected: Option<String>,
    pub period: Option<SharedBudgetPeriod>,
    /// Whether the current period's pool has been created yet (it is
    /// created lazily by the first `Admit`).
    pub pool_exists: bool,
    /// False when there is no usable configuration, so no capacity is
    /// known and none of the figures below are authoritative.
    pub capacity_known: bool,
    pub used_usd_cents: u64,
    pub reserved_usd_cents: u64,
    /// `cap - used - reserved`; negative on overrun. `None` without a
    /// usable configuration.
    pub remaining_usd_cents: Option<i64>,
    /// Sums to `used_usd_cents` / `reserved_usd_cents`, unattributed
    /// included.
    pub executors: Vec<SharedBudgetExecutorRow>,
    pub warnings: Vec<SoftLimitWarning>,
    /// Settlement telemetry (HORO-2196). Held in daemon memory: it counts
    /// since the daemon started and restarts at zero. The ledger rows
    /// (`usage_known`) are the durable record.
    ///
    /// Models seen in a transcript that the pricing table (plus the file's
    /// `pricing_overrides`) could not price, bounded and sanitised. A
    /// window containing one settled at the hold amount.
    #[serde(default)]
    pub unpriced_models: Vec<String>,
    /// Transcripts refused because their per-line session field named a
    /// different session than the hook's.
    #[serde(default)]
    pub transcript_binding_mismatches: u64,
    /// Windows or subagent files whose session could not be verified
    /// (field absent or unreadable); settled/charged conservatively.
    #[serde(default)]
    pub transcript_unverified: u64,
    /// Turn windows settled at the hold amount with `usage_known=false`
    /// because they could not be measured.
    #[serde(default)]
    pub unmeasured_settlements: u64,
    /// Subagent fallback charges written because the transcript could not
    /// be measured.
    #[serde(default)]
    pub unmeasured_charges: u64,
    /// Counted transcript records that carried no `message.id` and so
    /// could not be de-duplicated.
    #[serde(default)]
    pub id_less_records: u64,
}

/// The response to a [`SharedBudgetRequest`] (embedded in
/// [`crate::Response::SharedBudget`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum SharedBudgetOutcome {
    /// No shared-budget file: the budget is not in force and nothing was
    /// consulted or written.
    NotConfigured,
    Allowed {
        reservation_id: SharedPoolReservationId,
        pool_id: String,
        hold_usd_cents: u64,
        /// True when an existing unexpired lane hold was renewed instead
        /// of reserving again.
        hold_reused: bool,
        executor_kind: ExecutorKind,
        remaining_usd_cents: i64,
        period_end: String,
        warnings: Vec<SoftLimitWarning>,
        /// The hold was granted but the soft-limit warnings could not be
        /// computed; `warnings` is empty and says nothing about limits.
        warnings_unavailable: bool,
    },
    Denied {
        reason: SharedBudgetDenyReason,
        remaining_usd_cents: Option<i64>,
        period_end: Option<String>,
        detail: Option<String>,
    },
    Refused {
        reason: SharedBudgetRefusal,
    },
    Checked {
        state: SharedBudgetCheckState,
        remaining_usd_cents: i64,
        period_end: String,
    },
    Charged {
        reservation_id: SharedPoolReservationId,
        pool_id: String,
        amount_usd_cents: u64,
        /// True for an identical replay; nothing was written.
        already_charged: bool,
        executor_kind: ExecutorKind,
        remaining_usd_cents: i64,
        overrun_usd_cents: u64,
        capacity_known: bool,
        /// The event time named a later period than now, so the spend was
        /// booked to the current period instead of being dropped.
        event_time_clamped: bool,
    },
    /// Answer to [`SharedBudgetRequest::ChargeAgentTranscript`].
    TranscriptCharged {
        /// The row written; `None` when nothing new was booked.
        reservation_id: Option<SharedPoolReservationId>,
        pool_id: Option<String>,
        /// What this request added (the delta, or the fallback amount).
        amount_charged_usd_cents: u64,
        /// Everything now charged for this `(session, agent)` subagent
        /// transcript, including earlier calls and fallbacks.
        cumulative_usd_cents: u64,
        /// Nothing was written because the amount was already booked.
        already_charged: bool,
        executor_kind: ExecutorKind,
        remaining_usd_cents: i64,
        overrun_usd_cents: u64,
        /// False when the charge is the unmeasured fallback.
        usage_known: bool,
        unmeasured: bool,
        /// A model in the transcript could not be priced.
        unpriced: bool,
        /// The transcript's session could not be verified.
        transcript_unverified: bool,
        /// The transcript's per-line session field named a different
        /// session than the request. Its spend was not imported; the
        /// unmeasured fallback was charged instead.
        #[serde(default)]
        binding_mismatch: bool,
        event_time_clamped: bool,
    },
    /// Answer to [`SharedBudgetRequest::SettleTail`].
    TailSettled {
        /// The row written; `None` when nothing was charged.
        reservation_id: Option<SharedPoolReservationId>,
        pool_id: Option<String>,
        amount_charged_usd_cents: u64,
        /// An identical earlier tail charge was found (a retry).
        already_charged: bool,
        remaining_usd_cents: i64,
        overrun_usd_cents: u64,
        /// Records past the watermark exist but could not be measured
        /// (unreadable, over the cap, malformed, unpriced, unverifiable);
        /// nothing was charged for them.
        unmeasured: bool,
        unpriced: bool,
        transcript_unverified: bool,
        /// The transcript named another session; nothing was imported.
        binding_mismatch: bool,
    },
    Status(Box<SharedBudgetStatus>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_use_the_snake_case_op_tag_and_reject_unknown_fields() {
        let admit = SharedBudgetRequest::Admit {
            session_id: "s".into(),
            agent_id: None,
            prompt_id: Some("p".into()),
        };
        let json = serde_json::to_value(&admit).unwrap();
        assert_eq!(json["op"], "admit");
        let back: SharedBudgetRequest = serde_json::from_value(json).unwrap();
        assert_eq!(back, admit);

        assert!(serde_json::from_str::<SharedBudgetRequest>(
            r#"{"op":"admit","session_id":"s","bogus":1}"#
        )
        .is_err());
        // Optional fields may be omitted entirely.
        let minimal: SharedBudgetRequest =
            serde_json::from_str(r#"{"op":"check","session_id":"s"}"#).unwrap();
        assert_eq!(
            minimal,
            SharedBudgetRequest::Check {
                session_id: "s".into(),
                agent_id: None
            }
        );
        let status: SharedBudgetRequest = serde_json::from_str(r#"{"op":"status"}"#).unwrap();
        assert_eq!(status, SharedBudgetRequest::Status {});
    }

    #[test]
    fn charge_request_carries_a_typed_charge_source() {
        let json = r#"{"op":"charge_agent","session_id":"s","agent_id":"a","amount_usd_cents":7,
            "idempotency_key":"k","charge_source":"subagent_transcript"}"#;
        let req: SharedBudgetRequest = serde_json::from_str(json).unwrap();
        assert!(matches!(
            req,
            SharedBudgetRequest::ChargeAgent {
                charge_source: ChargeSource::SubagentTranscript,
                event_time: None,
                ..
            }
        ));
        assert!(serde_json::from_str::<SharedBudgetRequest>(
            &json.replace("subagent_transcript", "made_up")
        )
        .is_err());
    }

    #[test]
    fn outcomes_round_trip() {
        let denied = SharedBudgetOutcome::Denied {
            reason: SharedBudgetDenyReason::CapExhausted,
            remaining_usd_cents: Some(-3),
            period_end: Some("2027-02-01T00:00:00Z".into()),
            detail: None,
        };
        let json = serde_json::to_value(&denied).unwrap();
        assert_eq!(json["outcome"], "denied");
        assert_eq!(json["reason"], "cap_exhausted");
        assert_eq!(
            serde_json::from_value::<SharedBudgetOutcome>(json).unwrap(),
            denied
        );
        let not_configured = serde_json::to_value(SharedBudgetOutcome::NotConfigured).unwrap();
        assert_eq!(not_configured["outcome"], "not_configured");
    }
    #[test]
    fn transcript_charge_request_round_trips_and_rejects_unknown_fields() {
        let req = SharedBudgetRequest::ChargeAgentTranscript {
            session_id: "s".into(),
            agent_id: Some("a".into()),
            agent_transcript_path: "/x/agent-a.jsonl".into(),
        };
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json["op"], "charge_agent_transcript");
        assert_eq!(
            serde_json::from_value::<SharedBudgetRequest>(json).unwrap(),
            req
        );
        // The caller supplies a path, never an amount.
        assert!(serde_json::from_str::<SharedBudgetRequest>(
            r#"{"op":"charge_agent_transcript","session_id":"s","agent_transcript_path":"/p",
            "amount_usd_cents":5}"#
        )
        .is_err());
        let minimal: SharedBudgetRequest = serde_json::from_str(
            r#"{"op":"charge_agent_transcript","session_id":"s","agent_transcript_path":"/p"}"#,
        )
        .unwrap();
        assert!(matches!(
            minimal,
            SharedBudgetRequest::ChargeAgentTranscript { agent_id: None, .. }
        ));
    }

    #[test]
    fn transcript_outcome_round_trips() {
        let charged = SharedBudgetOutcome::TranscriptCharged {
            reservation_id: None,
            pool_id: None,
            amount_charged_usd_cents: 0,
            cumulative_usd_cents: 750,
            already_charged: true,
            executor_kind: ExecutorKind::Agent,
            remaining_usd_cents: 99_250,
            overrun_usd_cents: 0,
            usage_known: true,
            unmeasured: false,
            unpriced: false,
            transcript_unverified: false,
            binding_mismatch: true,
            event_time_clamped: false,
        };
        let json = serde_json::to_value(&charged).unwrap();
        assert_eq!(json["outcome"], "transcript_charged");
        assert_eq!(
            serde_json::from_value::<SharedBudgetOutcome>(json).unwrap(),
            charged
        );
    }

    #[test]
    fn a_status_from_an_older_daemon_decodes_with_zeroed_settlement_telemetry() {
        let mut status = serde_json::to_value(SharedBudgetStatus {
            basis: SHARED_BUDGET_BASIS.into(),
            guarantee: SHARED_BUDGET_GUARANTEE.into(),
            scope: SHARED_BUDGET_SCOPE.into(),
            config_state: SharedBudgetConfigState::Absent,
            config_invalid_reason: None,
            config_drift: SharedBudgetConfigDrift::None,
            config_changed_since_start: false,
            config_path: None,
            config_permissions_note: None,
            config: None,
            clock_anomaly: false,
            future_pool_detected: None,
            period: None,
            pool_exists: false,
            capacity_known: false,
            used_usd_cents: 0,
            reserved_usd_cents: 0,
            remaining_usd_cents: None,
            executors: vec![],
            warnings: vec![],
            unpriced_models: vec!["m".into()],
            transcript_binding_mismatches: 3,
            transcript_unverified: 2,
            unmeasured_settlements: 4,
            unmeasured_charges: 5,
            id_less_records: 6,
        })
        .unwrap();
        let obj = status.as_object_mut().unwrap();
        for k in [
            "unpriced_models",
            "transcript_binding_mismatches",
            "transcript_unverified",
            "unmeasured_settlements",
            "unmeasured_charges",
            "id_less_records",
        ] {
            obj.remove(k);
        }
        let back: SharedBudgetStatus = serde_json::from_value(status).unwrap();
        assert!(back.unpriced_models.is_empty());
        assert_eq!(
            (
                back.transcript_binding_mismatches,
                back.transcript_unverified,
                back.unmeasured_settlements,
                back.unmeasured_charges
            ),
            (0, 0, 0, 0)
        );
    }
}
