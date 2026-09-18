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
fn short_task_id(task_id: &str) -> &str {
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
    use libra_governor_protocol::TaskSummary;

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

    #[test]
    fn format_status_reports_idle_when_no_current_task() {
        let status = StatusResult { current_task: None };
        assert_eq!(format_status(&status), "libra: idle");
    }

    #[test]
    fn format_status_renders_a_single_line_with_confidence_and_recon_cost() {
        let status = StatusResult {
            current_task: Some(sample_task(ReplanState::Stable)),
        };
        let line = format_status(&status);
        assert!(line.starts_with("libra: task "));
        assert!(line.contains("medium"));
        assert!(line.contains("2.1s"));
        assert!(line.contains("stable"));
        assert!(line.contains("60s"), "must include the remaining P90");
        assert!(!line.contains('\n'));
    }

    #[test]
    fn format_status_reflects_a_replanned_state() {
        let status = StatusResult {
            current_task: Some(sample_task(ReplanState::Replanned { count: 2 })),
        };
        let line = format_status(&status);
        assert!(line.contains("replanned 2x"));
    }

    #[test]
    fn format_status_reflects_escalation() {
        let status = StatusResult {
            current_task: Some(sample_task(ReplanState::EscalatedAwaitingApproval)),
        };
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
