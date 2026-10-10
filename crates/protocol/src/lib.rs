//! `libra-governor-protocol` — the versioned request/response protocol
//! spoken between a Libra client (the `libra-governor` CLI's `hook` and
//! `statusline` subcommands) and the local Governor daemon.
//!
//! # Transport
//!
//! JSON over a Unix domain socket, one request per connection: a client
//! connects, writes exactly one newline-delimited JSON [`RequestEnvelope`],
//! reads exactly one newline-delimited JSON [`ResponseEnvelope`], and
//! closes. See [`wire`] for the framing helpers both the daemon and the
//! CLI client use.
//!
//! # Versioning
//!
//! Every envelope carries a required `protocol_version` field (see
//! [`PROTOCOL_VERSION`]). It is not defaulted and not optional: a client
//! or daemon on a different protocol version must fail loudly (a
//! [`Response::Error`]) rather than silently misinterpret a message shape
//! it does not actually understand.

pub mod execution_owner;
pub mod host_event;
pub use execution_owner::{
    ExecutionEffect, ExecutionOperation, ExecutionOwnerOutcome, ExecutionOwnerRequest,
    NativeExecutionContext,
};
mod messages;
pub mod wire;

pub use host_event::{
    validate_host_event, validate_host_json, validate_host_snapshot, HostBindingFailure,
    HostBindingReason, HostBindingStage, HostCapabilitySnapshot, HostEventKind, HostEventScope,
    ValidatedHostCapabilitySnapshot, ValidatedHostEvent,
};
pub use libra_governor_domain::Estimate;
pub use libra_governor_domain::{
    BudgetSnapshot, BusinessContextSummary, Confidence, CredentialCustody, EnforcementCapabilities,
    EnforcementTier, ExecutionOutcome, Headroom, MonetaryEnforcement, NoMonetaryCap, PlanId,
    PolicyDecision, ResourceAmount, ResourceKind, TaskId, UsageAccounting,
};
pub use libra_governor_estimator::{
    AdmissionOutcome, AdmissionPolicy, AdmissionStats, CoverageReport, QuantileCoverage, Stratum,
};
pub use messages::{
    AdmissionPolicyReport, BudgetPosture, BudgetScope, CalibrationReportResult, ConfiguredBudget,
    DoctorResult, FinalizeOutcome, FinalizeResult, GatewayStatusResult, OutcomeRecordedOutcome,
    OutcomeRecordedResult, PreflightResult, ReconSummary, ReplanState, Request, RequestEnvelope,
    Response, ResponseEnvelope, SignedOutcomeClaimWire, StatusResult, StatusScope, TaskSummary,
};

/// The protocol version this build of the crate speaks. Bump on any
/// breaking change to [`Request`] or [`Response`] shapes.
///
/// Bumped 1 -> 2 for HORO-1126: `Request` gained `ToolInvoked`/`Finalize`
/// variants and `Response` gained `Finalize`/`Ack`, which a v1 peer cannot
/// decode. Known limitation: a long-lived v1 daemon left running across
/// this upgrade will reject every v2 client request as a version
/// mismatch — see the HORO-1126 PR description. The daemon must be
/// restarted (killed, then re-spawned on the next hook invocation) after
/// upgrading.
///
/// Bumped 2 -> 3 for HORO-1132: `Request` gained `CalibrationReport` and
/// `Response` gained the matching `CalibrationReport` variant. Same known
/// limitation as the 1 -> 2 bump: a long-lived v2 daemon must be
/// restarted after upgrading.
///
/// Bumped 3 -> 4 for HORO-1139: `PreflightResult` gained `plan_id` and
/// `TaskSummary` gained `plan_id`/`remaining_estimate`/`replan_state` —
/// a v3 client would silently fail to deserialize these new required
/// fields. Same known limitation as the earlier bumps: a long-lived v3
/// daemon must be restarted after upgrading.
///
/// Bumped 4 -> 5 for HORO-1141: `PreflightResult` gained
/// `admission`/`completion_reserve` and `ExecutionReceipt` (embedded in
/// `FinalizeResult` -> `Response::Finalize`) gained `reservations` — a
/// v4 client would silently fail to deserialize these new required
/// fields. Same known limitation as the earlier bumps: a long-lived v4
/// daemon must be restarted after upgrading.
///
/// Bumped 5 -> 6 for HORO-1144: `Request` gained `GatewayStatus` and
/// `Response` gained the matching `GatewayStatus` variant, which a v5
/// peer cannot decode. Same known limitation as the earlier bumps: a
/// long-lived v5 daemon must be restarted after upgrading.
///
/// Bumped 6 -> 7 for HORO-1150: `Request` gained `Doctor` and `Response`
/// gained the matching `Doctor` variant, which a v6 peer cannot decode.
/// Same known limitation as the earlier bumps: a long-lived v6 daemon
/// must be restarted after upgrading — `libra-governor doctor` itself
/// surfaces this plainly (a protocol-version-mismatch `Response::Error`
/// renders as a failed "daemon reachable" check rather than a crash).
///
/// Bumped 7 -> 8 for HORO-1174 (local extension points): three
/// independent breaking shape changes, following the same precedent as
/// every bump above. `Request` gained `RecordOutcome` and `Response`
/// gained the matching `OutcomeRecorded` variant, which a v7 peer cannot
/// decode; `PreflightResult` gained `business_context`; `DoctorResult`
/// gained five new fields
/// (`extension_business_context_configured`/`extension_policy_webhook_configured`/
/// `extension_events_configured`/`extension_events_pending`/
/// `extension_config_error`). Same known limitation as every earlier
/// bump: a long-lived v7 daemon must be restarted after upgrading.
///
/// Bumped 8 -> 9 for HORO-1634: `StatusResult` gained `task_budget`, a
/// required field a v8 client would silently fail to deserialize. The
/// field rides the existing `Status` round trip deliberately — the
/// statusline provider's whole probe is budgeted at 200 ms across two
/// round trips, and a budget figure that cost a third request would not
/// be worth a statusline. Same known limitation as every earlier bump: a
/// long-lived v8 daemon must be restarted after upgrading, which the
/// statusline provider reports as its own `daemon_protocol_mismatch`
/// no-reading rather than as "no task".
///
/// Bumped 9 -> 10 for HORO-1725: `Request::Finalize` gained
/// `transcript_path`, a required field a v9 peer cannot decode. It is
/// what lets a receipt record *measured* token usage instead of an empty
/// `actual_usage`, which in turn is what lets
/// `Estimate::resource_p80` ever be `Some` — before this, every task on
/// a live store reserved the identical policy-target constant. Same known
/// limitation as every earlier bump: a long-lived v9 daemon must be
/// restarted after upgrading.
///
/// Bumped 10 -> 11 for HORO-1709: [`messages::StatusResult`] gained
/// `task_budget_amounts` and `configured_budget`. They are what let a
/// rendering surface say how much of an envelope is left *in the unit the
/// envelope is denominated in* rather than only as a share of itself —
/// and, separately, let an idle daemon report the envelope that would
/// govern the next task without that being mistaken for an active task's
/// remaining capacity.
///
/// Unlike most bumps above, the shape change here would **not** fail a
/// decode on its own, and the doc should not claim otherwise: both new
/// fields are `Option`, and serde defaults a missing `Option` to `None`
/// rather than erroring. A v11 client handed a v10 `StatusResult` would
/// deserialize it happily — and then report "no amounts available" about
/// a daemon whose only defect is being old. That silent downgrade of
/// *stale* to *unknown* is precisely what this bump exists to prevent:
/// the envelope's version check rejects the exchange outright, and the
/// statusline provider renders a `daemon_protocol_mismatch` no-reading
/// that names the real problem.
///
/// (The same correction applies to the 8 -> 9 entry above, which
/// described `task_budget` — also an `Option` — as a field a v8 client
/// "would silently fail to deserialize". The bump was right; that
/// reasoning for it was not.)
///
/// Same known limitation as every earlier bump: a long-lived v10 daemon
/// must be restarted after upgrading.
///
/// Bumped 11 -> 12 for the identity-v1 owner request and typed association
/// results. Status now requires an explicit host-latest scope, and unknown
/// request dimensions fail decoding rather than silently narrowing identity.
/// A long-lived v11 daemon must be restarted before owner traffic.
///
/// Bumped 12 -> 13 for ADR-0017/HORO-1727's outcome-attestation trust
/// boundary and conflict-semantics correction: `OutcomeRecordedOutcome`
/// gained `NoSuchPlan`/`IdempotencyKeyReused`, and `OutcomeRecordedResult`
/// gained `authoritative`/`contract_revision` — a v12 peer's
/// `deny_unknown_fields`-enforced decoder cannot parse either. Same known
/// limitation as every earlier bump: a long-lived v12 daemon must be
/// restarted after upgrading.
///
/// Bumped 13 -> 14 for HORO-1727 PR 5b's verified-provider signature
/// plumbing: `Request::RecordOutcome` gained `signed_claim: Option<SignedOutcomeClaimWire>`
/// — a v13 peer's `deny_unknown_fields`-enforced decoder cannot parse it.
/// Still production-unreachable either way (ADR-0017): nothing populates
/// `DaemonConfig::outcome_authority` with `Some` outside test code, so
/// the new field is always `None` on a real push today. Same known
/// limitation as every earlier bump: a long-lived v13 daemon must be
/// restarted after upgrading.
pub const PROTOCOL_VERSION: u32 = 14;
