//! `libra-governor calibration report` — real duration-coverage and
//! admission-replay calibration evidence (HORO-1132), computed by the
//! daemon over every locally recorded receipt paired back to its
//! originating estimate.
//!
//! Unlike the `hook` subcommands, this is a manual, interactive command:
//! stdout is a normal human-readable report (there is no
//! `hookSpecificOutput` JSON contract to protect here), and — like
//! `hook`, unlike `statusline` — it spawns the daemon if none is running,
//! since a user typing this command clearly wants a real answer, not a
//! silent no-op.

use libra_governor_protocol::{
    AdmissionOutcome, AdmissionPolicyReport, CoverageReport, QuantileCoverage, Request, Response,
    Stratum,
};

use crate::client;

pub fn run() {
    let socket_path = match libra_governor_daemon::paths::socket_path() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("libra-governor calibration report: could not resolve state dir: {e}");
            std::process::exit(1);
        }
    };

    let stream = match client::ensure_daemon_connection(&socket_path) {
        Ok(stream) => stream,
        Err(e) => {
            eprintln!("libra-governor calibration report: daemon unavailable: {e}");
            std::process::exit(1);
        }
    };

    match client::roundtrip(&stream, Request::CalibrationReport) {
        Ok(Response::CalibrationReport(result)) => {
            println!("{}", render_report(&result));
        }
        Ok(other) => {
            eprintln!("libra-governor calibration report: unexpected daemon response: {other:?}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("libra-governor calibration report: request failed: {e}");
            std::process::exit(1);
        }
    }
}

fn render_report(result: &libra_governor_protocol::CalibrationReportResult) -> String {
    let mut out = String::new();
    out.push_str("libra-governor calibration report\n");
    out.push_str("==================================\n\n");
    if result.dropped_rows > 0 {
        out.push_str(&format!(
            "Dropped {} locally recorded receipt(s): no usable estimate (estimate-less plan or \
             cold-start).\n\n",
            result.dropped_rows
        ));
    }

    out.push_str("Duration coverage\n------------------\n");
    out.push_str(&render_coverage(&result.coverage));
    out.push('\n');

    out.push_str("Admission replay\n-----------------\n");
    for report in &result.admission {
        out.push_str(&render_admission(report));
    }

    out
}

fn render_coverage(coverage: &CoverageReport) -> String {
    match coverage {
        CoverageReport::Insufficient { n, required } => format!(
            "insufficient data: n={n}, need >= {required} non-cold-start calibration pairs. \
             This is the honest, expected result for a pre-launch product with no real \
             trajectory history yet.\n"
        ),
        CoverageReport::Degenerate {
            n,
            distinct_actual_durations,
            reason,
        } => format!(
            "degenerate data: n={n}, only {distinct_actual_durations} distinct actual \
             duration(s). {reason}\n"
        ),
        CoverageReport::Computed {
            n,
            overall,
            by_bucket_tier,
            by_sample_band,
        } => {
            let mut out = format!("computed over n={n} real calibration pairs.\n\n");
            out.push_str("  overall:\n");
            for qc in overall {
                out.push_str(&format!("    {}\n", render_quantile(qc)));
            }
            if !by_bucket_tier.is_empty() {
                out.push_str("\n  by bucket tier:\n");
                for stratum in by_bucket_tier {
                    out.push_str(&render_stratum(stratum));
                }
            }
            if !by_sample_band.is_empty() {
                out.push_str("\n  by sample-count band:\n");
                for stratum in by_sample_band {
                    out.push_str(&render_stratum(stratum));
                }
            }
            out
        }
    }
}

fn render_stratum(stratum: &Stratum) -> String {
    let mut out = format!("    {}:\n", stratum.label);
    for qc in &stratum.quantiles {
        out.push_str(&format!("      {}\n", render_quantile(qc)));
    }
    out
}

fn render_quantile(qc: &QuantileCoverage) -> String {
    let coverage = qc
        .empirical_coverage
        .map(|c| format!("{:.1}%", c * 100.0))
        .unwrap_or_else(|| "n/a".to_string());
    let pinball = qc
        .pinball_loss
        .map(|p| format!("{p:.2}"))
        .unwrap_or_else(|| "n/a".to_string());
    format!(
        "P{:.0}: coverage={coverage} (hits={}/{}), pinball_loss={pinball}",
        qc.quantile * 100.0,
        qc.hits,
        qc.n,
    )
}

fn render_admission(report: &AdmissionPolicyReport) -> String {
    let policy_line = format!(
        "  policy: deadline={}s, threshold_quantile=P{:.0}\n",
        report.policy.deadline_secs,
        report.policy.threshold_quantile * 100.0
    );
    let outcome_line = match &report.outcome {
        AdmissionOutcome::Insufficient { n, required } => {
            format!("    insufficient data: n={n}, need >= {required}\n")
        }
        AdmissionOutcome::Computed(stats) => format!(
            "    n={}, admit={}, false_admit={}, false_reject={}, \
             mean_overrun_secs={}, p95_overrun_secs={}\n",
            stats.n,
            stats.admit_count,
            stats.false_admit_count,
            stats.false_reject_count,
            stats
                .mean_overrun_secs
                .map(|v| format!("{v:.1}"))
                .unwrap_or_else(|| "n/a".to_string()),
            stats
                .p95_overrun_secs
                .map(|v| format!("{v:.1}"))
                .unwrap_or_else(|| "n/a".to_string()),
        ),
    };
    format!("{policy_line}{outcome_line}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use libra_governor_protocol::{AdmissionPolicy, AdmissionStats, CalibrationReportResult};

    #[test]
    fn render_report_insufficient_coverage_is_honest_not_a_fabricated_number() {
        let result = CalibrationReportResult {
            coverage: CoverageReport::Insufficient { n: 2, required: 30 },
            admission: vec![AdmissionPolicyReport {
                policy: AdmissionPolicy {
                    deadline_secs: 300,
                    threshold_quantile: 0.80,
                },
                outcome: AdmissionOutcome::Insufficient { n: 0, required: 1 },
            }],
            dropped_rows: 0,
        };
        let report = render_report(&result);
        assert!(report.contains("insufficient data: n=2"));
        assert!(!report.contains("100.0%"));
    }

    #[test]
    fn render_report_computed_coverage_includes_dropped_rows_note() {
        let result = CalibrationReportResult {
            coverage: CoverageReport::Computed {
                n: 40,
                overall: vec![QuantileCoverage {
                    quantile: 0.5,
                    n: 40,
                    hits: 20,
                    empirical_coverage: Some(0.5),
                    pinball_loss: Some(1.5),
                }],
                by_bucket_tier: vec![],
                by_sample_band: vec![],
            },
            admission: vec![AdmissionPolicyReport {
                policy: AdmissionPolicy {
                    deadline_secs: 300,
                    threshold_quantile: 0.80,
                },
                outcome: AdmissionOutcome::Computed(AdmissionStats {
                    n: 40,
                    admit_count: 30,
                    false_admit_count: 2,
                    false_reject_count: 1,
                    mean_overrun_secs: Some(12.5),
                    p95_overrun_secs: Some(40.0),
                }),
            }],
            dropped_rows: 3,
        };
        let report = render_report(&result);
        assert!(report.contains("Dropped 3"));
        assert!(report.contains("P50: coverage=50.0%"));
        assert!(report.contains("admit=30"));
    }
}
