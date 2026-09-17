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

pub use libra_governor_domain::{Confidence, Estimate};
pub use libra_governor_estimator::{
    AdmissionOutcome, AdmissionPolicy, AdmissionStats, CoverageReport, QuantileCoverage, Stratum,
};
pub use messages::{
    AdmissionPolicyReport, CalibrationReportResult, FinalizeOutcome, FinalizeResult,
    PreflightResult, ReconSummary, Request, RequestEnvelope, Response, ResponseEnvelope,
    StatusResult, TaskSummary,
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
pub const PROTOCOL_VERSION: u32 = 3;
