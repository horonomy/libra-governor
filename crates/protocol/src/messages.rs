//! Request/response message shapes carried inside a
//! [`crate::RequestEnvelope`] / [`crate::ResponseEnvelope`].

use std::path::PathBuf;

use libra_governor_domain::{CompletionContract, Confidence, Estimate, ExecutionReceipt, TaskId};
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
    /// Fire-and-forget notification that a tool was invoked in
    /// `session_id`. The daemon increments a per-session counter and
    /// replies [`Response::Ack`]; it never records a full
    /// `ExecutionEvent` or does any other work here, so this stays cheap
    /// enough not to add perceptible latency to every tool call (see
    /// `libra-governor-cli`'s `hook post-tool-use`, HORO-1126).
    ToolInvoked {
        session_id: String,
        tool_name: String,
    },
    /// Ask the daemon to finalize the task bound to `session_id`: compute
    /// elapsed duration, gather the tool-call count, and persist an
    /// [`libra_governor_domain::ExecutionReceipt`]. Answered with
    /// [`Response::Finalize`]. Sent from `hook stop`
    /// (HORO-1126). `model`, if the harness's hook payload exposed one,
    /// is recorded on the receipt as-is; harnesses that do not expose it
    /// (see [`libra_governor_domain::ExecutionReceipt::provider`] docs)
    /// leave it `None`.
    Finalize {
        session_id: String,
        model: Option<String>,
    },
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
    /// The probabilistic cost/time estimate computed by
    /// `libra-governor-estimator` from local `ExecutionReceipt` history
    /// (HORO-1126). Structurally always `Some` in practice — even a
    /// cold-start (zero local history) result is a real, honestly-flagged
    /// [`Estimate`] (see [`Estimate::cold_start`]) rather than `None`; the
    /// field stays `Option` only so a hand-built or historical
    /// `PreflightResult` without one still deserializes.
    pub estimate: Option<Estimate>,
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

/// The outcome of a `Finalize` request.
///
/// A plain `Option<FinalizeResult>` would leave "no active task for this
/// session" and "task existed but its receipt somehow could not be built"
/// indistinguishable from each other and from a deserialize bug; this
/// enum keeps the no-active-task case an explicit, named, structurally
/// impossible-to-confuse variant (see `hook stop`'s "safe no-op" edge
/// case, HORO-1126).
///
/// Tagged `"state"`, not `"kind"`: this type is only ever carried inside
/// [`Response::Finalize`]'s newtype variant, and [`Response`] is itself
/// internally tagged with `"kind"`. Serde inserts an internally-tagged
/// enum's tag key directly into its newtype-variant payload's own map,
/// so tagging both enums `"kind"` would collide — the outer variant name
/// (`"finalize"`) and this type's own discriminant would both try to
/// occupy the same JSON key, producing a `{"kind":"finalize","kind":
/// "no_active_task"}`-shaped object that fails to round-trip (a real bug
/// hit and fixed during HORO-1126 development — see PR description).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum FinalizeOutcome {
    /// `session_id` has no task bound to it — e.g. `Stop` fired with no
    /// preceding `Preflight`/`UserPromptSubmit` for this session. Not an
    /// error: finalizing is a safe no-op.
    NoActiveTask,
    /// Boxed: `FinalizeResult` embeds a full `ExecutionReceipt`, which
    /// made this enum's largest variant ~336 bytes against `NoActiveTask`'s
    /// zero — clippy's `large_enum_variant` lint. Boxing keeps every
    /// `FinalizeOutcome` (and therefore every `Response`) the size of a
    /// pointer regardless of which variant it holds.
    Finalized(Box<FinalizeResult>),
}

/// The persisted receipt plus the original estimate it is being compared
/// against, returned together so `hook stop` can render the
/// estimate-vs-actual summary without a second round trip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FinalizeResult {
    pub receipt: ExecutionReceipt,
    /// The estimate recorded on the plan this receipt's `plan_id` points
    /// to, if the plan carried one (see [`Estimate::cold_start`] — even a
    /// cold-start plan carries `Some`, so this is `None` only for a plan
    /// predating HORO-1126, which cannot exist in a fresh MVP 1.0
    /// deployment but could in a database upgraded in place).
    pub estimate: Option<Estimate>,
}

/// One response the daemon may send back.
///
/// `Preflight` is boxed: `PreflightResult` (contract draft + recon
/// summary + `Estimate`) is materially larger than every other variant,
/// which would otherwise trip clippy's `large_enum_variant` lint on this
/// enum the same way it did on [`FinalizeOutcome`] — see that type's
/// docs for the underlying reasoning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Preflight(Box<PreflightResult>),
    Status(StatusResult),
    Finalize(FinalizeOutcome),
    /// Acknowledges a fire-and-forget request (`ToolInvoked`) with no
    /// further payload.
    Ack,
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

    #[test]
    fn finalize_outcome_no_active_task_round_trips() {
        let outcome = FinalizeOutcome::NoActiveTask;
        let json = serde_json::to_string(&outcome).unwrap();
        let round_tripped: FinalizeOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(outcome, round_tripped);
    }

    #[test]
    fn tool_invoked_request_round_trips() {
        let request = Request::ToolInvoked {
            session_id: "sess-1".to_string(),
            tool_name: "Bash".to_string(),
        };
        let json = serde_json::to_string(&request).unwrap();
        let round_tripped: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(request, round_tripped);
    }

    #[test]
    fn finalize_request_round_trips_with_and_without_model() {
        for model in [None, Some("claude-sonnet-5".to_string())] {
            let request = Request::Finalize {
                session_id: "sess-1".to_string(),
                model,
            };
            let json = serde_json::to_string(&request).unwrap();
            let round_tripped: Request = serde_json::from_str(&json).unwrap();
            assert_eq!(request, round_tripped);
        }
    }
}
