//! `libra-governor-quota-source` — the versioned quota-source contract
//! (HORO-1764), built on top of HORO-1762's `quota_window` contract
//! (`libra_governor_domain::{ProviderSnapshot, GaugeReading, QuotaWindow}`).
//!
//! # Scope — read this before anything else in this crate
//!
//! This repository's own campaign governance for v0.0.4 (HORO-1749) says:
//! "if actual quota endpoints or authorization require owner action,
//! isolate that ticket and continue simulation/domain work without
//! pretending to have live data." HORO-1764 is exactly that case: there
//! is no documented, approved, credentialed internal proxy endpoint
//! available to this build. Accordingly:
//!
//! - This crate has **no live network adapter**. [`QuotaSource`] is a
//!   trait a future, separately-reviewed crate could implement against a
//!   real endpoint; nothing in this crate implements it against a real
//!   one.
//! - [`ingest_response`] is a **pure function** over a caller-supplied
//!   [`RawSourceResponse`] — never a client that makes a network call.
//!   See `tests/no_network_symbols.rs` (cloned from
//!   `crates/evidence-adapter`'s own guard) for the enforced, scanned
//!   proof.
//! - [`fixture::import_fixture`] reads a local JSON file the operator
//!   supplies and labels it [`ReadingOrigin::FixtureImport`] — never
//!   presented as live data.
//! - [`authorization::LiveEndpointAuthorization`] exists so that *when* a
//!   real adapter is eventually written, it structurally cannot compile
//!   without an explicit, separately-configured endpoint + consent value
//!   — see that module's docs and its `compile_fail` doctest.
//!
//! # What a reading actually is
//!
//! [`AcquiredReading`] wraps a HORO-1762 [`ProviderSnapshot`] with
//! [`Provenance`]: exactly which capability produced it, who/what it is
//! scoped to, and who is responsible for trusting it. A `ProviderSnapshot`
//! alone cannot satisfy this ticket's acceptance criteria — it carries no
//! source/capability/trust-owner field, by design (that's HORO-1762's own
//! contract, not duplicated here). [`AcquiredReading::validated`] is the
//! only constructor, so an invalid reading (wrong window, wrong subject,
//! a bogus timestamp) can never exist.

pub mod authorization;
pub mod fixture;
pub mod merge;
pub mod mode;
pub mod provenance;
pub mod response;
mod rfc3339;

pub use authorization::{ConsentRecord, EndpointOwner, LiveEndpointAuthorization, QuotaSource};
pub use fixture::{import_fixture, FixtureError};
pub use merge::{merge_readings, ConflictReason, Conflicted, MergeOutcome};
pub use mode::{describe_active_mode, QuotaSourceMode};
pub use provenance::{
    AcquiredReading, AcquisitionError, Provenance, ReadingOrigin, SourceDescriptor,
};
pub use response::{DegradedReason, RawSourceResponse, SourceState, UnavailableReason};
