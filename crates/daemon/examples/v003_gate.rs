//! HORO-1669 Stage C: the v0.0.3 progressive-estimator benchmark gate.
//!
//! Replays every real, locally recorded `(Estimate, ExecutionReceipt)`
//! pair (`LedgerStore::calibration_pairs`) and compares three remaining-
//! duration predictors at matched elapsed fractions of each receipt's
//! real `actual_duration_secs`:
//!
//! - **baseline-0** — the plan's own unconditioned total quantiles,
//!   taken as-is (no widening, no conditioning on elapsed time at all).
//! - **baseline-1** — [`RemainingEstimate::from_bucketed`], the existing
//!   named deterministic-widening baseline (untouched by this ticket).
//! - **candidate** — [`libra_governor_estimator::remaining_bucketed`],
//!   HORO-1669's conditional-quantile progressive estimator.
//!
//! Each pair's own estimate/receipt is excluded from the history it is
//! evaluated against (leave-one-out), so no predictor sees its own
//! answer. Gated on [`libra_governor_estimator::REQUIRED_CALIBRATION_PAIRS`]
//! exactly like every other calibration report in this repo: below that
//! floor, the honest result is `Insufficient`, not a number.
//!
//! ```bash
//! cargo run -p libra-governor-daemon --example v003_gate -- <ledger.sqlite3>
//! ```
//!
//! Real local calibration pairs accumulate only from a real daemon's own
//! finalized tasks. Any fresh checkout or CI run has zero — reporting
//! `Insufficient` in that case is the expected, correct result, not a
//! failure of the harness.

use std::path::PathBuf;

use libra_governor_domain::{RemainingEstimate, TaskFeatures};
use libra_governor_estimator::{remaining_bucketed, REQUIRED_CALIBRATION_PAIRS};
use libra_governor_ledger::{CalibrationPair, LedgerStore};

/// Elapsed-time fractions (of the pair's own real total duration) at
/// which each predictor is evaluated.
const ELAPSED_FRACTIONS: [f64; 3] = [0.25, 0.5, 0.75];

struct PredictorResult {
    name: &'static str,
    /// One entry per (pair, fraction) this predictor produced a real
    /// P50/P80/P90 for — `None` entries (insufficient conditional
    /// evidence, or a cold-start estimate) are excluded, counted
    /// separately as `skipped`.
    abs_error_p50_secs: Vec<f64>,
    p90_coverage_hits: usize,
    p90_coverage_n: usize,
    skipped: usize,
}

impl PredictorResult {
    fn new(name: &'static str) -> Self {
        PredictorResult {
            name,
            abs_error_p50_secs: Vec::new(),
            p90_coverage_hits: 0,
            p90_coverage_n: 0,
            skipped: 0,
        }
    }

    fn record(&mut self, p50: Option<u64>, p90: Option<u64>, actual_remaining: u64) {
        match (p50, p90) {
            (Some(p50), Some(p90)) => {
                self.abs_error_p50_secs
                    .push((p50 as f64 - actual_remaining as f64).abs());
                self.p90_coverage_n += 1;
                if actual_remaining <= p90 {
                    self.p90_coverage_hits += 1;
                }
            }
            _ => self.skipped += 1,
        }
    }

    fn mean_abs_error(&self) -> Option<f64> {
        if self.abs_error_p50_secs.is_empty() {
            return None;
        }
        Some(self.abs_error_p50_secs.iter().sum::<f64>() / self.abs_error_p50_secs.len() as f64)
    }

    fn p90_coverage(&self) -> Option<f64> {
        if self.p90_coverage_n == 0 {
            return None;
        }
        Some(self.p90_coverage_hits as f64 / self.p90_coverage_n as f64)
    }

    fn report_line(&self) -> String {
        format!(
            "{:<10} mean|P50 error| = {:<10} P90 coverage = {:<10} (n={}, skipped={})",
            self.name,
            self.mean_abs_error()
                .map(|v| format!("{v:.1}s"))
                .unwrap_or_else(|| "n/a".to_string()),
            self.p90_coverage()
                .map(|v| format!("{:.1}%", v * 100.0))
                .unwrap_or_else(|| "n/a".to_string()),
            self.abs_error_p50_secs.len(),
            self.skipped,
        )
    }
}

fn synthetic_progress_evidence(
    elapsed_secs: u64,
    spend_so_far: libra_governor_domain::SpendSoFar,
) -> libra_governor_domain::ProgressEvidence {
    libra_governor_domain::ProgressEvidence {
        elapsed_secs,
        spend_so_far,
        tool_calls_total: 0,
        tool_calls_since_last_replan: 0,
        same_tool_streak: 0,
        plan_revision: 1,
        auto_replan_count: 0,
        active_lease_count: 0,
        child_account_count: 0,
        gateway_request_count: 0,
        observed_at: time::OffsetDateTime::now_utc(),
    }
}

fn history_without(
    pairs: &[CalibrationPair],
    skip_index: usize,
) -> Vec<(
    Option<TaskFeatures>,
    libra_governor_domain::ExecutionReceipt,
)> {
    pairs
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != skip_index)
        .map(|(_, p)| {
            let receipt = libra_governor_domain::ExecutionReceipt::new(
                libra_governor_domain::TaskId::default(),
                1,
                libra_governor_domain::PlanId::default(),
                p.actual_duration_secs,
                vec![],
                libra_governor_domain::ExecutionOutcome::Unknown,
                p.recorded_at,
            )
            .with_task_features(p.task_features.clone());
            (p.task_features.clone(), receipt)
        })
        .collect()
}

fn main() {
    let ledger_path: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("ledger.sqlite3"));

    let ledger = match LedgerStore::open(&ledger_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("v003_gate: could not open ledger at {ledger_path:?}: {e}");
            std::process::exit(2);
        }
    };

    let (pairs, total_receipts) = ledger
        .calibration_pairs()
        .expect("calibration_pairs query must succeed against a valid ledger schema");

    println!("v0.0.3 progressive-estimator benchmark gate");
    println!("ledger: {ledger_path:?}");
    println!(
        "calibration pairs: {} (of {total_receipts} total receipts)",
        pairs.len()
    );

    if pairs.len() < REQUIRED_CALIBRATION_PAIRS {
        println!(
            "INSUFFICIENT — {} pairs < required {REQUIRED_CALIBRATION_PAIRS}. \
             This is the honest result for a fresh checkout or CI run; it is \
             not a harness failure. Re-run against a ledger with real \
             dogfood history to get a real comparison.",
            pairs.len()
        );
        return;
    }

    let mut baseline0 = PredictorResult::new("baseline-0");
    let mut baseline1 = PredictorResult::new("baseline-1");
    let mut candidate = PredictorResult::new("candidate");

    for (i, pair) in pairs.iter().enumerate() {
        let Some(features) = pair.task_features.clone() else {
            continue;
        };
        let Some(regime) = pair.regime.clone() else {
            continue;
        };
        let history = history_without(&pairs, i);

        for &fraction in &ELAPSED_FRACTIONS {
            let elapsed_secs = (pair.actual_duration_secs as f64 * fraction) as u64;
            if elapsed_secs >= pair.actual_duration_secs {
                continue;
            }
            let actual_remaining = pair.actual_duration_secs - elapsed_secs;

            // baseline-0: the pair's OWN already-computed total quantiles,
            // unconditioned — the naive "ignore elapsed time entirely"
            // predictor.
            baseline0.record(
                pair.estimate.duration_p50_secs,
                pair.estimate.duration_p90_secs,
                actual_remaining,
            );

            // baseline-1: the existing named deterministic-widening
            // baseline, also unconditioned on elapsed time.
            let widened = RemainingEstimate::from_bucketed(pair.estimate.clone());
            baseline1.record(
                widened.estimate.duration_p50_secs,
                widened.estimate.duration_p90_secs,
                actual_remaining,
            );

            // candidate: HORO-1669's conditional-quantile estimator,
            // genuinely conditioned on `elapsed_secs` via leave-one-out
            // history.
            let evidence = synthetic_progress_evidence(
                elapsed_secs,
                libra_governor_domain::SpendSoFar::NoBasis {
                    reason: libra_governor_domain::NoSpendBasis::NoAccount,
                },
            );
            let remaining = remaining_bucketed(&history, &features, &regime, &evidence);
            let (p50, p90) = match remaining.duration {
                libra_governor_domain::RemainingDuration::Quantiles {
                    p50_secs, p90_secs, ..
                } => (Some(p50_secs), Some(p90_secs)),
                libra_governor_domain::RemainingDuration::Insufficient { .. } => (None, None),
            };
            candidate.record(p50, p90, actual_remaining);
        }
    }

    println!();
    println!("{}", baseline0.report_line());
    println!("{}", baseline1.report_line());
    println!("{}", candidate.report_line());
}
