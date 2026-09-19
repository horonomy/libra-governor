//! `libra-governor-extension` — local, outbound-only extension points
//! (HORO-1174): a Business Context Provider fetch, a Policy Webhook call,
//! and signed event delivery, all over loopback HTTP; plus the wire types
//! for the one new inbound path (`Request::RecordOutcome`, which reuses
//! the existing Unix socket rather than opening a new HTTP listener).
//!
//! # This crate does not decide
//!
//! Depends on `libra-governor-domain` and nothing else of Libra's — no
//! `libra-governor-ledger`, no `libra-governor-daemon`. An external
//! provider's HTTP response structurally cannot open the ledger,
//! construct a `Policy`, or decide an admission through this crate. See
//! `docs/adr/0005-local-extension-points.md`.
//!
//! # No policy logic, no new inbound HTTP listener
//!
//! Every function in this crate either builds an outbound request, signs
//! one, sends one, or parses one's response into a plain data type. No
//! function here evaluates a `Policy`, decides an `Admission`, or opens a
//! network listener.

mod client;
mod config;
mod dispatcher;
mod event;
mod queue;
mod secret;
mod sign;
mod wire;

pub use client::{ClientError, ProviderClient};
pub use config::{
    truncate_chars, validate, ConfigError, EventsConfig, ExtensionConfig, SurfaceConfig,
    ValidatedEventsConfig, ValidatedExtensionConfig, ValidatedSurfaceConfig,
    BUSINESS_CONTEXT_TIMEOUT_CAP_MS, BUSINESS_CONTEXT_TIMEOUT_DEFAULT_MS, CLI_REQUEST_TIMEOUT,
    EVENTS_MAX_ATTEMPTS_DEFAULT, EVENTS_TIMEOUT_DEFAULT_MS, MAX_ADVISORY_CRITERIA,
    MAX_ADVISORY_CRITERION_CHARS, MAX_RESPONSE_BYTES, POLICY_WEBHOOK_TIMEOUT_CAP_MS,
    POLICY_WEBHOOK_TIMEOUT_DEFAULT_MS, RECON_MAX_DURATION, REQUIRED_SLACK, WIRE_SCHEMA_VERSION,
};
pub use dispatcher::run_dispatcher;
pub use event::{
    AdmissionEventData, ApprovalEventData, BusinessContextEventRef, EventEnvelope, EventKind,
    ExternalApprovalEventRef, OutcomeEventData, ReplanEventData,
};
pub use queue::{DeliveryQueue, QueueError, QueuedDelivery};
pub use secret::{SecretError, WebhookSecret, WebhookSecretCommand};
pub use sign::{sign, SignedHeaders, SIGNATURE_VERSION};
pub use wire::{
    BusinessContextRequest, BusinessContextResponse, PolicyWebhookRequest, PolicyWebhookResponse,
    WireVerdict,
};
