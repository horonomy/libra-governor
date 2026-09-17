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

pub use messages::{
    Confidence, PreflightResult, ReconSummary, Request, RequestEnvelope, Response,
    ResponseEnvelope, StatusResult, TaskSummary,
};

/// The protocol version this build of the crate speaks. Bump on any
/// breaking change to [`Request`] or [`Response`] shapes.
pub const PROTOCOL_VERSION: u32 = 1;
