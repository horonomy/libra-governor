//! `libra-governor-evidence-adapter` — the DogFood evidence adapter
//! (HORO-1376) for Libra Governor's SHIPPED SQLite ledger.
//!
//! # What this crate is
//!
//! A **read-only** translation layer: it opens (never writes to, beyond
//! what `LedgerStore::open` itself already does for migrations) the
//! existing native ledger and projects its plan/admission and receipt
//! rows into the frozen ADR-0012 §3 `schema_version: 1` evidence event
//! shape. It is not a new store — see `crate::adapter` module docs for
//! the field-by-field derivation and its documented deviation from a
//! naive reading of the ticket brief.
//!
//! # Zero network, by construction and by test
//!
//! This crate has no dependency, direct or transitive through its own
//! `Cargo.toml`, on `libra-governor-gateway` or any HTTP/socket client
//! crate — see `tests/no_network_symbols.rs`, which both greps this
//! crate's own source (recursively, at test-run time, not via a single
//! `include_str!`) for network-capable symbols and proves that scan
//! actually flags a planted violation.
//!
//! # `local_only` only, in v1
//!
//! [`adapter::AdapterConfig::validate`] rejects any transport policy
//! other than `local_only` at config-load time (ADR-0012 §11.4,
//! DFC-ADAPT-08). This crate never constructs, and has no code path
//! capable of constructing, a `manual_window`/`monthly_window` transfer.

pub mod adapter;
pub mod canon;
pub mod event;

pub use adapter::{
    build_events, current_profile, summarize, AdapterConfig, AdapterConfigError, AdapterError,
    GapSummary, TransportPolicy,
};
pub use canon::CANONICALIZATION_ID;
pub use event::{DogfoodEvent, SchemaViolation};
