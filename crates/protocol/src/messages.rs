//! Request/response message shapes carried inside a
//! [`crate::RequestEnvelope`] / [`crate::ResponseEnvelope`].

use std::path::PathBuf;

use libra_governor_domain::{
    BusinessContextSummary, CompletionContract, Confidence, EnforcementCapabilities, Estimate,
    ExecutionOutcome, ExecutionReceipt, PlanId, PolicyDecision, ResourceAmount, TaskId,
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
    /// Ask the daemon whether the optional enforcement gateway is
    /// running, what enforcement tier it is operating at, and what it has
    /// admitted/refused so far (HORO-1144). Answered entirely from state
    /// the daemon already holds; never triggers a provider call.
    ///
    /// Read-only by construction: there is deliberately no request
    /// variant that starts, stops, or reconfigures the gateway. The
    /// gateway is a security boundary, and a boundary that a client can
    /// turn off over an IPC socket is not one.
    GatewayStatus,
    /// Ask the daemon for a read-only diagnostic snapshot of its own
    /// health (HORO-1150): daemon/schema version, whether `config.json`
    /// parsed, which policy preset is active, and the gateway's
    /// configuration presence and capability tier. Answered entirely from
    /// state the daemon already holds or can cheaply re-check (a fresh
    /// re-read of `config.json`, the same pattern `GatewayStatus` already
    /// uses) — never triggers a provider call and never returns a secret
    /// value, only presence/absence of one. The `libra-governor doctor`
    /// CLI subcommand pairs this with its own local-file checks (Claude
    /// Code settings wiring, state directory permissions) that do not
    /// require a daemon round trip.
    Doctor,
    /// Pushes an outcome attestation for `task_id` (optionally narrowed to
    /// `plan_id`) over the daemon's existing Unix socket (HORO-1174) —
    /// the one new *inbound* path this ticket adds, deliberately reusing
    /// the socket rather than opening a new HTTP listener (see
    /// `docs/adr/0005-local-extension-points.md`). `idempotency_key`
    /// deduplicates a retried push from the same `(task_id, source_id)`:
    /// a duplicate is [`Response::OutcomeRecorded`]'s
    /// [`OutcomeRecordedOutcome::Duplicate`], not a second write.
    /// `source_id` is a recorded claim, not an authenticated identity —
    /// the boundary is filesystem permissions on the socket itself (0600
    /// inside the daemon's 0700 state dir), the same boundary every other
    /// `Request` variant already relies on.
    RecordOutcome {
        task_id: TaskId,
        plan_id: Option<PlanId>,
        source_id: String,
        idempotency_key: String,
        outcome: ExecutionOutcome,
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
    /// The Business Context Provider's response, when one is configured
    /// and the fetch succeeded (HORO-1174). `None` when no provider is
    /// configured, or when the fetch failed/timed out/returned a
    /// malformed response (fail-open — see
    /// `docs/adr/0005-local-extension-points.md`). Advisory metadata
    /// only: nothing on this type ever reaches `admission.protected_criteria`
    /// — see `libra_governor_domain::BusinessContextSummary` docs for the
    /// R2 trust-boundary rule this field's presence does not weaken.
    pub business_context: Option<BusinessContextSummary>,
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

/// The result of a `GatewayStatus` request (HORO-1144).
///
/// Carries the honest capability statement rather than a
/// supported/unsupported boolean — see
/// `libra_governor_domain::EnforcementCapabilities` for why. Every
/// counter is an aggregate; there is no per-request detail here, and no
/// field that could carry a model name, a session id, or a credential.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GatewayStatusResult {
    /// `false` when no gateway is configured, or when one was configured
    /// but its validation or credential resolution failed at startup — in
    /// which case `disabled_reason` says which.
    pub running: bool,
    /// Why the gateway is not running, when it is not. Never contains a
    /// credential or any captured command output.
    pub disabled_reason: Option<String>,
    /// What this deployment may honestly claim to enforce. `None` when no
    /// gateway is configured at all.
    pub capabilities: Option<EnforcementCapabilities>,
    /// The loopback address the gateway listens on, when running.
    pub bind_addr: Option<String>,
    pub forwarded: u64,
    pub denied_budget: u64,
    pub denied_unenforceable: u64,
    pub denied_unauthorized: u64,
    pub approval_gated: u64,
    pub settled_with_known_usage: u64,
    pub settled_without_usage: u64,
    pub upstream_errors: u64,
    /// How many settled requests reported more output tokens than their
    /// own `max_tokens` declared. Should always be zero; surfaced rather
    /// than hidden because a nonzero value means the reservation
    /// arithmetic's bound was violated.
    pub bound_violations: u64,
}

/// The result of a `Doctor` request (HORO-1150).
///
/// Every field is either a version/count already tracked elsewhere
/// (daemon crate version, protocol version, applied vs. latest-known
/// schema migration) or a presence/absence boolean — never a credential,
/// a config file path's contents beyond what is already logged, or any
/// other secret value. `config_file_error`, when present, is the
/// [`std::fmt::Display`] of the same `ConfigFileError` the daemon already
/// logs to `daemon.log` on startup — never a raw credential-command
/// argument, since `config.json`'s `credential_command`/`credential_args`
/// fields never contain a secret themselves (they name a program to run,
/// not the credential it prints).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DoctorResult {
    /// This daemon build's `CARGO_PKG_VERSION`.
    pub daemon_version: String,
    /// The protocol version this daemon build speaks — always equal to
    /// the responding daemon's own [`crate::PROTOCOL_VERSION`] (a client
    /// on a different version never reaches this far; see
    /// `libra-governor-daemon::server::handle_connection`'s pre-dispatch
    /// version check).
    pub protocol_version: u32,
    /// The highest `schema_migrations.version` actually applied to this
    /// daemon's open ledger connection.
    pub schema_version_applied: i64,
    /// The highest migration version this daemon build knows about.
    /// Equal to `schema_version_applied` on a healthy, up-to-date
    /// database; lower would mean the ledger is somehow ahead of this
    /// binary (a downgrade), which [`Self::schema_ahead_of_binary`]
    /// names explicitly rather than leaving the reader to compare the
    /// two numbers themselves.
    pub schema_version_known: i64,
    /// `true` when `schema_version_applied > schema_version_known` — this
    /// binary is older than the database it just opened (e.g. a
    /// downgrade, or two builds sharing one state dir). A real, observed
    /// condition, not a guess.
    pub schema_ahead_of_binary: bool,
    /// The name of the admission [`Policy`][libra_governor_domain::Policy]
    /// preset currently in effect (`"balanced"` unless a valid
    /// `config.json` `[policy]` table selected another one).
    pub policy_preset: String,
    /// `true` if `<state_dir>/config.json` exists at all.
    pub config_file_present: bool,
    /// `true` if `config.json` exists and parsed/validated successfully.
    /// `false` when the file is present but rejected (in which case the
    /// daemon is running on its hardcoded defaults, not what the file
    /// says) — always `true` when `config_file_present` is `false`, since
    /// there is nothing to fail to parse.
    pub config_file_valid: bool,
    /// Why `config.json` was rejected, when it was present but invalid.
    pub config_file_error: Option<String>,
    /// `true` when the running daemon's in-memory config already
    /// reflects what's currently on disk in `config.json` — `false`
    /// means the file was edited (policy preset and/or gateway presence
    /// changed) since this daemon process last read it at startup, and a
    /// restart is needed to pick the change up. Always `true` when
    /// `config_file_present` is `false` (nothing on disk to disagree
    /// with) or when `config_file_valid` is `false` (a rejected file
    /// changes nothing, so there is no drift to report). Computed by
    /// re-reading `config.json` fresh on every `doctor` call and
    /// comparing its policy name and gateway presence against the
    /// values the running config actually reports below.
    pub running_config_matches_disk: bool,
    /// `true` when a `[gateway]` table is configured at all (regardless
    /// of whether it actually started — see `gateway_running`).
    pub gateway_configured: bool,
    /// `true` when the gateway is actually running right now. Mirrors
    /// [`GatewayStatusResult::running`].
    pub gateway_running: bool,
    /// Why the gateway is not running, when it is not (and one was
    /// configured) — mirrors [`GatewayStatusResult::disabled_reason`].
    pub gateway_disabled_reason: Option<String>,
    /// What this deployment may honestly claim to enforce — mirrors
    /// [`GatewayStatusResult::capabilities`].
    pub gateway_capabilities: Option<EnforcementCapabilities>,
    /// `true` only when the gateway is configured for
    /// `credential_mode: governor_held` — i.e. this daemon itself holds
    /// (invokes a `credential_command` for) a credential, as opposed to
    /// `pass_through_subscription`, where the daemon holds nothing and
    /// simply relays the agent's own credential through unmodified.
    /// Presence only, never the credential's value or the command's
    /// output. Deliberately narrower than "any gateway credential mode
    /// is configured" (which would be redundant with `gateway_configured`
    /// whenever a `[gateway]` table exists at all) — this field exists to
    /// answer the one question that actually varies: does the daemon
    /// hold a credential of its own.
    pub gateway_credential_configured: bool,
    /// Always `false` in this build: no telemetry code path exists
    /// anywhere in this repository (see `ARCHITECTURE.md`'s privacy
    /// boundary) — this is a real, observed absence, not a fabricated
    /// claim. Present as a field (rather than left to prose) so
    /// `doctor --json` can be asserted on by a caller that wants to
    /// verify it itself.
    pub telemetry_enabled: bool,
    /// `true` when `extensions.business_context_provider` is configured
    /// in `config.json` (HORO-1174). Presence only — never the URL or
    /// secret command.
    pub extension_business_context_configured: bool,
    /// `true` when `extensions.policy_webhook` is configured.
    pub extension_policy_webhook_configured: bool,
    /// `true` when `extensions.events` is configured (and therefore the
    /// event-dispatcher thread was started).
    pub extension_events_configured: bool,
    /// How many `webhook_deliveries` rows are still `pending` right now.
    /// `0` when `extension_events_configured` is `false` (nothing to
    /// deliver).
    pub extension_events_pending: u64,
    /// Why the `[extensions]` block was rejected, when it was present but
    /// invalid — mirrors `config_file_error`'s discipline: the
    /// [`std::fmt::Display`] of the same error the daemon already logs,
    /// never a raw secret command argument (`secret_command`/
    /// `secret_args` never contain a secret value themselves — they name
    /// a program to run, not the secret it prints).
    pub extension_config_error: Option<String>,
}

/// Everything recorded from a successful `RecordOutcome` push (HORO-1174).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutcomeRecordedResult {
    pub attested: ExecutionOutcome,
    /// `true` if this push also promoted `receipts.outcome_json` — see
    /// `libra_governor_ledger::LedgerStore::promote_receipt_outcome`.
    /// `false` when the attestation was recorded but no receipt existed
    /// yet to promote (an outcome pushed before `Finalize` ever ran for
    /// this task), or when the attestation's
    /// `AttestationSource::is_authoritative()` was `false` (an
    /// `Agent`-sourced attestation is recorded but never promotes).
    pub receipt_updated: bool,
}

/// The outcome of a `RecordOutcome` request (HORO-1174). Tagged `"state"`,
/// mirroring [`FinalizeOutcome`]'s own discipline for the exact same
/// reason — see that type's docs on the tag-collision bug this avoids.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum OutcomeRecordedOutcome {
    /// `task_id` has no `tasks` row at all.
    NoSuchTask,
    /// This exact `(task_id, source_id, idempotency_key)` was already
    /// recorded — a safe no-op replay, not an error.
    Duplicate,
    /// Boxed for the same large-enum-variant reason as
    /// [`FinalizeOutcome::Finalized`].
    Recorded(Box<OutcomeRecordedResult>),
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
    /// Boxed for the same large-enum-variant reason as its siblings:
    /// `GatewayStatusResult` carries an `EnforcementCapabilities` plus
    /// nine counters.
    GatewayStatus(Box<GatewayStatusResult>),
    /// Boxed for the same large-enum-variant reason as its siblings:
    /// `DoctorResult` carries an optional `EnforcementCapabilities` plus
    /// several `String`/`Option<String>` fields.
    Doctor(Box<DoctorResult>),
    /// Answers a `RecordOutcome` request (HORO-1174).
    OutcomeRecorded(OutcomeRecordedOutcome),
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
    use crate::PROTOCOL_VERSION;
    use libra_governor_domain::EnforcementTier;

    #[test]
    fn doctor_request_round_trips() {
        let json = serde_json::to_string(&Request::Doctor).unwrap();
        assert_eq!(json, r#"{"kind":"doctor"}"#);
        let round_tripped: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(Request::Doctor, round_tripped);
    }

    #[test]
    fn doctor_response_round_trips_through_a_full_envelope() {
        let envelope = ResponseEnvelope {
            protocol_version: 8,
            response: Response::Doctor(Box::new(DoctorResult {
                daemon_version: "0.0.0".to_string(),
                protocol_version: 8,
                schema_version_applied: 9,
                schema_version_known: 9,
                schema_ahead_of_binary: false,
                policy_preset: "balanced".to_string(),
                config_file_present: false,
                config_file_valid: true,
                config_file_error: None,
                running_config_matches_disk: true,
                gateway_configured: true,
                gateway_running: true,
                gateway_disabled_reason: None,
                gateway_capabilities: Some(EnforcementCapabilities::for_tier(
                    EnforcementTier::GatewayObservedQuota,
                    "pricing-test-v1",
                )),
                gateway_credential_configured: true,
                telemetry_enabled: false,
                extension_business_context_configured: false,
                extension_policy_webhook_configured: false,
                extension_events_configured: false,
                extension_events_pending: 0,
                extension_config_error: None,
            })),
        };
        let json = serde_json::to_string(&envelope).unwrap();
        let round_tripped: ResponseEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(envelope, round_tripped);
    }

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
            business_context: None,
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

    #[test]
    fn record_outcome_request_round_trips() {
        let request = Request::RecordOutcome {
            task_id: TaskId::new(),
            plan_id: Some(PlanId::new()),
            source_id: "example-provider".to_string(),
            idempotency_key: "ci-run-42".to_string(),
            outcome: libra_governor_domain::ExecutionOutcome::Completed {
                evidence: vec!["https://ci.example.com/runs/42".to_string()],
            },
        };
        let json = serde_json::to_string(&request).unwrap();
        let round_tripped: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(request, round_tripped);
    }

    #[test]
    fn outcome_recorded_outcome_round_trips_every_variant() {
        for outcome in [
            OutcomeRecordedOutcome::NoSuchTask,
            OutcomeRecordedOutcome::Duplicate,
            OutcomeRecordedOutcome::Recorded(Box::new(OutcomeRecordedResult {
                attested: libra_governor_domain::ExecutionOutcome::Completed { evidence: vec![] },
                receipt_updated: true,
            })),
        ] {
            let envelope = ResponseEnvelope {
                protocol_version: PROTOCOL_VERSION,
                response: Response::OutcomeRecorded(outcome.clone()),
            };
            let json = serde_json::to_string(&envelope).unwrap();
            let round_tripped: ResponseEnvelope = serde_json::from_str(&json).unwrap();
            assert_eq!(envelope, round_tripped);
        }
    }

    #[test]
    fn preflight_result_business_context_defaults_absent_but_present_in_schema() {
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
            business_context: None,
        };
        let json = serde_json::to_value(&result).unwrap();
        assert!(json.get("business_context").is_some());
        assert!(json["business_context"].is_null());
    }
}
