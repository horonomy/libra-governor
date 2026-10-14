//! `libra-governor-pacing-gate` — falsification harness for the pacing
//! scheduler (HORO-1768, PR-A of 3).
//!
//! This crate never modifies `pacing/*`, `step.rs`, `daemon/src`,
//! `gateway/src`, or `ledger/src` — it falsifies, it does not fix. See
//! `crates/domain/src/pacing/mod.rs`'s HORO-1727 seam and
//! `crates/domain/tests/pacing_not_wired_live.rs`, which this crate's own
//! `Paced` strategy is careful never to trip (it calls into `pacing`
//! from *this* crate, never from `daemon`/`gateway`/`ledger`).
//!
//! # PR-A scope (this PR)
//!
//! - [`trace`]: frozen trace fixtures — relative workload, not absolute
//!   schedule.
//! - [`world`]: the ground-truth model, independent of what any strategy
//!   is told.
//! - [`strategy`]: the `Strategy` trait plus `StaticBudget` (live-
//!   capable) and `Paced` (simulation-only).
//! - [`drive`]: the closed-loop driver that replays a trace's workload
//!   against a strategy.
//!
//! # PR-B scope (this PR)
//!
//! - [`verify`]: an independent verifier computing every AC1 metric
//!   structurally from the [`world::World`]'s ground-truth log — never
//!   from a strategy's own self-report.
//! - [`mutants`]: negative-control strategies (`Unguarded`,
//!   `ForeignSession`, `ForgetfulBaseline`) that each deliberately
//!   violate one guarantee AC2 claims is structurally enforced, proving
//!   `verify` actually catches a real violation.
//! - [`report`]: a pure `Verdict`/`EvidenceClass` type. Every row this
//!   crate itself produces is `EvidenceClass::Simulation` — PR-C (live
//!   occupancy) and real external calibration are what would eventually
//!   populate the other evidence classes, not this crate.
//!
//! # Fixture numbering
//!
//! The design called for "9 frozen trace JSON fixtures... `t0_occupancy`
//! through `t7_burst_to_sustain`" — 8 names spanning a 9-fixture count.
//! Resolved here as: `t0`..`t7` (8 fixtures, one per named scenario in
//! the brief, with `t6_overrun_backoff` filling the one scenario the
//! brief's prose lists but the `t0..t7` naming didn't have a slot for)
//! plus two unnumbered adversarial fixtures, `x0_unguarded` (PR-A) and
//! `x1_foreign_session` (PR-B, added so the `ForeignSession` mutant's
//! identity violation has a fixture with more than one principal to
//! violate) — following `two-principal-harness`'s own
//! `x_confused_deputy.rs`/`x_startup_checks.rs` convention for an
//! adversarial test outside the main numbered sequence. See `fixtures/`
//! for all ten.

pub mod drive;
pub mod mutants;
pub mod report;
pub mod strategy;
pub mod trace;
pub mod verify;
pub mod world;

/// Every frozen fixture's raw JSON, embedded at compile time so a
/// `cargo test` run never depends on `CARGO_MANIFEST_DIR`-relative file
/// resolution working from an arbitrary working directory. `tests/` load
/// fixtures via [`crate::trace::TraceSpec::from_json`] against these
/// constants (see `fixtures()` below) rather than re-deriving file paths.
pub mod fixtures {
    use crate::trace::{TraceError, TraceSpec};

    macro_rules! fixture {
        ($name:literal) => {
            include_str!(concat!("../fixtures/", $name, ".json"))
        };
    }

    /// Every fixture's `(name, raw JSON)` pair — the single registration
    /// point `tests/fixtures_load.rs` checks against the files actually
    /// present on disk, so an orphaned `fixtures/*.json` file (added but
    /// never registered here) fails that test rather than silently never
    /// being exercised.
    pub const ALL: &[(&str, &str)] = &[
        ("t0_occupancy", fixture!("t0_occupancy")),
        ("t1_multi_window", fixture!("t1_multi_window")),
        ("t2_concurrent_session", fixture!("t2_concurrent_session")),
        ("t3_opaque_quota", fixture!("t3_opaque_quota")),
        ("t4_delayed_settlement", fixture!("t4_delayed_settlement")),
        ("t5_daemon_restart", fixture!("t5_daemon_restart")),
        ("t6_overrun_backoff", fixture!("t6_overrun_backoff")),
        ("t7_burst_to_sustain", fixture!("t7_burst_to_sustain")),
        ("x0_unguarded", fixture!("x0_unguarded")),
        ("x1_foreign_session", fixture!("x1_foreign_session")),
    ];

    pub fn load(name: &str) -> Result<TraceSpec, TraceError> {
        let raw = ALL
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, json)| *json)
            .unwrap_or_else(|| panic!("fixture {name:?} is not registered in fixtures::ALL"));
        TraceSpec::from_json(raw)
    }

    pub fn load_all() -> Vec<(&'static str, Result<TraceSpec, TraceError>)> {
        ALL.iter()
            .map(|(name, json)| (*name, TraceSpec::from_json(json)))
            .collect()
    }
}
