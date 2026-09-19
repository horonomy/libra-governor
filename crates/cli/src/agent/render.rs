//! Rendering helpers moved verbatim from the pre-HORO-1157 `hook.rs`/
//! `hook_stop.rs` (byte-identical behavior — see
//! `crates/cli/tests/agent_contract.rs`), used by `agent::run` for both
//! Claude Code and Codex. Codex's `UserPromptSubmit`/`Stop` hook event
//! names are identical strings to Claude Code's, so the
//! `"UserPromptSubmit"`/`"Stop"` literals below are correct for either
//! host, not Claude-specific.

use libra_governor_domain::{Estimate, ResourceAmount};
use libra_governor_protocol::{Confidence, FinalizeResult, PreflightResult};

use crate::bucket_prose::describe_bucket_tier;

/// Renders the `hookSpecificOutput.additionalContext` JSON object for a
/// successful [`PreflightResult`].
pub fn format_additional_context(result: &PreflightResult) -> String {
    let confidence = match result.confidence {
        Confidence::Low => "low",
        Confidence::Medium => "medium",
        Confidence::High => "high",
    };

    let mut text = format!(
        "[libra-governor] Preflight complete (task {}, confidence: {confidence}, recon: {:.2}s).\n",
        result.task_id, result.recon_cost_seconds
    );
    text.push_str("Draft Completion Contract (revision ");
    text.push_str(&result.contract_draft.revision.to_string());
    text.push_str("):\n");
    for criterion in &result.contract_draft.criteria {
        let marker = if criterion.required {
            "required"
        } else {
            "optional"
        };
        text.push_str(&format!("  - [{marker}] {}\n", criterion.description));
    }
    if let Some(reason) = &result.recon_summary.reason {
        text.push_str(&format!("Note: {reason}\n"));
    }
    text.push_str(&format_estimate_summary(result.estimate.as_ref()));
    text.push_str(" This preflight is advisory only.");

    let output = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": text,
        }
    });
    serde_json::to_string(&output).unwrap_or_else(|_| "{}".to_string())
}

/// Renders one line summarizing the preflight [`Estimate`] (HORO-1126).
/// A cold-start estimate (no local history yet) is reported honestly as
/// such, never as a fabricated number.
fn format_estimate_summary(estimate: Option<&Estimate>) -> String {
    match estimate {
        None => "Cost/time estimate: unavailable.".to_string(),
        Some(estimate) if estimate.cold_start => format!(
            "Cost/time estimate: cold start — {} (confidence: low).",
            estimate
                .reason
                .as_deref()
                .unwrap_or("insufficient local history")
        ),
        Some(estimate) => format!(
            "Cost/time estimate: P50 {}s / P90 {}s (confidence: {:?}, n={}, estimator {}, {}).",
            estimate.duration_p50_secs.unwrap_or(0),
            estimate.duration_p90_secs.unwrap_or(0),
            estimate.confidence,
            estimate.sample_count,
            estimate.estimator_version,
            describe_bucket_tier(estimate.bucket_tier, estimate.sample_count),
        ),
    }
}

/// Renders a plain-text `additionalContext` message wrapped in the
/// expected `hookSpecificOutput` JSON shape — the fallback path used
/// whenever a real [`PreflightResult`] could not be obtained.
pub fn render_context_envelope(message: &str) -> String {
    let output = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "UserPromptSubmit",
            "additionalContext": message,
        }
    });
    serde_json::to_string(&output).unwrap_or_else(|_| "{}".to_string())
}

/// Renders the human-readable Estimate-vs-Actual summary printed to
/// stderr on a successful finalize.
pub fn format_receipt_summary(result: &FinalizeResult) -> String {
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
        "Estimate: P50 {p50} / P90 {p90} (confidence: {:?}, n={}, {})",
        estimate.confidence,
        estimate.sample_count,
        describe_bucket_tier(estimate.bucket_tier, estimate.sample_count),
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
    use libra_governor_domain::{
        CompletionContract, CompletionCriterion, ExecutionOutcome, ExecutionReceipt, PlanId, TaskId,
    };
    use libra_governor_protocol::ReconSummary;
    use time::OffsetDateTime;

    fn sample_preflight_result() -> PreflightResult {
        PreflightResult {
            task_id: TaskId::new(),
            plan_id: PlanId::new(),
            contract_draft: CompletionContract::first(vec![
                CompletionCriterion::required("Matches the user's stated request"),
                CompletionCriterion::required("Relevant tests pass (cargo test)"),
            ]),
            recon_summary: ReconSummary {
                files_scanned: 5,
                dirs_scanned: 2,
                likely_affected_paths: vec!["src/login.rs".to_string()],
                detected_test_commands: vec!["cargo test".to_string()],
                truncated: false,
                reason: None,
            },
            confidence: Confidence::High,
            recon_cost_seconds: 0.42,
            estimate: None,
            admission: None,
            completion_reserve: None,
        }
    }

    #[test]
    fn format_additional_context_includes_confidence_and_criteria() {
        let result = sample_preflight_result();
        let json = format_additional_context(&result);
        assert!(json.contains("hookSpecificOutput"));
        assert!(json.contains("UserPromptSubmit"));
        assert!(json.contains("additionalContext"));
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let ctx = parsed["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(ctx.contains("confidence"));
        assert!(ctx.contains("required"));
    }

    #[test]
    fn render_context_envelope_produces_valid_json() {
        let s = render_context_envelope("daemon unavailable");
        let parsed: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(
            parsed["hookSpecificOutput"]["hookEventName"],
            "UserPromptSubmit"
        );
        assert_eq!(
            parsed["hookSpecificOutput"]["additionalContext"],
            "daemon unavailable"
        );
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
            estimator_version: "v2-bucketed-quantile".to_string(),
            reason: None,
            feature_schema_version: "fs-v1".to_string(),
            bucket_tier: libra_governor_domain::BucketTier::Global,
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
            estimator_version: "v2-bucketed-quantile".to_string(),
            reason: None,
            feature_schema_version: "fs-v1".to_string(),
            bucket_tier: libra_governor_domain::BucketTier::Global,
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
