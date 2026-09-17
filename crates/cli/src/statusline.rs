//! `libra-governor statusline` — Claude Code `statusLine` command.
//!
//! Reads the daemon's current task/preflight state over the same IPC the
//! hook uses and prints one short line. Never spawns the daemon (a
//! statusline refreshes on a short interval; spawning from it would be a
//! race factory — see `crates/cli/src/client.rs::connect_only`) and
//! never makes an LLM call of its own. Always prints exactly one line
//! and exits 0, even when the daemon is unreachable.

use libra_governor_protocol::{Confidence, Request, Response, StatusResult};

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
            let confidence = match task.confidence {
                Confidence::Low => "low",
                Confidence::Medium => "medium",
                Confidence::High => "high",
            };
            format!(
                "libra: task {} | preflight: {confidence} | recon: {:.1}s",
                short_task_id(&task.task_id.to_string()),
                task.recon_cost_seconds
            )
        }
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
    use libra_governor_domain::TaskId;
    use libra_governor_protocol::TaskSummary;

    #[test]
    fn format_status_reports_idle_when_no_current_task() {
        let status = StatusResult { current_task: None };
        assert_eq!(format_status(&status), "libra: idle");
    }

    #[test]
    fn format_status_renders_a_single_line_with_confidence_and_recon_cost() {
        let status = StatusResult {
            current_task: Some(TaskSummary {
                task_id: TaskId::new(),
                confidence: Confidence::Medium,
                recon_cost_seconds: 2.1,
            }),
        };
        let line = format_status(&status);
        assert!(line.starts_with("libra: task "));
        assert!(line.contains("medium"));
        assert!(line.contains("2.1s"));
        assert!(!line.contains('\n'));
    }

    #[test]
    fn short_task_id_truncates_to_eight_characters() {
        let id = TaskId::new().to_string();
        assert_eq!(short_task_id(&id).len(), 8);
    }
}
