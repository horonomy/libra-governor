//! `libra-governor-quota-observation` — the bounded one-shot stdio
//! observation runtime (HORO-1854), implementing the accepted
//! design-v2 quota-observation extension contract (HORO-1853).
//!
//! # Scope — read this before anything else in this crate
//!
//! This crate is **observation-only**. It has no admission, pacing, or
//! enforcement authority: nothing here decides whether a task may run,
//! reserves capacity, or feeds a real quota/ledger decision. It exists
//! purely to let an operator ask "what does this registered extension
//! currently report about some external quota/usage constraint" and get
//! back a bounded, trust-labeled answer — display/diagnostics, never a
//! gate.
//!
//! It is also, deliberately, **never a credential broker**: the
//! one-shot stdio child this crate launches receives no environment
//! beyond `LANG=C.UTF-8`/`LC_ALL=C.UTF-8` (see [`runner`]), no `HOME`,
//! and no credential of any kind. This mirrors founder decision E1 for
//! this campaign.
//!
//! # Why this crate depends on neither `libra-governor-domain` nor
//! `libra-governor-quota-source`
//!
//! The accepted design doc anticipated this crate might reuse
//! `libra-governor-quota-source`'s quantity/unit types if they "cleanly
//! fit" this wire shape. They do not: `quota-source` itself reuses
//! `libra-governor-domain::quota_window`'s own `GaugeReading`/
//! `QuotaUnit`/`QuotaAmount` types (see `quota-source/src/response.rs`),
//! which represent a reading as a Rust-native `u64`/`QuotaAmount` pair
//! tagged `"state"`, with variants `Undisclosed`/`Used { used, limit }`
//! and a `QuotaUnit` that has no `percent_basis_points` or
//! `opaque_credit`-with-namespace-field shape matching this protocol's
//! wire format. This protocol's wire `reading`/`unit` are tagged
//! `"kind"`, have different variant sets (`used`/`remaining`/
//! `utilization`/`undisclosed`; `tokens`/`requests`/`usd_cents`/
//! `percent_basis_points`/`opaque_credit`), and every numeric value
//! ([`wire::Quantity`]) is a canonical, unsigned, `i64::MAX`-bounded
//! *decimal string* that must serialize back out byte-identical to what
//! was received — a wire-format stability requirement across extension
//! versions that `domain`'s in-process `u64`/`QuotaAmount`
//! representation was never designed for. Similarly,
//! `libra-governor-domain`'s `quota_window` module's own `PoolId` is an
//! unrelated opaque string identifier for a *different* concept (a
//! shared-pool ledger reservation key), not this protocol's load-bearing
//! `(account_fingerprint, pool_alias)` pool identity. Reusing either
//! would have forced a fit where none exists. Per the design doc's own
//! instruction and this codebase's ADR-0005 precedent of preferring a
//! narrow duplicate over a cross-cutting dependency at a trust boundary,
//! this crate defines its own types instead, and has **no path
//! dependency** on either crate, nor on `crates/daemon`,
//! `crates/extension`, `crates/cli`, or the host adapter registry.

pub mod wire;
