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
