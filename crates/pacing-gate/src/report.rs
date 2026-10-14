//! Verdict composition (HORO-1768 PR-B).
//!
//! Pure functions over [`crate::verify::VerifyReport`]s — this module
//! never recomputes a metric itself, only labels and gates what
//! `verify::verify` already computed.
//!
//! # Evidence classes, and why `Go` is structurally unreachable here
//!
//! Every [`MetricRow`] this crate can ever produce is
//! [`EvidenceClass::Simulation`]: both `strategy::StaticBudget` and
//! `strategy::Paced` are, in this crate, exercised only via
//! `drive::run_trace` against a frozen fixture — never against a real
//! launcher, a real quota provider, or production traffic. `StaticBudget`
//! being [`crate::strategy::Capability::LiveCapable`] describes a
//! *future* possibility (it could be wired live without reopening
//! HORO-1727's seam), not anything this crate itself has done.
//!
//! [`decide`] therefore gates `Go` behind TWO independent conditions,
//! both of which must hold: a human-supplied [`ValueCriterion`]
//! ([`Verdict::value_criterion`], which this crate never constructs
//! itself — see its own docs), AND at least one row whose evidence is
//! NOT [`EvidenceClass::Simulation`]. Since every row this crate builds
//! is `Simulation`, the second condition can never be satisfied by this
//! crate's own output alone, regardless of what `value_criterion` is set
//! to — `Go` requires evidence this crate, by design, does not produce.
//! See `tests/verdict_gating.rs` for the empirical proof (every
//! simulation row green, `value_criterion` supplied, result still
//! `NoGoPreview`).
//!
//! PR-C (live occupancy consistency, HORO-1768's remaining scope) and
//! real external resolution of HORO-1727/1714/1599 are what would
//! eventually produce a `TestOwnedLive`/`ProductionObserved` row — not
//! this crate.

use crate::verify::VerifyReport;

/// Labels which kind of evidence a [`MetricRow`] rests on. See module
/// docs for why this crate only ever produces [`Self::Simulation`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceClass {
    /// Produced by replaying a frozen fixture through `drive::run_trace`
    /// — exactly what every row in this crate is.
    Simulation,
    /// Produced by a real, owned test environment exercising real code
    /// paths (e.g. a live launcher integration test) — not yet built for
    /// this ticket (PR-C, HORO-1766).
    TestOwnedLive,
    /// Observed from real production traffic.
    ProductionObserved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoDecision {
    Go,
    NoGoPreview,
}

/// One labeled metric line in a verdict.
#[derive(Debug, Clone)]
pub struct MetricRow {
    pub label: String,
    pub evidence: EvidenceClass,
    pub summary: String,
}

/// A human sign-off this crate never fabricates — see [`Verdict`]'s own
/// docs. Constructing one is exclusively the caller's (a human
/// reviewer's) decision; nothing in this crate ever builds a
/// `ValueCriterion` for itself.
#[derive(Debug, Clone)]
pub struct ValueCriterion {
    pub signed_off_by: String,
    pub note: String,
}

/// The final verdict. `value_criterion` is `None`/unset by every
/// constructor this crate defines — see [`decide`]'s own docs for why
/// that alone is still not sufficient to reach `Go`.
#[derive(Debug, Clone)]
pub struct Verdict {
    pub decision: GoDecision,
    pub rows: Vec<MetricRow>,
    pub value_criterion: Option<ValueCriterion>,
}

/// Builds one [`MetricRow`] per AC1 metric in `report`, every one labeled
/// [`EvidenceClass::Simulation`] — see module docs.
pub fn rows_for_report(report: &VerifyReport) -> Vec<MetricRow> {
    let fixture = &report.fixture;
    let strategy = &report.strategy.name;
    vec![
        MetricRow {
            label: format!("{fixture}/{strategy}: successful completed work"),
            evidence: EvidenceClass::Simulation,
            summary: format!(
                "{}/{} tasks completed ({:.0}%), {} units settled",
                report.tasks_completed,
                report.tasks_total,
                report.completion_rate * 100.0,
                report.volume_settled
            ),
        },
        MetricRow {
            label: format!("{fixture}/{strategy}: involuntary wait"),
            evidence: EvidenceClass::Simulation,
            summary: format!(
                "{} total seconds across all tasks",
                report.involuntary_wait_secs
            ),
        },
        MetricRow {
            label: format!("{fixture}/{strategy}: quota waste"),
            evidence: EvidenceClass::Simulation,
            summary: format!("{} units of unused-but-wanted headroom", report.quota_waste),
        },
        MetricRow {
            label: format!("{fixture}/{strategy}: false throttles"),
            evidence: EvidenceClass::Simulation,
            summary: format!(
                "{} task(s), {} total seconds",
                report.false_throttle_task_count, report.false_throttle_secs
            ),
        },
        MetricRow {
            label: format!("{fixture}/{strategy}: downstream cooldown impact"),
            evidence: EvidenceClass::Simulation,
            summary: format!(
                "{} seconds inherited by dependents",
                report.downstream_cooldown_secs
            ),
        },
        MetricRow {
            label: format!("{fixture}/{strategy}: completion-contract compliance (proxy)"),
            evidence: EvidenceClass::Simulation,
            summary: format!(
                "{:.0}% of declared tasks completed",
                report.completion_rate * 100.0
            ),
        },
        MetricRow {
            label: format!("{fixture}/{strategy}: hard limits (AC2)"),
            evidence: EvidenceClass::Simulation,
            summary: format!(
                "{} violation(s), {} principal violation(s)",
                report.hard_limit_violations.len(),
                report.principal_violations.len()
            ),
        },
    ]
}

/// Builds rows for every report in `reports`, in order.
pub fn rows_for_reports(reports: &[VerifyReport]) -> Vec<MetricRow> {
    reports.iter().flat_map(rows_for_report).collect()
}

/// Decides `Go`/`NoGoPreview` from `rows` and an optional
/// `value_criterion`. `Go` requires BOTH: `value_criterion.is_some()`
/// AND at least one row that is not `EvidenceClass::Simulation`. See
/// module docs for why this crate's own `rows_for_report`/
/// `rows_for_reports` output can never satisfy the second condition.
pub fn decide(rows: Vec<MetricRow>, value_criterion: Option<ValueCriterion>) -> Verdict {
    let has_non_simulation_evidence = rows.iter().any(|r| r.evidence != EvidenceClass::Simulation);
    let decision = if value_criterion.is_some() && has_non_simulation_evidence {
        GoDecision::Go
    } else {
        GoDecision::NoGoPreview
    };
    Verdict {
        decision,
        rows,
        value_criterion,
    }
}
