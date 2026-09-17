//! Request/response message shapes carried inside a
//! [`crate::RequestEnvelope`] / [`crate::ResponseEnvelope`].

use std::path::PathBuf;

use libra_governor_domain::{CompletionContract, TaskId};
use serde::{Deserialize, Serialize};

/// A request envelope: a required, non-defaulted protocol version plus
/// the request payload. See crate docs on why the version is required.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestEnvelope {
    pub protocol_version: u32,
    pub request: Request,
}

/// A response envelope: mirrors [`RequestEnvelope`]'s versioning
/// discipline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseEnvelope {
    pub protocol_version: u32,
    pub response: Response,
}

/// One request a client may send the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Request {
    /// Ask the daemon to run bounded reconnaissance against `cwd` for the
    /// given prompt hint and return a preflight result (draft Completion
    /// Contract + reconnaissance summary), creating or reusing the task
    /// identity associated with `session_id`.
    Preflight {
        /// The user's submitted prompt text (or a truncated hint of it).
        /// Crosses the socket because it is the recon/heuristic input,
        /// but is never persisted to the ledger or written to a log —
        /// see the daemon's privacy handling.
        task_hint: String,
        cwd: PathBuf,
        session_id: String,
    },
    /// Ask the daemon for its current task/preflight state. Answered
    /// entirely from state the daemon already holds — never triggers new
    /// reconnaissance or any LLM call.
    Status,
}

/// How much the daemon trusts a produced [`PreflightResult`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// Insufficient evidence: an explicit reason is carried on
    /// [`ReconSummary::reason`] rather than silently expanding scope or
    /// guessing confidently.
    Low,
    Medium,
    High,
}

/// Summary of one bounded reconnaissance run. Never contains raw file
/// contents or raw prompt text — only structural signal (paths, detected
/// tooling) and accounting (counts, whether the budget was hit).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReconSummary {
    pub files_scanned: usize,
    pub dirs_scanned: usize,
    /// Repo-relative paths whose name plausibly matches something
    /// mentioned in the prompt (deterministic keyword/path matching —
    /// never an LLM call).
    pub likely_affected_paths: Vec<String>,
    /// Test/build commands implied by detected project files (e.g.
    /// `Cargo.toml` -> `cargo test`).
    pub detected_test_commands: Vec<String>,
    /// `true` if the walk stopped because it hit its time or size budget
    /// rather than exhausting the tree naturally.
    pub truncated: bool,
    /// Present when [`Confidence::Low`]: why the daemon considers the
    /// evidence insufficient.
    pub reason: Option<String>,
}

/// The result of one `Preflight` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PreflightResult {
    pub task_id: TaskId,
    /// The draft Completion Contract inferred from recon + prompt. The
    /// user may correct it later (out of scope for this ticket).
    pub contract_draft: CompletionContract,
    pub recon_summary: ReconSummary,
    pub confidence: Confidence,
    /// Wall-clock cost of the reconnaissance itself, in seconds — recon
    /// is not free and is accounted for explicitly.
    pub recon_cost_seconds: f64,
    /// Placeholder for the probabilistic cost/time estimate HORO-1126
    /// will compute. Always `None` as produced by this ticket's code;
    /// the field exists so `PreflightResult` is structurally ready for
    /// HORO-1126 to populate without another protocol version bump.
    pub estimate: Option<serde_json::Value>,
}

/// The result of a `Status` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusResult {
    pub current_task: Option<TaskSummary>,
}

/// A compact summary of the daemon's most recently produced preflight,
/// suitable for a one-line statusline render.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskSummary {
    pub task_id: TaskId,
    pub confidence: Confidence,
    pub recon_cost_seconds: f64,
}

/// One response the daemon may send back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Preflight(PreflightResult),
    Status(StatusResult),
    /// The daemon could not (or would not) answer the request — e.g. a
    /// protocol version mismatch, or an internal error it caught rather
    /// than let propagate as a crash.
    Error {
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_envelope_round_trips_through_json() {
        let envelope = RequestEnvelope {
            protocol_version: 1,
            request: Request::Preflight {
                task_hint: "fix the login bug".to_string(),
                cwd: PathBuf::from("/repo"),
                session_id: "sess-1".to_string(),
            },
        };
        let json = serde_json::to_string(&envelope).unwrap();
        let round_tripped: RequestEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(envelope, round_tripped);
    }

    #[test]
    fn status_request_serializes_with_kind_tag() {
        let json = serde_json::to_string(&Request::Status).unwrap();
        assert_eq!(json, r#"{"kind":"status"}"#);
    }

    #[test]
    fn missing_protocol_version_field_fails_to_deserialize() {
        // protocol_version must be required, never defaulted: skew must
        // fail loud, not silently coerce to some default version.
        let json = r#"{"request":{"kind":"status"}}"#;
        let result: Result<RequestEnvelope, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn response_error_round_trips() {
        let envelope = ResponseEnvelope {
            protocol_version: 1,
            response: Response::Error {
                message: "protocol version mismatch".to_string(),
            },
        };
        let json = serde_json::to_string(&envelope).unwrap();
        let round_tripped: ResponseEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(envelope, round_tripped);
    }

    #[test]
    fn preflight_result_estimate_defaults_absent_but_present_in_schema() {
        let result = PreflightResult {
            task_id: TaskId::new(),
            contract_draft: CompletionContract::first(vec![]),
            recon_summary: ReconSummary {
                files_scanned: 0,
                dirs_scanned: 0,
                likely_affected_paths: vec![],
                detected_test_commands: vec![],
                truncated: false,
                reason: None,
            },
            confidence: Confidence::Low,
            recon_cost_seconds: 0.01,
            estimate: None,
        };
        let json = serde_json::to_value(&result).unwrap();
        assert!(
            json.get("estimate").is_some(),
            "field must serialize as null, not be omitted"
        );
        assert!(json["estimate"].is_null());
    }
}
