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

mod messages;
pub mod wire;

pub use libra_governor_domain::Estimate;
pub use libra_governor_domain::{
    BusinessContextSummary, Confidence, CredentialCustody, EnforcementCapabilities,
    EnforcementTier, ExecutionOutcome, MonetaryEnforcement, NoMonetaryCap, PlanId, PolicyDecision,
    ResourceAmount, TaskId, UsageAccounting,
};
pub use libra_governor_estimator::{
    AdmissionOutcome, AdmissionPolicy, AdmissionStats, CoverageReport, QuantileCoverage, Stratum,
};
pub use messages::{
    AdmissionPolicyReport, CalibrationReportResult, DoctorResult, FinalizeOutcome, FinalizeResult,
    GatewayStatusResult, OutcomeRecordedOutcome, OutcomeRecordedResult, PreflightResult,
    ReconSummary, ReplanState, Request, RequestEnvelope, Response, ResponseEnvelope, StatusResult,
    TaskSummary,
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
pub const PROTOCOL_VERSION: u32 = 8;
