//! Request/response message shapes carried inside a
//! [`crate::RequestEnvelope`] / [`crate::ResponseEnvelope`].

use std::path::PathBuf;

use libra_governor_domain::{
    BudgetSnapshot, BusinessContextSummary, CompletionContract, Confidence,
    EnforcementCapabilities, Estimate, ExecutionOutcome, ExecutionReceipt, PlanId, PolicyDecision,
    ResourceAmount, TaskId,
};
use libra_governor_estimator::{
    AdmissionOutcome, AdmissionPolicy, CoverageReport, RegimeCalibrationReport,
};
use serde::{Deserialize, Serialize};

/// A request envelope: a required, non-defaulted protocol version plus
/// the request payload. See crate docs on why the version is required.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    ExecutionOwner {
        event: Box<crate::ExecutionOwnerRequest>,
    },
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
    /// leave it `None`. `provider` is never guessed from the harness's
    /// own payload (neither host's `Stop` payload exposes one) — it is
    /// the calling CLI entry point's own known agent-kind label
    /// (`"claude-code"` / `"codex"`), set by `hook stop` /
    /// `codex-hook stop` respectively (HORO-1689 follow-up: distinct
    /// from `model`, which still requires a `SessionStart` hook to
    /// capture and remains `None` until that's wired).
    Finalize {
        session_id: String,
        model: Option<String>,
        provider: Option<String>,
        /// The host's own transcript for this session, when its hook
        /// payload exposed one (Claude Code does as `transcript_path`;
        /// Codex does not, so this is `None` there — see
        /// `docs/adr/0004-agent-adapter-contract.md`).
        ///
        /// A path, not a token count, on purpose (HORO-1725). The CLI
        /// could parse the four usage counts itself and send integers,
        /// but that would make a spend figure caller-supplied, and
        /// `libra_governor_domain::Reservation`'s docs are explicit that
        /// no caller-supplied value may "forge a spend or credit by
        /// itself". Measurement authority stays in the daemon; the CLI
        /// relays only what the host told it. See
        /// `libra_governor_daemon::usage` for the bounded read and for
        /// what is (and is not) extracted from the file.
        transcript_path: Option<String>,
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
        /// A verified-provider signature over this exact push (HORO-1727
        /// PR 5b / ADR-0017). `None` for every ordinary push today.
        /// Deliberately NOT a separate `outcome_kind`/`evidence_digest`
        /// the caller asserts — the daemon derives both from this same
        /// request's own `outcome` field before verifying, so a
        /// signature can never describe content other than what is
        /// actually persisted. See `SignedOutcomeClaimWire`'s own docs
        /// for why `task_id`/`plan_id`/`idempotency_key` are not
        /// repeated here either.
        signed_claim: Option<SignedOutcomeClaimWire>,
    },
}

/// The signed portion of a `RecordOutcome` push that is NOT already a
/// top-level field of [`Request::RecordOutcome`] (HORO-1727 PR 5b).
/// `task_id`, `plan_id`, and `idempotency_key` are deliberately not
/// duplicated here — the daemon signs/verifies using the same values
/// already present on the surrounding request, so there is exactly one
/// source of truth for what a claim is about, never two fields that
/// could disagree.
///
/// Still production-unreachable (ADR-0017): nothing in `daemon::server`
/// or `cli` ever constructs a `Some` value for `DaemonConfig::outcome_authority`,
/// so even a caller who populates this field gets treated as
/// `Unverified`, exactly like today.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SignedOutcomeClaimWire {
    /// Unix seconds. Checked against the daemon's configured clock-skew
    /// tolerance, not trusted verbatim.
    pub issued_at: u64,
    /// `v1=<hex>` — the same wire shape `libra_governor_extension::sign`
    /// produces.
    pub signature: String,
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
    /// Legacy Status is the last observed host task, never a current-session lookup.
    pub scope: StatusScope,
    pub current_task: Option<TaskSummary>,
    /// What the reservation ledger says about `current_task`'s resource
    /// envelope (HORO-1634), computed while answering *this* request
    /// rather than cached alongside the task.
    ///
    /// That distinction is the reason it sits here rather than on
    /// [`TaskSummary`]. `TaskSummary` is the daemon's in-memory
    /// projection of the task, refreshed when a replan recomputes the
    /// plan; a budget share changes on every settled reservation, so a
    /// copy stored beside the task would be stale for exactly as long as
    /// nothing replanned. A sibling field cannot be read as "the budget
    /// as of the plan" by mistake.
    ///
    /// `None` when there is no current task: a share of a budget that was
    /// never admitted is not zero, it is absent.
    pub task_budget: Option<BudgetPosture>,
    /// The amounts behind `task_budget` (HORO-1709), in the unit the
    /// task's envelope is actually denominated in.
    ///
    /// This and `task_budget` are two projections of **one**
    /// [`BudgetSnapshot`], read in one ledger transaction, and that is the
    /// whole reason the snapshot type exists. A percentage and a set of
    /// amounts obtained from separate reads can each be correct and still
    /// contradict each other on screen, which is the defect this field
    /// would otherwise have introduced rather than fixed.
    ///
    /// `None` whenever `task_budget` is [`BudgetPosture::NotEstablished`]
    /// or [`BudgetPosture::Unreadable`]: there is no snapshot to project
    /// in either case. It is deliberately `Some` for
    /// [`BudgetPosture::Uncommitted`] — the *ceiling* is authoritative
    /// there even though consumption has never been observed, and
    /// [`BudgetSnapshot::is_observed`] is how a renderer tells the
    /// difference. Withholding the whole snapshot would discard a figure
    /// the ledger genuinely holds.
    ///
    /// Scope: the **active task**, always. Never the daemon's configured
    /// default, and never a host or principal cap — see
    /// `configured_budget` and [`BudgetScope`].
    pub task_budget_amounts: Option<BudgetSnapshot>,
    /// The envelope that would govern the *next* task admitted on this
    /// machine (HORO-1709) — the daemon's running policy ceiling, not a
    /// measurement of anything.
    ///
    /// A different scope from `task_budget_amounts`, carried in a
    /// different field for that reason. The two can disagree in normal
    /// operation and neither is wrong when they do: a task's ceiling is
    /// fixed at admission for the life of the task
    /// (`LedgerStore::initialize_task_budget` never overwrites an
    /// existing row), so editing `config.json` changes what the next task
    /// gets without touching what the running one has.
    ///
    /// It exists because the honest alternative while idle was to report
    /// nothing at all. A daemon with no current task has no active-task
    /// percentage to show — and a surface that filled the gap with
    /// "100% budget left" would be inventing a measurement — but it does
    /// know, authoritatively, what the next task's ceiling would be.
    /// Reporting that as its own scope is the one way to be more
    /// informative than silence without being false.
    ///
    /// Always `Some` on a daemon that resolved a policy, including while
    /// a task is active, so a renderer can show "this task has X, the
    /// next would get Y" without a second request. A renderer must
    /// nonetheless never present it as consumption: see
    /// [`BudgetScope::ConfiguredDefault`].
    pub configured_budget: Option<ConfiguredBudget>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusScope {
    HostLatestObservation,
}

/// Which envelope an economic figure is a figure *of* (HORO-1709).
///
/// Two members, and the distinction is load-bearing rather than
/// bookkeeping: `$32.63` means two different things depending on which
/// one it describes, and a statusline that conflated them would answer
/// "how much do I have left" with a number that was never about this
/// task. Every amount Libra publishes is tagged with one of these.
///
/// There is deliberately **no host/principal/global cap member**. Such a
/// cap is a real thing — a subscription ceiling, an organisation budget —
/// and nothing on this machine holds an authoritative value for one. A
/// variant would have had exactly one possible source: a figure guessed
/// from local configuration and then labelled as if a provider had
/// confirmed it. That is a worse outcome than the absence, because a cap
/// is precisely the figure a user would act on. When an authoritative
/// source exists, the variant arrives with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetScope {
    /// The envelope of the task being governed right now, backed by the
    /// reservation ledger. The only scope whose figures describe
    /// consumption, and therefore the only one a pressure/urgency reading
    /// may be computed from.
    ActiveTask,
    /// The ceiling the daemon's running policy would give the next task.
    /// Context, not consumption: nothing has been spent against it,
    /// because it is not an account — it is a setting. A renderer must
    /// not derive a utilization band from it, and must not show it as a
    /// remaining amount.
    ConfiguredDefault,
}

/// The daemon's running resource ceiling (HORO-1709) — scope
/// [`BudgetScope::ConfiguredDefault`].
///
/// A ceiling and its unit, and nothing else. There is no `used`, no
/// `remaining` and no percentage here, and their absence is the type's
/// main assertion: a configured default has no consumption to report, so
/// a field that could carry one would eventually be filled in with a
/// task's figure.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ConfiguredBudget {
    /// `policy.resource.hard_ceiling` as the running daemon resolved it —
    /// the same value `initialize_task_budget` would copy into the next
    /// task's budget row, read from the policy actually in force rather
    /// than from disk. A daemon whose `config.json` has been edited since
    /// it started reports what it is running; the `profile` segment is
    /// what says the edit has not taken effect yet.
    pub ceiling: ResourceAmount,
}

/// What remains of a governed task's resource envelope (HORO-1634).
///
/// A *classification*, not a measurement: which of five economically
/// distinct situations the task is in, plus the one share that situation
/// implies. The amounts are a sibling field —
/// [`StatusResult::task_budget_amounts`] — and the split is on purpose.
/// This type is what a renderer branches on; a figure is what it then
/// prints. Fusing the two would mean every surface that wanted to know
/// "is this exhausted" had to first decide what a negative token count
/// meant.
///
/// # On amounts
///
/// Until HORO-1709 this doc said "a ratio, never an amount", on the
/// ground that a share answers "how much room is left" without
/// disclosing what the room is measured in or how much was bought. That
/// reasoning was wrong about who it protected. The figures in question
/// are the user's own ceiling and the user's own consumption, on the
/// user's own machine, and withholding them did not make the statusline
/// safer — it made it unactionable, because "38% left" of an unstated
/// quantity cannot tell anyone whether to keep going. So amounts are now
/// published, in the envelope's own unit, beside this classification.
///
/// What remains withheld is unchanged and is a different thing: the
/// estimator's resource and cost quantiles, its free-text reasoning, and
/// any prompt or tool content. Those describe *predictions about the
/// user's behaviour* derived across tasks, not the envelope in force now.
/// `statusline_provider::explain_text` still says so, and its tests
/// still assert it.
///
/// Five variants, because the four ways there can be no reportable share
/// are different facts and collapsing them loses the one a user could act
/// on: an exhausted budget is not an unadmitted task, an envelope nothing
/// has ever drawn against is neither, and none of the three is a ledger
/// that would not read.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BudgetPosture {
    /// Capacity remains. `fraction_left` is the share of the task's hard
    /// limit that is neither settled nor held by an active reservation,
    /// in `0.0..=1.0` and strictly greater than zero — a share that had
    /// reached zero would be [`BudgetPosture::Exhausted`] instead.
    ///
    /// The protected Completion Reserve is *not* subtracted: it is
    /// earmarked capacity, not spent capacity, and required completion
    /// work may still draw against it (see
    /// `libra_governor_domain::ReservationClass::RequiredWork`). A share
    /// that excluded it would under-report what the task actually has.
    Remaining { fraction_left: f64 },
    /// An envelope exists, but nothing has ever been committed against it:
    /// the task holds no reservation in any state, so settled spend and
    /// active holds are both structurally zero.
    ///
    /// This is the one case that may *not* be reported as
    /// [`BudgetPosture::Remaining`], and it is why this variant exists
    /// (HORO-1708). The canonical share is
    /// `(hard_limit - settled - active) / hard_limit`, which for a task
    /// that never drew against its envelope is `hard_limit / hard_limit`
    /// — exactly `1.0`, rendered as "100% budget left". That number is
    /// arithmetically correct and still a false statement: it reads as
    /// "measured, and all of it is available", when what happened is that
    /// no economic event was ever attributed to this task at all. Absence
    /// of observation is not observation of fullness.
    ///
    /// It is reachable in normal operation, not only in theory. A plan
    /// whose admission came back `Deny` or `ApprovalRequired` gets no
    /// plan-level reservation by design, so every task in that state
    /// reports a full envelope for its entire life — and because a
    /// successfully admitted task has its work envelope reserved before
    /// any `Status` can be answered, a share of exactly `1.0` in practice
    /// *means* this case rather than a lucky rounding.
    ///
    /// Distinct from the three below: the envelope is readable, present
    /// and unspent. The missing thing is a draw against it, which is a
    /// fact about attribution rather than about capacity — so it is
    /// reported as the unknown it is, and the share is withheld rather
    /// than fabricated.
    Uncommitted,
    /// Nothing remains: settled spend plus active reservations have
    /// reached or passed the hard limit, so the ledger will refuse the
    /// next reservation.
    Exhausted,
    /// The task has no budget row, so there is no envelope to report a
    /// share of. Distinct from `Exhausted`: nothing has been spent, the
    /// task was never admitted to a budget in the first place. Distinct
    /// from `Uncommitted` in the other direction: there the envelope
    /// exists and nothing drew against it, here there is no envelope to
    /// draw against.
    NotEstablished,
    /// The ledger could not be read. Carried as a fact rather than
    /// dropped to `None`, because a budget whose state is unknown and a
    /// task that has no budget are different things to be told.
    Unreadable,
}

impl BudgetPosture {
    /// Classifies one [`BudgetSnapshot`] (HORO-1709).
    ///
    /// Pure, and that is the point. Before this the daemon derived the
    /// posture with four separate ledger reads of its own; publishing
    /// amounts alongside it would have made that six, against a 200 ms
    /// statusline budget, with every read free to observe a different
    /// instant. Now the ledger reads once and this decides what the
    /// single value means, so the share below and the amounts beside it
    /// are the same measurement seen twice rather than two measurements
    /// hoped to agree.
    ///
    /// [`BudgetPosture::NotEstablished`] is not reachable from here: a
    /// task with no budget row has no snapshot at all, so the absence is
    /// the caller's `None` and never a classification of something.
    ///
    /// # Why the branches are in this order
    ///
    /// Exhaustion first. It is the only posture that changes what the
    /// user should do next, and it is a fact about the ledger's own
    /// refusal threshold rather than about the share — an exhausted
    /// envelope that happens to have had no reservation attributed to it
    /// is still exhausted.
    ///
    /// Observation second. An envelope nothing has ever drawn against has
    /// `settled == 0` and `active == 0`, so the canonical share evaluates
    /// to exactly `1.0` and renders as "100% budget left" (HORO-1708).
    /// That is arithmetically correct and still false: it reads as
    /// measured-and-full when what happened is that no economic event was
    /// ever attributed to this task. Taken from
    /// [`BudgetSnapshot::is_observed`] — a reservation count — rather than
    /// from the share being `1.0`, because a float equality standing in
    /// for "nothing happened" would start lying the first time a
    /// settlement rounded back to the limit.
    pub fn from_snapshot(snapshot: &BudgetSnapshot) -> Self {
        if snapshot.remaining().is_exhausted() {
            return BudgetPosture::Exhausted;
        }
        if !snapshot.is_observed() {
            return BudgetPosture::Uncommitted;
        }
        match snapshot.fraction_left() {
            Some(fraction_left) => BudgetPosture::Remaining { fraction_left },
            // Unreachable on any row this workspace writes, and mapped
            // deliberately rather than unwrapped. `fraction_left` is
            // `None` only for a non-positive or non-finite hard limit;
            // non-positive was already caught as exhaustion above, so
            // what is left is a limit that is NaN or infinite — a budget
            // row whose own ceiling is not a quantity. "Unreadable" is
            // the truthful reading of that: the envelope's state is
            // unknown. Fabricating a share from it, or reporting it as an
            // envelope that was never established, would both claim to
            // know which.
            None => BudgetPosture::Unreadable,
        }
    }
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
    /// Regime-aware confidence/drift reporting (HORO-1671) — see
    /// `libra_governor_estimator::regime` module docs.
    pub regime: RegimeCalibrationReport,
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

/// Everything recorded from a successful `RecordOutcome` push (HORO-1174,
/// corrected by ADR-0017/HORO-1727).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutcomeRecordedResult {
    pub attested: ExecutionOutcome,
    /// `true` if this attestation was treated as authoritative. As of
    /// ADR-0017, always `false` on this deployment — see
    /// `libra_governor_domain::outcome_attestation::AttestationSource`.
    pub authoritative: bool,
    /// The contract revision this attestation is bound to, resolved
    /// from the request's `plan_id` — `None` when `plan_id` was `None`
    /// (deliberately unbound; never promotes; never counts toward any
    /// revision's conflict resolution — see ADR-0017).
    pub contract_revision: Option<u32>,
    /// `true` if this push also promoted `receipts.outcome_json` for
    /// every receipt at `contract_revision` — see
    /// `libra_governor_ledger::LedgerStore::record_outcome_attestation`.
    /// `false` when the attestation was recorded but no receipt existed
    /// yet to promote, when `authoritative` was `false`, when
    /// `contract_revision` was `None`, or when the authoritative claims
    /// for that revision don't yet agree on one terminal outcome.
    pub receipt_updated: bool,
}

/// The outcome of a `RecordOutcome` request (HORO-1174, extended by
/// ADR-0017/HORO-1727). Tagged `"state"`, mirroring
/// [`FinalizeOutcome`]'s own discipline for the exact same reason — see
/// that type's docs on the tag-collision bug this avoids.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum OutcomeRecordedOutcome {
    /// `task_id` has no `tasks` row at all.
    NoSuchTask,
    /// `plan_id` was `Some(..)` but does not name a real plan belonging
    /// to `task_id` (ADR-0017).
    NoSuchPlan,
    /// This exact `(task_id, source_id, idempotency_key)` was already
    /// recorded with the same outcome kind and evidence — a safe no-op
    /// replay, not an error.
    Duplicate,
    /// This exact `(task_id, source_id, idempotency_key)` was already
    /// recorded with *different* content (ADR-0017) — refused, nothing
    /// written. A genuine correction needs its own, different
    /// `idempotency_key`.
    IdempotencyKeyReused,
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
    ExecutionOwner(Box<crate::ExecutionOwnerOutcome>),
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
                provider: Some("claude-code".to_string()),
                transcript_path: None,
            };
            let json = serde_json::to_string(&request).unwrap();
            let round_tripped: Request = serde_json::from_str(&json).unwrap();
            assert_eq!(request, round_tripped);
        }
    }

    /// A host that exposes a transcript and one that does not must both
    /// survive the round trip — Codex has no `transcript_path`, so `None`
    /// is a normal value on this wire, not a degraded one (HORO-1725).
    #[test]
    fn finalize_request_round_trips_with_and_without_a_transcript_path() {
        for transcript_path in [None, Some("/tmp/horo-1725/session.jsonl".to_string())] {
            let request = Request::Finalize {
                session_id: "sess-1".to_string(),
                model: None,
                provider: Some("claude-code".to_string()),
                transcript_path,
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
                regime: libra_governor_estimator::build_report(
                    &[],
                    &libra_governor_domain::RegimeKey::builder()
                        .feature_schema("fs-v1")
                        .build(),
                    libra_governor_domain::BucketTier::Global,
                ),
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
            signed_claim: None,
        };
        let json = serde_json::to_string(&request).unwrap();
        let round_tripped: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(request, round_tripped);
    }

    #[test]
    fn record_outcome_request_with_a_signed_claim_round_trips() {
        let request = Request::RecordOutcome {
            task_id: TaskId::new(),
            plan_id: Some(PlanId::new()),
            source_id: "example-provider".to_string(),
            idempotency_key: "ci-run-42".to_string(),
            outcome: libra_governor_domain::ExecutionOutcome::Completed {
                evidence: vec!["https://ci.example.com/runs/42".to_string()],
            },
            signed_claim: Some(SignedOutcomeClaimWire {
                issued_at: 1_700_000_000,
                signature: "v1=deadbeef".to_string(),
            }),
        };
        let json = serde_json::to_string(&request).unwrap();
        let round_tripped: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(request, round_tripped);
    }

    #[test]
    fn outcome_recorded_outcome_round_trips_every_variant() {
        for outcome in [
            OutcomeRecordedOutcome::NoSuchTask,
            OutcomeRecordedOutcome::NoSuchPlan,
            OutcomeRecordedOutcome::Duplicate,
            OutcomeRecordedOutcome::IdempotencyKeyReused,
            OutcomeRecordedOutcome::Recorded(Box::new(OutcomeRecordedResult {
                attested: libra_governor_domain::ExecutionOutcome::Completed { evidence: vec![] },
                authoritative: false,
                contract_revision: Some(1),
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

    // --- HORO-1709: budget amounts, scopes, and posture classification ---

    use libra_governor_domain::ResourceKind;

    /// 150,000 tokens of envelope, 18,000 settled, 70,000 held, two
    /// reservation rows: a live mid-task state.
    fn observed_snapshot() -> BudgetSnapshot {
        BudgetSnapshot::new(
            ResourceKind::Tokens,
            150_000.0,
            30_000.0,
            18_000.0,
            70_000.0,
            2,
        )
    }

    /// The HORO-1708 case: an envelope exists, nothing was ever
    /// attributed to it.
    fn unobserved_snapshot() -> BudgetSnapshot {
        BudgetSnapshot::new(ResourceKind::Tokens, 150_000.0, 30_000.0, 0.0, 0.0, 0)
    }

    #[test]
    fn a_posture_and_its_amounts_describe_the_same_envelope() {
        let snapshot = observed_snapshot();
        let BudgetPosture::Remaining { fraction_left } = BudgetPosture::from_snapshot(&snapshot)
        else {
            panic!("a partly-spent observed envelope has remaining capacity");
        };
        // The claim this pins is HORO-1709 AC 5: the share and the
        // amounts cannot contradict each other. Not "they happen to
        // agree" — the share *is* the amounts, so reconstructing one
        // from the other is exact.
        let remaining = snapshot.remaining().value;
        let total = snapshot.total().as_f64();
        assert!(
            (fraction_left - remaining / total).abs() < f64::EPSILON,
            "share {fraction_left} must be remaining {remaining} over total {total}"
        );
    }

    /// HORO-1708's defect, re-pinned at the layer that now owns the
    /// branch. A full envelope nothing drew against must not read as a
    /// measured 100%.
    #[test]
    fn an_unobserved_envelope_is_uncommitted_rather_than_fully_remaining() {
        assert_eq!(
            BudgetPosture::from_snapshot(&unobserved_snapshot()),
            BudgetPosture::Uncommitted
        );
    }

    /// Exhaustion is tested before observation, so an envelope that is
    /// over its limit reports the fact that matters even if no
    /// reservation row survived to explain it.
    #[test]
    fn exhaustion_outranks_the_absence_of_an_observation() {
        let spent_out =
            BudgetSnapshot::new(ResourceKind::Tokens, 150_000.0, 0.0, 150_000.0, 0.0, 0);
        assert_eq!(
            BudgetPosture::from_snapshot(&spent_out),
            BudgetPosture::Exhausted
        );
    }

    #[test]
    fn an_overrun_is_exhausted_not_a_negative_share() {
        let overrun = BudgetSnapshot::new(ResourceKind::Tokens, 100.0, 0.0, 130.0, 0.0, 3);
        assert_eq!(
            BudgetPosture::from_snapshot(&overrun),
            BudgetPosture::Exhausted
        );
    }

    /// A zero-ceiling envelope has no room by construction. It must not
    /// become a share of zero-over-zero, and it must not be reported as
    /// an envelope that was never established — the row exists.
    #[test]
    fn a_zero_ceiling_envelope_is_exhausted_rather_than_a_nonsense_share() {
        let zero = BudgetSnapshot::new(ResourceKind::Tokens, 0.0, 0.0, 0.0, 0.0, 0);
        assert_eq!(
            BudgetPosture::from_snapshot(&zero),
            BudgetPosture::Exhausted
        );
    }

    /// The protected Completion Reserve is earmarked, not spent:
    /// required completion work may still draw against it, so a share
    /// that subtracted it would under-report what the task has.
    #[test]
    fn the_completion_reserve_does_not_reduce_the_reported_share() {
        let with_reserve = BudgetSnapshot::new(ResourceKind::Tokens, 1_000.0, 400.0, 100.0, 0.0, 1);
        let without_reserve =
            BudgetSnapshot::new(ResourceKind::Tokens, 1_000.0, 0.0, 100.0, 0.0, 1);
        assert_eq!(
            BudgetPosture::from_snapshot(&with_reserve),
            BudgetPosture::from_snapshot(&without_reserve)
        );
    }

    /// Every live envelope on this machine is denominated in tokens, not
    /// currency (HORO-1725/HORO-1727). The amounts must come back in the
    /// unit the ledger actually stores, never coerced to a default one.
    ///
    /// The fixture's share is deliberately `0.5` — exactly representable
    /// — because a *share* does not survive this wire bit-for-bit. See
    /// `a_transported_share_can_shift_by_one_ulp_but_the_amounts_cannot`.
    #[test]
    fn amounts_keep_the_envelopes_own_unit_across_the_wire() {
        let half_spent =
            BudgetSnapshot::new(ResourceKind::Tokens, 160_000.0, 0.0, 20_000.0, 60_000.0, 2);
        let envelope = ResponseEnvelope {
            protocol_version: PROTOCOL_VERSION,
            response: Response::Status(Box::new(StatusResult {
                scope: crate::StatusScope::HostLatestObservation,
                current_task: None,
                task_budget: Some(BudgetPosture::from_snapshot(&half_spent)),
                task_budget_amounts: Some(half_spent),
                configured_budget: Some(ConfiguredBudget {
                    ceiling: ResourceAmount::Tokens(150_000),
                }),
            })),
        };
        let json = serde_json::to_string(&envelope).unwrap();
        let round_tripped: ResponseEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(envelope, round_tripped);

        let value = serde_json::to_value(&envelope).unwrap();
        assert_eq!(value["response"]["task_budget_amounts"]["kind"], "tokens");
        assert_eq!(
            value["response"]["configured_budget"]["ceiling"]["kind"],
            "tokens"
        );
    }

    /// A finding worth a test rather than a comment: this wire does not
    /// preserve an arbitrary `f64` bit-for-bit. `serde_json`'s default
    /// parser is a fast approximate one — exact round-tripping is behind
    /// its `float_roundtrip` feature, which this workspace does not
    /// enable — so a share like `62_000 / 150_000` can come back one ULP
    /// away from what was sent.
    ///
    /// Harmless for the amounts, which are integral: a token count, a
    /// ceiling and a settled sum are whole numbers whose `f64`
    /// representation is exact and whose shortest decimal form parses
    /// back exactly. Not harmless for a share, and that is the point of
    /// this test. A displayed percentage is `floor(share * 100)`, so a
    /// share sitting exactly on an integer boundary could render one
    /// point lower after transport than the amounts beside it imply —
    /// which is HORO-1709's AC 5 violated by the transport rather than
    /// by any arithmetic.
    ///
    /// The fix lives in the provider, and this test is what justifies it:
    /// the displayed percentage is recomputed from the transported
    /// *amounts* (exact) rather than read from the transported *share*
    /// (approximate). `BudgetPosture` keeps carrying `fraction_left`
    /// because it is the classification's own datum and the only
    /// available one when a peer sends no snapshot.
    #[test]
    fn a_transported_share_can_shift_by_one_ulp_but_the_amounts_cannot() {
        let snapshot = observed_snapshot();
        let sent = BudgetPosture::from_snapshot(&snapshot);
        let round_tripped: BudgetPosture =
            serde_json::from_str(&serde_json::to_string(&sent).unwrap()).unwrap();

        let (
            BudgetPosture::Remaining {
                fraction_left: before,
            },
            BudgetPosture::Remaining {
                fraction_left: after,
            },
        ) = (sent, round_tripped)
        else {
            panic!("the fixture is a partly-spent observed envelope");
        };
        assert!(
            (before - after).abs() <= f64::EPSILON,
            "a share must survive transport to within an ULP: {before} vs {after}"
        );

        // The amounts, by contrast, are integral and must be exact — this
        // is what makes recomputing the percentage from them sound.
        let amounts: BudgetSnapshot =
            serde_json::from_str(&serde_json::to_string(&snapshot).unwrap()).unwrap();
        assert_eq!(amounts, snapshot);
        assert_eq!(amounts.total().as_f64(), 150_000.0);
        assert_eq!(amounts.remaining().value, 62_000.0);
    }

    /// Optional budget fields still default to None within the current
    /// scoped status shape. The required envelope version rejects omitted
    /// versions before a daemon can reinterpret that absence.
    #[test]
    fn a_missing_budget_field_decodes_to_none_so_the_version_gate_is_the_protection() {
        let older_budget_shape =
            r#"{"scope":"host_latest_observation","current_task":null,"task_budget":null}"#;
        let decoded: StatusResult = serde_json::from_str(older_budget_shape)
            .expect("an Option field absent from the payload is not a decode error");
        assert_eq!(decoded.task_budget_amounts, None);
        assert_eq!(decoded.configured_budget, None);

        // And the envelope that would actually carry it does fail, which
        // is the behaviour the bump buys.
        let skewed = r#"{"request":{"kind":"status"}}"#;
        assert!(serde_json::from_str::<RequestEnvelope>(skewed).is_err());
    }

    /// The two scopes are distinguishable on the wire. A renderer that
    /// could not tell them apart would answer "how much do I have left"
    /// with a configured setting (HORO-1709's Idle refinement).
    #[test]
    fn the_two_budget_scopes_round_trip_distinguishably() {
        for (scope, expected) in [
            (BudgetScope::ActiveTask, "\"active_task\""),
            (BudgetScope::ConfiguredDefault, "\"configured_default\""),
        ] {
            let json = serde_json::to_string(&scope).unwrap();
            assert_eq!(json, expected);
            let round_tripped: BudgetScope = serde_json::from_str(&json).unwrap();
            assert_eq!(scope, round_tripped);
        }
    }

    /// An idle daemon carries a configured ceiling and no active-task
    /// figures at all. Nothing in this shape can be mistaken for
    /// consumption: there is no posture, no snapshot, and
    /// [`ConfiguredBudget`] has no field that could hold a spend.
    #[test]
    fn an_idle_status_carries_a_configured_ceiling_and_no_task_amounts() {
        let idle = StatusResult {
            scope: crate::StatusScope::HostLatestObservation,
            current_task: None,
            task_budget: None,
            task_budget_amounts: None,
            configured_budget: Some(ConfiguredBudget {
                ceiling: ResourceAmount::Tokens(150_000),
            }),
        };
        let value = serde_json::to_value(&idle).unwrap();
        assert!(value["task_budget"].is_null());
        assert!(value["task_budget_amounts"].is_null());
        assert_eq!(value["configured_budget"]["ceiling"]["amount"], 150_000);
        let configured = value["configured_budget"].as_object().unwrap();
        assert_eq!(
            configured.len(),
            1,
            "a configured ceiling has exactly one field: the ceiling. A \
             `used` or `remaining` here would eventually be filled in \
             with an active task's figure."
        );
    }
}
