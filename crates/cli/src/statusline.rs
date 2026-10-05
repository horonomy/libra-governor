//! `libra-governor statusline` — Claude Code `statusLine` command.
//!
//! Reads the daemon's current task/preflight state over the same IPC the
//! hook uses and prints one short line. Never spawns the daemon (a
//! statusline refreshes on a short interval; spawning from it would be a
//! race factory — see `crates/cli/src/client.rs::connect_only`) and
//! never makes an LLM call of its own. Always prints exactly one line
//! and exits 0, even when the daemon is unreachable.

use libra_governor_protocol::{Confidence, ReplanState, Request, Response, StatusResult};

use crate::client;

pub fn run() {
    let line = render_status_line();
    println!("{line}");
}

fn render_status_line() -> String {
    let socket_path = match libra_governor_daemon::paths::socket_path() {
        Ok(p) => p,
        Err(_) => return "libra: -".to_string(),
    };

    let stream = match client::connect_only(&socket_path) {
        Ok(stream) => stream,
        Err(_) => return "libra: -".to_string(),
    };

    match client::roundtrip(&stream, Request::Status) {
        Ok(Response::Status(status)) => format_status(&status),
        _ => "libra: -".to_string(),
    }
}

fn format_status(status: &StatusResult) -> String {
    match &status.current_task {
        None => "libra: idle".to_string(),
        Some(task) => {
            let confidence = confidence_str(task.confidence);
            let remaining_p90 = task
                .remaining_estimate
                .duration_p90_secs
                .map(|s| format!("{s}s"))
                .unwrap_or_else(|| "unknown".to_string());
            format!(
                "libra: task {} | plan {} | preflight: {confidence} | recon: {:.1}s | remaining P90: {remaining_p90} | {}",
                short_task_id(&task.task_id.to_string()),
                short_task_id(&task.plan_id.0.to_string()),
                task.recon_cost_seconds,
                format_replan_state(&task.replan_state),
            )
        }
    }
}

fn confidence_str(confidence: Confidence) -> &'static str {
    match confidence {
        Confidence::Low => "low",
        Confidence::Medium => "medium",
        Confidence::High => "high",
    }
}

/// Renders the runtime-replanning half of the statusline (HORO-1139): a
/// task's replan state, so a material replan is visible without a
/// manual query (no MCP surface exists in this codebase — see
/// `crates/domain/src/replan.rs` module docs).
fn format_replan_state(state: &ReplanState) -> String {
    match state {
        ReplanState::Stable => "stable".to_string(),
        ReplanState::Replanned { count } => format!("replanned {count}x"),
        ReplanState::EscalatedAwaitingApproval => "escalated — awaiting approval".to_string(),
    }
}

/// The first 8 hex characters of a UUID string — enough to disambiguate
/// on a one-line status render without eating the whole line width.
///
/// Shared with [`crate::statusline_provider`] rather than reimplemented
/// there: the two renderings must truncate identically, or the same task
/// would carry two different short ids depending on which surface a user
/// happened to be looking at.
pub(crate) fn short_task_id(task_id: &str) -> &str {
    let end = task_id
        .char_indices()
        .nth(8)
        .map(|(i, _)| i)
        .unwrap_or(task_id.len());
    &task_id[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use libra_governor_domain::{Estimate, PlanId, TaskId};
    use libra_governor_protocol::{BudgetPosture, TaskSummary};

    fn sample_estimate() -> Estimate {
        Estimate {
            duration_p50_secs: Some(30),
            duration_p80_secs: Some(50),
            duration_p90_secs: Some(60),
            resource_p50: None,
            resource_p80: None,
            resource_p90: None,
            confidence: Confidence::Medium,
            sample_count: 8,
            cold_start: false,
            estimator_version: "v3-tiered-confidence".to_string(),
            reason: None,
            feature_schema_version: "fs-v1".to_string(),
            bucket_tier: libra_governor_domain::BucketTier::Global,
            regime: Default::default(),
        }
    }

    fn sample_task(replan_state: ReplanState) -> TaskSummary {
        TaskSummary {
            task_id: TaskId::new(),
            confidence: Confidence::Medium,
            recon_cost_seconds: 2.1,
            plan_id: PlanId::new(),
            remaining_estimate: sample_estimate(),
            replan_state,
        }
    }

    /// A `Status` reply carrying no economic figures at all. The budget
    /// fields are varied explicitly by the tests that care about them
    /// (see `the_parsed_compatibility_line_is_unaffected_by_the_budget_posture`);
    /// everything else here is asserting about the task half of the line,
    /// and a helper keeps that intent legible as the reply grows fields.
    fn status_for(current_task: Option<TaskSummary>) -> StatusResult {
        StatusResult {
            current_task,
            task_budget: None,
            task_budget_amounts: None,
            configured_budget: None,
        }
    }

    #[test]
    fn format_status_reports_idle_when_no_current_task() {
        assert_eq!(format_status(&status_for(None)), "libra: idle");
    }

    #[test]
    fn format_status_renders_a_single_line_with_confidence_and_recon_cost() {
        let status = status_for(Some(sample_task(ReplanState::Stable)));
        let line = format_status(&status);
        assert!(line.starts_with("libra: task "));
        assert!(line.contains("medium"));
        assert!(line.contains("2.1s"));
        assert!(line.contains("stable"));
        assert!(line.contains("60s"), "must include the remaining P90");
        assert!(!line.contains('\n'));
    }

    #[test]
    fn the_parsed_compatibility_line_is_unaffected_by_the_budget_posture() {
        // This surface exists because a founder wrapper greps it. A budget
        // share arriving on the `Status` reply must not move a byte of it:
        // the share belongs to the structured provider document, and adding
        // it here would silently break every pattern downstream.
        // One task value, reused: `sample_task` mints fresh ids per call.
        let task = sample_task(ReplanState::Stable);
        let baseline = format_status(&status_for(Some(task.clone())));
        for posture in [
            BudgetPosture::Remaining {
                fraction_left: 0.38,
            },
            BudgetPosture::Uncommitted,
            BudgetPosture::Exhausted,
            BudgetPosture::NotEstablished,
            BudgetPosture::Unreadable,
        ] {
            let line = format_status(&StatusResult {
                current_task: Some(task.clone()),
                task_budget: Some(posture),
                task_budget_amounts: None,
                configured_budget: None,
            });
            assert_eq!(line, baseline, "{posture:?} changed the legacy line");
            assert!(!line.contains('%'));
        }
    }

    /// HORO-1709 put *amounts* on the same reply, which is a far stronger
    /// temptation to leak into this line than a bare share was — an
    /// amount is exactly what a human reading a statusline wants. It
    /// still must not: this is the legacy text surface a founder wrapper
    /// greps, and the amounts belong to the structured provider
    /// document. Asserted against a full snapshot and a configured
    /// ceiling, not against `None`, so the test would fail if a future
    /// edit started rendering either.
    #[test]
    fn the_parsed_compatibility_line_never_renders_a_budget_amount() {
        use libra_governor_protocol::{
            BudgetSnapshot, ConfiguredBudget, ResourceAmount, ResourceKind,
        };

        let task = sample_task(ReplanState::Stable);
        let baseline = format_status(&status_for(Some(task.clone())));
        let line = format_status(&StatusResult {
            current_task: Some(task),
            task_budget: Some(BudgetPosture::Remaining {
                fraction_left: 0.38,
            }),
            task_budget_amounts: Some(BudgetSnapshot::new(
                ResourceKind::Tokens,
                150_000.0,
                30_000.0,
                18_000.0,
                70_000.0,
                2,
            )),
            configured_budget: Some(ConfiguredBudget {
                ceiling: ResourceAmount::Tokens(150_000),
            }),
        });
        assert_eq!(line, baseline, "budget amounts changed the legacy line");
        for leaked in ["150000", "150,000", "62000", "62,000", "token", "$"] {
            assert!(
                !line.contains(leaked),
                "legacy line leaked {leaked:?}: {line}"
            );
        }
    }

    #[test]
    fn format_status_reflects_a_replanned_state() {
        let status = status_for(Some(sample_task(ReplanState::Replanned { count: 2 })));
        let line = format_status(&status);
        assert!(line.contains("replanned 2x"));
    }

    #[test]
    fn format_status_reflects_escalation() {
        let status = status_for(Some(sample_task(ReplanState::EscalatedAwaitingApproval)));
        let line = format_status(&status);
        assert!(line.contains("escalated"));
        assert!(line.contains("awaiting approval"));
    }

    #[test]
    fn short_task_id_truncates_to_eight_characters() {
        let id = TaskId::new().to_string();
        assert_eq!(short_task_id(&id).len(), 8);
    }
}
