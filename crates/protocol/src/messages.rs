//! Request/response message shapes carried inside a
//! [`crate::RequestEnvelope`] / [`crate::ResponseEnvelope`].

use std::path::PathBuf;

use libra_governor_domain::{
    CompletionContract, Confidence, Estimate, ExecutionReceipt, PlanId, PolicyDecision,
    ResourceAmount, TaskId,
};
use libra_governor_estimator::{AdmissionOutcome, AdmissionPolicy, CoverageReport};
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
    /// Ask the daemon to compute real calibration evidence — duration
    /// coverage and admission-replay metrics — over every locally
    /// recorded receipt paired back to the estimate its plan was made
    /// from (HORO-1132). Answered entirely from local ledger state;
    /// never triggers new reconnaissance or any LLM call. Sent from
    /// `libra-governor calibration report`.
    CalibrationReport,
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
    /// The [`PlanId`] of the `ExecutionPlan` this preflight produced
    /// (HORO-1139) — so a client can correlate a later `Status` render's
    /// `TaskSummary::plan_id` back to "the plan this preflight created"
    /// without a separate lookup.
    pub plan_id: PlanId,
    /// The admission decision (HORO-1137's `Policy::evaluate`, first
    /// wired up into the daemon in HORO-1141) for this preflight's
    /// projected resource/time requirement. `None` only if a task's
    /// budget could not be resolved at all — never `None` on a normal
    /// preflight.
    pub admission: Option<PolicyDecision>,
    /// The protected Completion Reserve held for this task's required
    /// completion work (HORO-1141), after this preflight's own
    /// recomputation. `None` only if a task's budget could not be
    /// resolved at all.
    pub completion_reserve: Option<ResourceAmount>,
}

/// The result of a `Status` request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusResult {
    pub current_task: Option<TaskSummary>,
}

/// A compact summary of the daemon's most recently produced preflight,
/// suitable for a one-line statusline render.
///
/// `remaining_estimate`/`replan_state`/`plan_id` (HORO-1139) are the
/// runtime-replanning visibility surface: after a material replan they
/// reflect the *current* remaining-work estimate and plan, not the
/// original preflight one — see `libra-governor-daemon`'s `ToolInvoked`
/// handling and `crates/cli/src/statusline.rs`'s render of this type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskSummary {
    pub task_id: TaskId,
    /// Confidence of the *current* best estimate — the original
    /// preflight estimate's confidence until a replan happens, then the
    /// (possibly downgraded) remaining estimate's confidence.
    pub confidence: Confidence,
    pub recon_cost_seconds: f64,
    /// The plan this summary currently reflects: the original preflight
    /// plan, or the most recent replan's new plan.
    pub plan_id: PlanId,
    /// The current best remaining-work estimate: the original preflight
    /// [`Estimate`] until a replan happens, then the most recent
    /// replan's recomputed one (HORO-1139).
    pub remaining_estimate: Estimate,
    pub replan_state: ReplanState,
}

/// Runtime replanning state for one task, as of the daemon's most recent
/// `ToolInvoked` handling (HORO-1139) — the statusline-visible half of
/// "govern the run."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ReplanState {
    /// No material deviation has triggered a replan yet.
    Stable,
    /// This task has been automatically replanned `count` times so far
    /// (still within its hysteresis budget — see
    /// `libra_governor_domain::ReplanHysteresisConfig`).
    Replanned { count: u32 },
    /// This task's automatic-replan budget is exhausted; the next
    /// material event will not silently replan again — it needs human
    /// approval (see `libra_governor_domain::HysteresisOutcome::EscalateApprovalNeeded`).
    EscalatedAwaitingApproval,
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

/// One [`AdmissionPolicy`] the daemon replayed local history against,
/// paired with the real outcome of that replay. `CalibrationReport`
/// carries a `Vec` of these (rather than a single `Option<AdmissionOutcome>`)
/// because the daemon replays more than one reasonable default policy —
/// see `handle_calibration_report` in `libra-governor-daemon`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdmissionPolicyReport {
    pub policy: AdmissionPolicy,
    pub outcome: AdmissionOutcome,
}

/// The result of a `CalibrationReport` request (HORO-1132): real duration
/// coverage plus admission-replay outcomes for one or more default
/// policies, computed over every locally recorded receipt paired back to
/// its originating estimate. See
/// `libra_governor_estimator::calibration` for what `coverage` and each
/// `admission` entry can honestly say when there is not yet enough real
/// local evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CalibrationReportResult {
    pub coverage: CoverageReport,
    pub admission: Vec<AdmissionPolicyReport>,
    /// How many locally recorded receipts were excluded from `coverage`
    /// and `admission` because their plan carried no estimate at all, or
    /// a cold-start one — see
    /// `libra_governor_ledger::LedgerStore::calibration_pairs` docs.
    /// Surfaced rather than silently dropped.
    pub dropped_rows: usize,
}

/// One response the daemon may send back.
///
/// `Preflight` is boxed: `PreflightResult` (contract draft + recon
/// summary + `Estimate`) is materially larger than every other variant,
/// which would otherwise trip clippy's `large_enum_variant` lint on this
/// enum the same way it did on [`FinalizeOutcome`] — see that type's
/// docs for the underlying reasoning. `CalibrationReport` is boxed for
/// the same reason: it carries a full `CoverageReport` (per-quantile,
/// per-stratum breakdowns) plus a `Vec<AdmissionPolicyReport>`. `Status`
/// is boxed as of HORO-1139: `TaskSummary` grew a full `Estimate`
/// (`remaining_estimate`), pushing `StatusResult` past the same
/// large-enum-variant threshold.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    Preflight(Box<PreflightResult>),
    Status(Box<StatusResult>),
    Finalize(FinalizeOutcome),
    /// Acknowledges a fire-and-forget request (`ToolInvoked`) with no
    /// further payload.
    Ack,
    CalibrationReport(Box<CalibrationReportResult>),
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
            plan_id: PlanId::new(),
            admission: None,
            completion_reserve: None,
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

    #[test]
    fn calibration_report_request_round_trips() {
        let request = Request::CalibrationReport;
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(json, r#"{"kind":"calibration_report"}"#);
        let round_tripped: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(request, round_tripped);
    }

    /// `CalibrationReportResult` embeds two internally-tagged enums of its
    /// own (`CoverageReport` and `AdmissionOutcome`, both tagged
    /// `"state"`) inside `Response`'s own `"kind"`-tagged enum — precisely
    /// the shape that produced a real tag-collision bug during HORO-1126
    /// (see [`FinalizeOutcome`]'s docs). This round-trips a full envelope
    /// with real `Computed` variants on both nested enums to prove they
    /// nest without colliding.
    #[test]
    fn calibration_report_response_round_trips_through_a_full_envelope() {
        use libra_governor_estimator::{AdmissionStats, QuantileCoverage};

        let envelope = ResponseEnvelope {
            protocol_version: 1,
            response: Response::CalibrationReport(Box::new(CalibrationReportResult {
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
            })),
        };

        let json = serde_json::to_string(&envelope).unwrap();
        let round_tripped: ResponseEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(envelope, round_tripped);
    }
}
