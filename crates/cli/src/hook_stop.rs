//! `libra-governor hook stop` — the Claude Code `Stop` hook entry point.
//!
//! Reads the hook JSON payload from stdin and asks the daemon to
//! finalize the task bound to this session: compute elapsed duration,
//! gather the tool-call count, and persist an
//! [`libra_governor_domain::ExecutionReceipt`] (HORO-1126). Prints a
//! concise human-readable summary to **stderr** — never stdout, which is
//! reserved the same way `UserPromptSubmit`'s is (see `hook.rs` docs);
//! MVP 1.0 is advisory only and never uses the `Stop` hook's
//! `{"decision": "block", ...}` mechanism, so stdout stays empty.
//!
//! Never spawns the daemon: if none is running, there is no in-flight
//! preflight state to finalize against anyway, so a safe no-op is
//! correct. Never panics, never exits nonzero.

use std::path::PathBuf;

use libra_governor_domain::{Estimate, ResourceAmount};
use libra_governor_protocol::{FinalizeOutcome, FinalizeResult, Request, Response};
use serde::Deserialize;

use crate::client;

/// The subset of the Claude Code `Stop` hook payload this integration
/// needs. `model` is included because Claude Code's `Stop` hook payload
/// does expose it (unlike a provider or cost/token figure — see the
/// HORO-1126 PR description for what was verified against Claude Code's
/// hooks documentation). Extra fields are ignored, not rejected.
#[derive(Debug, Deserialize)]
struct HookPayload {
    session_id: String,
    #[serde(default)]
    model: Option<String>,
}

pub fn run() {
    let log_path = libra_governor_daemon::paths::log_path();

    let mut raw = String::new();
    if std::io::Read::read_to_string(&mut std::io::stdin(), &mut raw).is_err() {
        log(&log_path, "stop: failed to read hook payload from stdin");
        return;
    }

    let payload: HookPayload = match serde_json::from_str(&raw) {
        Ok(p) => p,
        Err(e) => {
            log(&log_path, &format!("stop: malformed hook payload: {e}"));
            return;
        }
    };

    let socket_path = match libra_governor_daemon::paths::socket_path() {
        Ok(p) => p,
        Err(e) => {
            log(
                &log_path,
                &format!("stop: could not resolve state dir: {e}"),
            );
            return;
        }
    };

    let stream = match client::connect_only(&socket_path) {
        Ok(stream) => stream,
        Err(e) => {
            // No daemon reachable: nothing to finalize against. A safe,
            // silent no-op, not an error condition worth surfacing.
            log(
                &log_path,
                &format!("stop: daemon unreachable, skipping finalize: {e}"),
            );
            return;
        }
    };

    let request = Request::Finalize {
        session_id: payload.session_id,
        model: payload.model,
    };

    match client::roundtrip(&stream, request) {
        Ok(Response::Finalize(FinalizeOutcome::NoActiveTask)) => {
            log(
                &log_path,
                "stop: no active task for this session, safe no-op",
            );
        }
        Ok(Response::Finalize(FinalizeOutcome::Finalized(result))) => {
            eprintln!("{}", format_receipt_summary(&result));
        }
        Ok(other) => {
            log(
                &log_path,
                &format!("stop: unexpected response kind for Finalize: {other:?}"),
            );
        }
        Err(e) => {
            log(&log_path, &format!("stop: finalize request failed: {e}"));
        }
    }
}

fn log(log_path: &Result<PathBuf, libra_governor_daemon::paths::PathsError>, message: &str) {
    if let Ok(path) = log_path {
        libra_governor_daemon::log::append_line(path, message);
    }
}

/// Renders the human-readable Estimate-vs-Actual summary printed to
/// stderr on a successful finalize.
fn format_receipt_summary(result: &FinalizeResult) -> String {
    let receipt = &result.receipt;

    let (estimate_line, inside_p90) = match &result.estimate {
        None => (
            "Estimate: unavailable (no estimate recorded on this plan)".to_string(),
            "n/a".to_string(),
        ),
        Some(estimate) if estimate.cold_start => (
            format!(
                "Estimate: cold start — {} (confidence: {:?}, n=0)",
                estimate
                    .reason
                    .as_deref()
                    .unwrap_or("insufficient local history"),
                estimate.confidence
            ),
            "n/a (cold start)".to_string(),
        ),
        Some(estimate) => {
            let inside = match estimate.duration_p90_secs {
                Some(p90) if receipt.actual_duration_secs <= p90 => "yes".to_string(),
                Some(_) => "no".to_string(),
                None => "n/a".to_string(),
            };
            (format_estimate_line(estimate), inside)
        }
    };

    format!(
        "[libra-governor] Execution Receipt (task {})\n\
         {estimate_line}\n\
         Actual:   {}\n\
         Outcome:  Unknown (no automated completion verification in MVP 1)\n\
         Tool calls: {}\n\
         Inside P90: {inside_p90}",
        receipt.task_id,
        format_actual_line(receipt.actual_duration_secs, &receipt.actual_usage),
        receipt.tool_call_count,
    )
}

fn format_estimate_line(estimate: &Estimate) -> String {
    let p50 = format_pair(estimate.duration_p50_secs, &estimate.resource_p50);
    let p90 = format_pair(estimate.duration_p90_secs, &estimate.resource_p90);
    format!(
        "Estimate: P50 {p50} / P90 {p90} (confidence: {:?}, n={})",
        estimate.confidence, estimate.sample_count
    )
}

fn format_pair(duration_secs: Option<u64>, resource: &Option<ResourceAmount>) -> String {
    let duration = duration_secs
        .map(|s| format!("{s}s"))
        .unwrap_or_else(|| "unknown".to_string());
    let resource = resource
        .as_ref()
        .map(format_resource)
        .unwrap_or_else(|| "unknown".to_string());
    format!("{duration} / {resource}")
}

fn format_actual_line(duration_secs: u64, usage: &[ResourceAmount]) -> String {
    let resource = usage
        .first()
        .map(format_resource)
        .unwrap_or_else(|| "unknown (harness does not expose cost/usage data)".to_string());
    format!("{duration_secs}s / {resource}")
}

fn format_resource(amount: &ResourceAmount) -> String {
    match amount {
        ResourceAmount::UsdCents(cents) => format!("${:.2}", *cents as f64 / 100.0),
        ResourceAmount::Tokens(tokens) => format!("{tokens} tok"),
        ResourceAmount::QuotaPercent(pct) => format!("{pct:.1}%"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libra_governor_domain::{Confidence, ExecutionOutcome, ExecutionReceipt, PlanId, TaskId};
    use time::OffsetDateTime;

    #[test]
    fn hook_payload_ignores_unknown_fields_and_missing_model() {
        let json = r#"{
            "session_id": "sess-1",
            "cwd": "/repo",
            "hook_event_name": "Stop",
            "stop_hook_active": false
        }"#;
        let payload: HookPayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.session_id, "sess-1");
        assert_eq!(payload.model, None);
    }

    #[test]
    fn hook_payload_parses_model_when_present() {
        let json = r#"{"session_id": "sess-1", "model": "claude-sonnet-5"}"#;
        let payload: HookPayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.model.as_deref(), Some("claude-sonnet-5"));
    }

    fn sample_receipt() -> ExecutionReceipt {
        ExecutionReceipt::new(
            TaskId::new(),
            1,
            PlanId::new(),
            42,
            vec![],
            ExecutionOutcome::Unknown,
            OffsetDateTime::UNIX_EPOCH,
        )
        .with_tool_call_count(3)
    }

    #[test]
    fn format_receipt_summary_cold_start_is_structurally_distinct() {
        let result = FinalizeResult {
            receipt: sample_receipt(),
            estimate: Some(Estimate::cold_start()),
        };
        let summary = format_receipt_summary(&result);
        assert!(summary.contains("cold start"));
        assert!(summary.contains("Inside P90: n/a"));
        assert!(summary.contains("Tool calls: 3"));
        assert!(summary.contains("Outcome:  Unknown"));
    }

    #[test]
    fn format_receipt_summary_computed_estimate_reports_inside_p90() {
        let estimate = Estimate {
            duration_p50_secs: Some(30),
            duration_p80_secs: Some(50),
            duration_p90_secs: Some(60),
            resource_p50: None,
            resource_p80: None,
            resource_p90: None,
            confidence: Confidence::Medium,
            sample_count: 8,
            cold_start: false,
            estimator_version: "v1-empirical-quantile".to_string(),
            reason: None,
        };
        let result = FinalizeResult {
            receipt: sample_receipt(),
            estimate: Some(estimate),
        };
        let summary = format_receipt_summary(&result);
        assert!(summary.contains("Inside P90: yes"), "{summary}");
        assert!(summary.contains("n=8"));
    }

    #[test]
    fn format_receipt_summary_outside_p90_reports_no() {
        let mut receipt = sample_receipt();
        receipt.actual_duration_secs = 999;
        let estimate = Estimate {
            duration_p50_secs: Some(30),
            duration_p80_secs: Some(50),
            duration_p90_secs: Some(60),
            resource_p50: None,
            resource_p80: None,
            resource_p90: None,
            confidence: Confidence::Medium,
            sample_count: 8,
            cold_start: false,
            estimator_version: "v1-empirical-quantile".to_string(),
            reason: None,
        };
        let result = FinalizeResult {
            receipt,
            estimate: Some(estimate),
        };
        let summary = format_receipt_summary(&result);
        assert!(summary.contains("Inside P90: no"), "{summary}");
    }

    #[test]
    fn format_resource_renders_each_kind() {
        assert_eq!(format_resource(&ResourceAmount::UsdCents(199)), "$1.99");
        assert_eq!(format_resource(&ResourceAmount::Tokens(5000)), "5000 tok");
        assert_eq!(
            format_resource(&ResourceAmount::QuotaPercent(12.5)),
            "12.5%"
        );
    }
}
