//! The daemon's Unix-socket accept loop and per-request dispatch.
//!
//! # Concurrency model
//!
//! Single-threaded, serial accept loop: the daemon handles one
//! connection fully (read request, do the work, write response, close)
//! before accepting the next. MVP 1.0's expected request rate — one
//! `Preflight` per submitted prompt, one `Status` per statusline refresh
//! — never approaches a level where serial handling is a bottleneck, and
//! staying single-threaded avoids sharing `rusqlite::Connection` (not
//! `Sync`) across threads, which the ledger crate's own docs call out as
//! "one `LedgerStore` per thread/process." A future ticket can move to a
//! thread-per-connection model with a connection pool if request volume
//! ever justifies the added complexity.
//!
//! # Spawn-if-absent / stale-socket detection
//!
//! [`bind_or_detect_running`] implements the race-safe "am I the only
//! daemon" check: bind the socket path first. `AddrInUse` does not by
//! itself mean a live daemon owns it — a prior daemon that crashed
//! leaves its socket file behind. So on `AddrInUse` this process tries
//! to *connect* to the path: a successful connect proves a peer is
//! actually listening (this process loses the race, exits cleanly); a
//! failed connect proves the file is stale (unlinked, and bind retried
//! exactly once). Binding is never preceded by an unconditional unlink —
//! that would let a slow-starting live daemon's socket be deleted out
//! from under it.

use std::io::BufReader;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;

use libra_governor_domain::{
    completion_reserve_for, evaluate_hysteresis, possible_tool_loop, tool_call_count_is_material,
    Admission, CompletionContract, CompletionCriterion, Estimate, ExecutionOutcome, ExecutionPlan,
    ExecutionReceipt, HysteresisOutcome, Policy, PolicyPresetInputs, RemainingEstimate, ReplanId,
    ReplanReason, ReplanRecord, ReplanTriggerKind, ReservationClass, ReservationState,
    ResourceAmount, ABSOLUTE_TOOL_CALL_COUNT_FALLBACK, DEFAULT_LOOP_STREAK_THRESHOLD,
};
use libra_governor_estimator::{
    admission_replay, duration_coverage, estimate_bucketed, typical_tool_call_count_bucketed,
    AdmissionPolicy,
};
use libra_governor_ledger::{LedgerStore, ReserveOutcome, ReserveRequest};
use libra_governor_protocol::{
    wire, AdmissionPolicyReport, CalibrationReportResult, FinalizeOutcome, FinalizeResult,
    GatewayStatusResult, PreflightResult, ReconSummary, ReplanState, Request, RequestEnvelope,
    Response, ResponseEnvelope, StatusResult, TaskSummary, PROTOCOL_VERSION,
};

use crate::{contract, features, gateway_authority, log, recon, recon::ReconBudget};

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("ledger error: {0}")]
    Ledger(#[from] libra_governor_ledger::LedgerError),
    #[error("wire error: {0}")]
    Wire(#[from] wire::WireError),
    #[error("another daemon is already running at {0}")]
    AlreadyRunning(PathBuf),
    /// Only reachable if a `Policy`/projected-amount pairing is
    /// internally inconsistent (e.g. a hand-built `Policy` that bypassed
    /// [`Policy::validated`]) — every `Policy` this daemon constructs
    /// itself goes through validation, so this is a defensive path, not
    /// an expected one (HORO-1141).
    #[error("policy evaluation error: {0}")]
    Policy(#[from] libra_governor_domain::PolicyEvaluationError),
}

pub struct DaemonConfig {
    pub socket_path: PathBuf,
    pub ledger_path: PathBuf,
    pub log_path: PathBuf,
    pub recon_budget: ReconBudget,
    /// Cooldown and max-auto-replan-count controls (HORO-1139) — see
    /// `libra_governor_domain::ReplanHysteresisConfig` docs.
    pub replan_hysteresis: libra_governor_domain::ReplanHysteresisConfig,
    /// The admission [`Policy`] every task's [`TaskBudget`][libra_governor_domain::TaskBudget]
    /// is initialized from at its first `Preflight` (HORO-1141 — the
    /// first real wiring of HORO-1137's `Policy::evaluate` into the
    /// daemon). See [`default_admission_policy`] for the shipped
    /// default.
    pub policy: Policy,
    /// How long a reservation may stay `Active` before startup/opportunistic
    /// reconciliation reclaims it (HORO-1141) — see
    /// [`reconcile_stale_reservations`].
    pub reservation_ttl_secs: u64,
    /// The optional enforcement gateway (HORO-1144). `None` — the
    /// default — means no gateway: the daemon serves hooks and the
    /// statusline exactly as before, and every spend gate stays advisory.
    ///
    /// Runtime-gated rather than feature-gated on purpose. A Cargo
    /// feature on a security-critical path produces a configuration that
    /// is never compiled in CI and therefore never tested; as an
    /// `Option`, the enforcement code is compiled, linted, and tested on
    /// every build whether or not a given user turns it on. See ADR 0003
    /// §1.
    pub gateway: Option<libra_governor_gateway::config::GatewayConfig>,
    /// Live counters for a running gateway, shared with it so
    /// `GatewayStatus` can render them. Present even when no gateway is
    /// configured, in which case every counter stays zero.
    pub gateway_stats: std::sync::Arc<libra_governor_gateway::stats::GatewayStats>,
    /// Header naming the agent session a gateway request belongs to. See
    /// `libra_governor_gateway::proxy::DEFAULT_SESSION_HEADER` and the
    /// known limitation in `integrations/claude-code/README.md`.
    pub gateway_session_header: String,
}

/// The default admission [`Policy`] every task's budget is initialized
/// from when no per-task policy override exists yet (no such override
/// surface exists as of HORO-1141 — every task uses this one policy).
/// `resource_target` is denominated in [`libra_governor_domain::ResourceAmount::Tokens`]:
/// the only resource figure this system can honestly report today is a
/// token count (Claude Code's hook payloads expose no cost/USD data —
/// see `ExecutionReceipt::provider` docs), so pricing every task budget
/// in USD would fabricate a conversion nothing here can back up.
///
/// Constructed from fixed, known-valid inputs, so the `Policy::balanced`
/// validation this calls can never actually fail in practice — `.expect`
/// documents that invariant rather than propagating a `Result` that has
/// no real failure mode for a caller to handle.
pub fn default_admission_policy() -> Policy {
    Policy::balanced(PolicyPresetInputs {
        resource_target: ResourceAmount::Tokens(100_000),
        time_target_secs: 3600,
        quality_floor: CompletionContract::first(vec![CompletionCriterion::required(
            "required verification (tests/build/lint) passes",
        )]),
    })
    .expect("default_admission_policy's hardcoded inputs are always valid")
}

/// Binds `socket_path`, handling the stale-vs-live detection documented
/// on this module. Returns the bound listener, or
/// [`DaemonError::AlreadyRunning`] if a live daemon already holds it.
pub fn bind_or_detect_running(socket_path: &PathBuf) -> Result<UnixListener, DaemonError> {
    match UnixListener::bind(socket_path) {
        Ok(listener) => Ok(listener),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            if UnixStream::connect(socket_path).is_ok() {
                return Err(DaemonError::AlreadyRunning(socket_path.clone()));
            }
            // Stale socket file with no listener behind it: remove and
            // retry exactly once. If a third process wins this narrow
            // window, its bind succeeds and ours below fails loudly
            // rather than silently — acceptable for MVP 1.0's
            // single-user local scope (documented known limitation).
            std::fs::remove_file(socket_path)?;
            Ok(UnixListener::bind(socket_path)?)
        }
        Err(e) => Err(e.into()),
    }
}

/// Reclaims every reservation left `Active` past its `expires_at`
/// (HORO-1141) — the crash/restart reconciliation path: a reservation
/// issued by a process (a hook invocation, or this same daemon) that
/// then crashed without settling or releasing it would otherwise stay
/// `Active`, and its drawn Completion Reserve unrestored, forever.
/// Called once at daemon startup (before the accept loop begins — see
/// [`serve`]) and again opportunistically at the top of every
/// `Preflight` (see `handle_preflight`) so a long-lived daemon process
/// stays honest without a dedicated timer thread. Errors are logged, not
/// propagated — reconciliation failing must never take the whole daemon
/// down.
pub(crate) fn reconcile_stale_reservations(ledger: &mut LedgerStore, config: &DaemonConfig) {
    let now = time::OffsetDateTime::now_utc();
    match ledger.expire_stale_reservations(now) {
        Ok(expired) if !expired.is_empty() => {
            let restored: f64 = expired.iter().map(|r| r.drawn_from_reserve.as_f64()).sum();
            log::append_line(
                &config.log_path,
                &format!(
                    "startup reconciliation: expired {} stale reservation(s), restored {restored} \
                     to their completion reserve(s)",
                    expired.len()
                ),
            );
        }
        Ok(_) => {}
        Err(e) => {
            log::append_line(&config.log_path, &format!("reconciliation error: {e}"));
        }
    }
}

/// Runs the daemon's blocking accept loop against an already-bound
/// listener. Returns only on an unrecoverable I/O error accepting a new
/// connection; per-connection errors are caught and logged, never
/// propagated (one bad request must not take the daemon down).
pub fn serve(listener: UnixListener, config: &DaemonConfig) -> Result<(), DaemonError> {
    let mut ledger = LedgerStore::open(&config.ledger_path)?;
    let mut current_task: Option<TaskSummary> = None;

    reconcile_stale_reservations(&mut ledger, config);

    // Started AFTER reconciliation so the gateway's first request sees a
    // budget from which crashed predecessors' capacity has already been
    // reclaimed. The handle is kept alive for the life of `serve`; when
    // it drops, the gateway's shutdown channel closes and its thread
    // winds down.
    let _gateway = start_gateway(config);

    for incoming in listener.incoming() {
        let stream = match incoming {
            Ok(stream) => stream,
            Err(e) => {
                log::append_line(&config.log_path, &format!("accept error: {e}"));
                continue;
            }
        };
        if let Err(e) = handle_connection(stream, &mut ledger, &mut current_task, config) {
            log::append_line(&config.log_path, &format!("connection error: {e}"));
        }
    }
    Ok(())
}

/// Handles exactly one request/response exchange on an already-accepted
/// connection: read, dispatch, write, close. `pub` (rather than crate-
/// private) so integration tests can drive the real dispatch logic
/// against a hand-rolled single-connection server without duplicating
/// it — see `crates/daemon/tests/preflight_integration.rs`.
pub fn handle_connection(
    stream: UnixStream,
    ledger: &mut LedgerStore,
    current_task: &mut Option<TaskSummary>,
    config: &DaemonConfig,
) -> Result<(), DaemonError> {
    let reader = BufReader::new(stream.try_clone()?);
    let envelope: Result<RequestEnvelope, _> = wire::read_message(reader);

    let response = match envelope {
        Ok(envelope) if envelope.protocol_version != PROTOCOL_VERSION => Response::Error {
            message: format!(
                "protocol version mismatch: daemon speaks {PROTOCOL_VERSION}, client sent {}",
                envelope.protocol_version
            ),
        },
        Ok(envelope) => dispatch(envelope.request, ledger, current_task, config),
        Err(e) => {
            log::append_line(&config.log_path, &format!("malformed request: {e}"));
            Response::Error {
                message: "malformed request".to_string(),
            }
        }
    };

    let out = ResponseEnvelope {
        protocol_version: PROTOCOL_VERSION,
        response,
    };
    wire::write_message(&stream, &out)?;
    Ok(())
}

fn dispatch(
    request: Request,
    ledger: &mut LedgerStore,
    current_task: &mut Option<TaskSummary>,
    config: &DaemonConfig,
) -> Response {
    match request {
        Request::Preflight {
            task_hint,
            cwd,
            session_id,
        } => match handle_preflight(&task_hint, &cwd, &session_id, ledger, config) {
            Ok(result) => {
                // A fresh preflight resolves to the SAME task on a second
                // (or later) prompt within a session (docs/adr/0002), and
                // that task's `replan_state` in the ledger may already
                // carry replans/escalation from earlier in the session.
                // Seed the displayed state from the ledger rather than
                // hardcoding `Stable` — otherwise a task that has already
                // exhausted its auto-replan budget would misleadingly
                // show "stable" again until its next material event.
                let replan_state =
                    replan_state_for_summary(ledger, result.task_id, &config.replan_hysteresis);
                *current_task = Some(TaskSummary {
                    task_id: result.task_id,
                    confidence: result.confidence,
                    recon_cost_seconds: result.recon_cost_seconds,
                    plan_id: result.plan_id,
                    remaining_estimate: result
                        .estimate
                        .clone()
                        .unwrap_or_else(Estimate::cold_start),
                    replan_state,
                });
                Response::Preflight(Box::new(result))
            }
            Err(e) => {
                log::append_line(&config.log_path, &format!("preflight error: {e}"));
                Response::Error {
                    message: "internal error handling preflight request".to_string(),
                }
            }
        },
        Request::Status => Response::Status(Box::new(StatusResult {
            current_task: current_task.clone(),
        })),
        Request::ToolInvoked {
            session_id,
            tool_name,
        } => match handle_tool_invoked(&session_id, &tool_name, ledger, current_task, config) {
            Ok(()) => Response::Ack,
            Err(e) => {
                log::append_line(&config.log_path, &format!("tool_invoked error: {e}"));
                Response::Error {
                    message: "internal error recording tool invocation".to_string(),
                }
            }
        },
        Request::Finalize { session_id, model } => {
            match handle_finalize(&session_id, model, ledger, current_task) {
                Ok(outcome) => Response::Finalize(outcome),
                Err(e) => {
                    log::append_line(&config.log_path, &format!("finalize error: {e}"));
                    Response::Error {
                        message: "internal error finalizing task".to_string(),
                    }
                }
            }
        }
        Request::GatewayStatus => Response::GatewayStatus(Box::new(gateway_status(config))),
        Request::CalibrationReport => match handle_calibration_report(ledger, config) {
            Ok(result) => Response::CalibrationReport(Box::new(result)),
            Err(e) => {
                log::append_line(&config.log_path, &format!("calibration_report error: {e}"));
                Response::Error {
                    message: "internal error computing calibration report".to_string(),
                }
            }
        },
    }
}

/// The default [`AdmissionPolicy`]s `calibration report` replays against
/// local history — a short, medium wall-clock deadline at the same
/// threshold quantile (P80) the estimator itself would be checked
/// against for an admission decision. Not tunable yet (no `Request`
/// field for it): a fixed, documented default is more useful for a
/// pre-launch product's first real evidence than a configuration surface
/// nobody has asked for.
const DEFAULT_ADMISSION_POLICIES: [AdmissionPolicy; 2] = [
    AdmissionPolicy {
        deadline_secs: 300,
        threshold_quantile: 0.80,
    },
    AdmissionPolicy {
        deadline_secs: 600,
        threshold_quantile: 0.80,
    },
];

/// Computes real duration-coverage and admission-replay calibration
/// metrics (HORO-1132) over every locally recorded receipt paired back to
/// its originating estimate. Per this repo's own architecture rule (see
/// `ARCHITECTURE.md`), this computation lives in the daemon, not the CLI
/// reaching into the ledger directly.
fn handle_calibration_report(
    ledger: &LedgerStore,
    config: &DaemonConfig,
) -> Result<CalibrationReportResult, DaemonError> {
    let (pairs, dropped_rows) = ledger.calibration_pairs()?;
    if dropped_rows > 0 {
        log::append_line(
            &config.log_path,
            &format!(
                "calibration_report: dropped {dropped_rows} receipt row(s) with no usable \
                 estimate (estimate-less plan or cold-start estimate)"
            ),
        );
    }

    let coverage = duration_coverage(&pairs);
    let admission = DEFAULT_ADMISSION_POLICIES
        .into_iter()
        .map(|policy| AdmissionPolicyReport {
            policy,
            outcome: admission_replay(&pairs, policy),
        })
        .collect();

    Ok(CalibrationReportResult {
        coverage,
        admission,
        dropped_rows,
    })
}

/// Maps `task_id`'s persisted [`libra_governor_domain::ReplanHysteresisState`]
/// onto the statusline-facing [`ReplanState`] (HORO-1139): a task that has
/// already reached `max_auto_replans` displays as escalated, one with a
/// nonzero `auto_replan_count` displays its count, otherwise `Stable`.
/// Used both by the `Preflight` dispatch arm (so a second prompt in the
/// same session shows a task's real replan history, not a hardcoded
/// `Stable`) and available for any future caller building a `TaskSummary`
/// from scratch. A lookup failure degrades to `Stable` rather than
/// failing the whole preflight over what is purely a display concern.
fn replan_state_for_summary(
    ledger: &LedgerStore,
    task_id: libra_governor_domain::TaskId,
    hysteresis_config: &libra_governor_domain::ReplanHysteresisConfig,
) -> ReplanState {
    let state = match ledger.replan_state_for_task(task_id) {
        Ok(state) => state,
        Err(_) => return ReplanState::Stable,
    };
    if state.auto_replan_count >= hysteresis_config.max_auto_replans {
        ReplanState::EscalatedAwaitingApproval
    } else if state.auto_replan_count > 0 {
        ReplanState::Replanned {
            count: state.auto_replan_count,
        }
    } else {
        ReplanState::Stable
    }
}

fn handle_preflight(
    task_hint: &str,
    cwd: &std::path::Path,
    session_id: &str,
    ledger: &mut LedgerStore,
    config: &DaemonConfig,
) -> Result<PreflightResult, DaemonError> {
    let now = time::OffsetDateTime::now_utc();

    // Opportunistic reconciliation (HORO-1141): a long-lived daemon does
    // this once at startup (see `serve`) too, but re-running it here
    // means a reservation left dangling by a crashed subagent gets
    // reclaimed before the very next preflight needs its capacity, not
    // only after the next daemon restart.
    reconcile_stale_reservations(ledger, config);

    let task_id = ledger.resolve_or_create_task_for_session(session_id, now)?;
    let previous_contract = ledger.latest_contract(task_id)?;
    let had_existing_budget = ledger.task_budget(task_id)?.is_some();
    // Captured before `supersede_in_flight_preflights` below so the
    // outgoing plan's reservation (if this is a second-or-later prompt
    // in the same session, not a replan) can be released rather than
    // left `active` and untouched until its TTL eventually expires
    // (HORO-1141).
    let previous_plan_id = ledger.in_flight_plan_for_session(session_id)?;

    let recon = recon::run_recon(cwd, task_hint, &config.recon_budget);
    let contract = contract::draft_contract(previous_contract.as_ref(), &recon);
    ledger.insert_contract(task_id, &contract, now)?;

    // Estimator input (HORO-1130): real task-class bucketing over
    // preflight-knowable features. `model` is unavailable at preflight
    // time (see `features::derive_task_features` docs) so it is always
    // `None` here — the RepoTopologyModel tier of the ladder simply never
    // matches on it until a future preflight payload exposes the model
    // up front.
    let task_features = features::derive_task_features(&recon, task_hint, cwd, None);
    let history = ledger.receipts_for_estimation()?;
    let estimate = libra_governor_estimator::estimate_bucketed(&history, &task_features);

    let plan = libra_governor_domain::ExecutionPlan::new(task_id, contract.revision, None, now)
        .with_estimate(estimate.clone())
        .with_task_features(Some(task_features));
    ledger.insert_plan(&plan)?;

    // Supersede any prior in-flight preflight for this session before
    // recording the new one, so at most one is ever in_flight at a time
    // — this is what keeps a re-invoked hook (new prompt, or a user
    // cancelling mid-session) from leaving orphaned "active" state.
    ledger.supersede_in_flight_preflights(session_id)?;
    ledger.record_preflight(session_id, task_id, plan.id, now)?;

    // Release the outgoing plan's reservation, if any (HORO-1141): a
    // fresh (non-replan) preflight for a task that already had an
    // in-flight plan — e.g. a second prompt in the same session — must
    // not leave that plan's envelope `active` forever; only `ToolInvoked`'s
    // replan path and `Finalize`'s settlement otherwise touch it.
    if let Some(previous_plan_id) = previous_plan_id {
        ledger.release_active_for_plan(task_id, previous_plan_id, now)?;
    }

    // Completion Reserve + atomic admission (HORO-1141): the first real
    // wiring of HORO-1137's `Policy::evaluate` into the daemon. Every
    // task's budget is initialized (idempotently — a no-op if one
    // already exists) from `config.policy` and the reserve this
    // contract/estimate imply; a second-or-later preflight for the same
    // task (a new prompt in the same session, or a replan) recomputes
    // and adjusts the reserve against the freshest evidence.
    let reserve_estimate = completion_reserve_for(&contract, Some(&estimate), &config.policy);
    let budget = ledger.initialize_task_budget(task_id, &config.policy, &reserve_estimate, now)?;
    if had_existing_budget {
        match ledger.adjust_completion_reserve(
            task_id,
            reserve_estimate.amount,
            reserve_estimate.basis,
            now,
        )? {
            libra_governor_ledger::AdjustOutcome::Insufficient { available, .. } => {
                log::append_line(
                    &config.log_path,
                    &format!(
                        "task {task_id}: could not raise completion reserve to \
                         {reserve_estimate:?} — only {available:?} of headroom available"
                    ),
                );
            }
            libra_governor_ledger::AdjustOutcome::Adjusted { .. }
            | libra_governor_ledger::AdjustOutcome::NoBudget => {}
        }
    }

    // Project total committed capacity (settled + active + this
    // preflight's own requested envelope) and evaluate it against the
    // policy's Hard/Elastic/Approval resource and time constraints
    // (HORO-1137). `resource_p80` is preferred when its kind matches the
    // policy's; a cold-start (or kind-mismatched) estimate falls back to
    // the policy's own target — the same basis rule `completion_reserve_for`
    // uses, applied here to the admission projection.
    let requested = estimate
        .resource_p80
        .filter(|amount| amount.kind() == budget.resource_kind)
        .unwrap_or(config.policy.resource.target);
    let required_headroom = ledger.available(task_id, ReservationClass::RequiredWork)?;
    let committed = required_headroom
        .map(|h| budget.hard_limit.as_f64() - h.value)
        .unwrap_or(0.0);
    let projected =
        ResourceAmount::from_kind_f64(budget.resource_kind, committed + requested.as_f64());
    let decision = config.policy.evaluate(
        projected,
        estimate.duration_p80_secs.unwrap_or(0),
        estimate.confidence,
    )?;

    match &decision.admission {
        Admission::Admit => {
            // The reserve's own share of `requested` is already held on
            // `task_budgets.completion_reserve` — only the remainder
            // (the ordinary work envelope) needs an explicit reservation
            // row, so `work_envelope + completion_reserve == requested`
            // and nothing is double-counted.
            let work_envelope_value =
                (requested.as_f64() - reserve_estimate.amount.as_f64()).max(0.0);
            let work_envelope =
                ResourceAmount::from_kind_f64(budget.resource_kind, work_envelope_value);
            // When the gateway is enabled, per-request gateway
            // reservations REPLACE this plan-level envelope rather than
            // stacking with it (HORO-1144, ADR 0003 §4). Reserving both
            // would double-count the same spend against one `hard_limit`
            // and deny the task at roughly half its real budget.
            if config.gateway.is_some() {
                log::append_line(
                    &config.log_path,
                    &format!(
                        "task {task_id}: gateway enabled — per-request reservations replace the \
                         plan-level work envelope, none reserved here"
                    ),
                );
            } else {
                match ledger.reserve(ReserveRequest {
                    task_id,
                    session_id,
                    plan_id: Some(plan.id),
                    class: ReservationClass::RequiredWork,
                    amount: work_envelope,
                    idempotency_key: &format!("plan:{}", plan.id.0),
                    now,
                    ttl_secs: config.reservation_ttl_secs,
                })? {
                    ReserveOutcome::Granted(_) | ReserveOutcome::AlreadyGranted(_) => {}
                    ReserveOutcome::Insufficient { available, .. } => {
                        log::append_line(
                            &config.log_path,
                            &format!(
                                "task {task_id}: policy admitted but the ledger could not reserve \
                             the work envelope — only {available:?} available"
                            ),
                        );
                    }
                    ReserveOutcome::NoBudget => {
                        log::append_line(
                            &config.log_path,
                            &format!("task {task_id}: reserve attempted with no task_budgets row"),
                        );
                    }
                }
            }
        }
        Admission::ApprovalRequired(_) | Admission::Deny(_) => {
            // Optional work is denied/approval-gated before it can
            // consume protected completion resources (HORO-1141
            // acceptance criterion): no reservation is written here at
            // all, so nothing beyond what earlier reservations already
            // hold is committed against this task's envelope.
            log::append_line(
                &config.log_path,
                &format!(
                    "task {task_id}: admission decision {:?} — no reservation made",
                    decision.admission
                ),
            );
        }
    }

    let completion_reserve = ledger.task_budget(task_id)?.map(|b| b.completion_reserve);

    Ok(PreflightResult {
        task_id,
        contract_draft: contract,
        recon_summary: ReconSummary {
            files_scanned: recon.files_scanned,
            dirs_scanned: recon.dirs_scanned,
            likely_affected_paths: recon.likely_affected_paths,
            detected_test_commands: recon.detected_test_commands,
            truncated: recon.truncated,
            reason: recon.reason,
        },
        confidence: recon.confidence,
        recon_cost_seconds: recon.elapsed.as_secs_f64(),
        estimate: Some(estimate),
        plan_id: plan.id,
        admission: Some(decision),
        completion_reserve,
    })
}

/// Handles a `ToolInvoked` notification (HORO-1139): records the tool
/// call (count + same-tool streak), then checks whether the resulting
/// evidence is a MATERIAL deviation from what the current plan's
/// estimate implied. If so, subject to hysteresis (cooldown / max
/// auto-replan count), recomputes the remaining-work estimate via the
/// deterministic tier and records a linked replan.
///
/// Deliberately does not call
/// [`libra_governor_domain::should_replan`]/[`libra_governor_domain::evaluate_replan_cost_against_policy`]
/// here: those are the general cost/benefit gate for a replan whose cost
/// is nontrivial (e.g. a future LLM-assisted tier). The deterministic
/// tier's own cost is a local SQLite re-estimate — negligible enough
/// that, once a material event clears hysteresis, it always clears
/// `should_replan` too. Both functions are still fully implemented and
/// tested at the domain level (see `crates/domain/src/replan.rs`) for a
/// caller (a future tier, or a manual replan command) whose cost is not
/// negligible.
///
/// Still fire-and-forget from the *client's* side
/// ([`crate::client::fire_and_forget`] is unaffected by anything here) —
/// this function runs entirely server-side against local SQLite state,
/// so it does not touch the deliberate PostToolUse latency contract
/// documented on `libra-governor-cli`'s `hook_post_tool_use` module.
fn handle_tool_invoked(
    session_id: &str,
    tool_name: &str,
    ledger: &mut LedgerStore,
    current_task: &mut Option<TaskSummary>,
    config: &DaemonConfig,
) -> Result<(), DaemonError> {
    let now = time::OffsetDateTime::now_utc();
    ledger.increment_tool_call_count(session_id)?;
    let same_tool_streak = ledger.record_tool_invocation(session_id, tool_name, now)?;

    let Some(task_id) = ledger.task_id_for_session(session_id)? else {
        return Ok(());
    };
    let Some(plan_id) = ledger.in_flight_plan_for_session(session_id)? else {
        return Ok(());
    };
    let Some(plan) = ledger.get_plan(plan_id)? else {
        return Ok(());
    };
    // No `TaskFeatures` recorded on the in-flight plan (a pre-MVP-2 plan,
    // impossible in a fresh deployment): nothing to bucket a remaining
    // estimate against. Honestly skip replanning rather than guessing —
    // matches this crate's `resource_quantiles` precedent of returning
    // "unavailable" over fabricating a number.
    let Some(task_features) = plan.task_features.clone() else {
        return Ok(());
    };

    let total_tool_calls = ledger.tool_call_count_for_session(session_id)?;
    let history = ledger.receipts_for_estimation()?;
    let typical_tool_calls = typical_tool_call_count_bucketed(&history, &task_features);

    // Hysteresis state is fetched up front (not just for the cooldown/
    // max-count check below) because `tool_call_count_at_last_replan` is
    // also the re-baseline for material-event detection: `total_tool_calls`
    // is the session's raw *cumulative* count, which never goes back down,
    // so comparing it directly against `typical` would keep flagging every
    // tool call as "material" for the rest of the session once the first
    // deviation fires. `calls_since_last_replan` measures fresh evidence
    // since the plan was last adjusted — see
    // `libra_governor_domain::ReplanHysteresisState` docs.
    let hysteresis_state = ledger.replan_state_for_task(task_id)?;
    let calls_since_last_replan =
        total_tool_calls.saturating_sub(hysteresis_state.tool_call_count_at_last_replan);

    let loop_signal = possible_tool_loop(same_tool_streak, DEFAULT_LOOP_STREAK_THRESHOLD);
    let count_signal = tool_call_count_is_material(calls_since_last_replan, typical_tool_calls);
    if !loop_signal && !count_signal {
        return Ok(());
    }

    let (trigger, detail) = if loop_signal {
        (
            ReplanTriggerKind::PossibleToolLoop,
            format!("{tool_name} invoked {same_tool_streak} times in a row"),
        )
    } else {
        let typical_desc = typical_tool_calls
            .map(|t| t.to_string())
            .unwrap_or_else(|| {
                format!("unknown (absolute fallback {ABSOLUTE_TOOL_CALL_COUNT_FALLBACK})")
            });
        (
            ReplanTriggerKind::ToolCallCountExceeded,
            format!(
                "{calls_since_last_replan} tool calls since the last replan vs. typical {typical_desc}"
            ),
        )
    };

    match evaluate_hysteresis(&config.replan_hysteresis, &hysteresis_state, now) {
        HysteresisOutcome::SuppressedCooldown => {
            log::append_line(
                &config.log_path,
                &format!(
                    "replan suppressed by cooldown for task {task_id}: {trigger:?} ({detail})"
                ),
            );
            return Ok(());
        }
        HysteresisOutcome::EscalateApprovalNeeded => {
            log::append_line(
                &config.log_path,
                &format!(
                    "material event for task {task_id} escalated (auto-replan budget \
                     exhausted): {trigger:?} ({detail})"
                ),
            );
            if let Some(summary) = current_task.as_mut() {
                if summary.task_id == task_id {
                    summary.replan_state = ReplanState::EscalatedAwaitingApproval;
                }
            }
            return Ok(());
        }
        HysteresisOutcome::Allow => {}
    }

    let base_estimate = estimate_bucketed(&history, &task_features);
    let remaining = RemainingEstimate::from_bucketed(base_estimate);
    let reason = ReplanReason::new(trigger, detail);

    let new_plan = ExecutionPlan::new(
        task_id,
        plan.contract_revision,
        plan.recon_snapshot_ref.clone(),
        now,
    )
    .with_estimate(remaining.estimate.clone())
    .with_task_features(Some(task_features))
    .with_replan_linkage(plan.id, reason.clone());
    ledger.insert_plan(&new_plan)?;
    ledger.supersede_in_flight_preflights(session_id)?;
    ledger.record_preflight(session_id, task_id, new_plan.id, now)?;

    // Recompute and re-reserve against the replan's remaining estimate
    // (HORO-1141): the superseded plan's envelope is refunded, not
    // stranded `active` forever, and the Completion Reserve is
    // recalculated against fresh evidence rather than staying pinned to
    // the original (now-stale) preflight's estimate.
    ledger.release_active_for_plan(task_id, plan.id, now)?;
    if let Some(contract) = ledger.latest_contract(task_id)? {
        let reserve_estimate =
            completion_reserve_for(&contract, Some(&remaining.estimate), &config.policy);
        match ledger.adjust_completion_reserve(
            task_id,
            reserve_estimate.amount,
            reserve_estimate.basis,
            now,
        )? {
            libra_governor_ledger::AdjustOutcome::Insufficient { available, .. } => {
                log::append_line(
                    &config.log_path,
                    &format!(
                        "task {task_id}: replan could not raise completion reserve to \
                         {reserve_estimate:?} — only {available:?} of headroom available"
                    ),
                );
            }
            libra_governor_ledger::AdjustOutcome::Adjusted { .. }
            | libra_governor_ledger::AdjustOutcome::NoBudget => {}
        }

        if let Some(budget) = ledger.task_budget(task_id)? {
            let requested = remaining
                .estimate
                .resource_p80
                .filter(|amount| amount.kind() == budget.resource_kind)
                .unwrap_or(config.policy.resource.target);
            let work_envelope_value =
                (requested.as_f64() - reserve_estimate.amount.as_f64()).max(0.0);
            let work_envelope =
                ResourceAmount::from_kind_f64(budget.resource_kind, work_envelope_value);
            // Same mutual exclusion as `handle_preflight`'s: with the
            // gateway on, the per-request reservations are the envelope.
            if config.gateway.is_some() {
                log::append_line(
                    &config.log_path,
                    &format!(
                        "task {task_id}: gateway enabled — replan reserves no plan-level work \
                         envelope; per-request gateway reservations carry it"
                    ),
                );
            } else {
                match ledger.reserve(ReserveRequest {
                    task_id,
                    session_id,
                    plan_id: Some(new_plan.id),
                    class: ReservationClass::RequiredWork,
                    amount: work_envelope,
                    idempotency_key: &format!("plan:{}", new_plan.id.0),
                    now,
                    ttl_secs: config.reservation_ttl_secs,
                })? {
                    ReserveOutcome::Granted(_) | ReserveOutcome::AlreadyGranted(_) => {}
                    ReserveOutcome::Insufficient { available, .. } => {
                        log::append_line(
                            &config.log_path,
                            &format!(
                                "task {task_id}: replan could not reserve the recomputed work \
                             envelope — only {available:?} available"
                            ),
                        );
                    }
                    ReserveOutcome::NoBudget => {
                        log::append_line(
                            &config.log_path,
                            &format!(
                                "task {task_id}: replan reserve attempted with no task_budgets row"
                            ),
                        );
                    }
                }
            }
        }
    }

    ledger.insert_replan_event(&ReplanRecord {
        id: ReplanId::new(),
        task_id,
        prior_plan_id: plan.id,
        new_plan_id: new_plan.id,
        reason: reason.clone(),
        remaining_estimate: remaining.clone(),
        created_at: now,
    })?;
    ledger.record_replan_for_task(task_id, now, total_tool_calls)?;
    // Also re-baseline the loop-streak signal: without this, the very
    // next repeated tool call would look like a continuation of the
    // already-replanned-on streak rather than fresh evidence.
    ledger.reset_tool_streak(session_id)?;
    let auto_replan_count = hysteresis_state.auto_replan_count + 1;

    log::append_line(
        &config.log_path,
        &format!(
            "replan #{auto_replan_count} for task {task_id}: {:?} ({}) — plan {} -> {} \
             (duration P90 {:?}s -> {:?}s, confidence {:?} -> {:?})",
            reason.trigger,
            reason.detail.as_deref().unwrap_or(""),
            plan.id.0,
            new_plan.id.0,
            plan.estimate.as_ref().and_then(|e| e.duration_p90_secs),
            remaining.estimate.duration_p90_secs,
            plan.estimate.as_ref().map(|e| e.confidence),
            remaining.estimate.confidence,
        ),
    );

    if let Some(summary) = current_task.as_mut() {
        if summary.task_id == task_id {
            summary.plan_id = new_plan.id;
            summary.confidence = remaining.estimate.confidence;
            summary.remaining_estimate = remaining.estimate;
            // Derived from the same mapping the `Preflight` dispatch arm
            // uses (`replan_state_for_summary`), not constructed inline
            // as `Replanned { count: auto_replan_count }` — at
            // `auto_replan_count == max_auto_replans` this just-landed
            // replan IS the one that exhausts the budget, so it must
            // render as escalated immediately rather than waiting for
            // the next Preflight to notice (which would otherwise
            // disagree with `evaluate_hysteresis`'s own `>=` check).
            summary.replan_state =
                replan_state_for_summary(ledger, task_id, &config.replan_hysteresis);
        }
    }

    Ok(())
}

/// Finalizes the task bound to `session_id`: computes elapsed duration
/// and gathers the tool-call count, builds an [`ExecutionReceipt`] with
/// `outcome: Unknown` (MVP 1.0 has no automated Completion Contract
/// verification — see crate docs), and persists it. A safe no-op
/// (`FinalizeOutcome::NoActiveTask`) when `session_id` has no task bound
/// to it at all (e.g. `Stop` fired with no preceding `Preflight`).
fn handle_finalize(
    session_id: &str,
    model: Option<String>,
    ledger: &mut LedgerStore,
    current_task: &mut Option<TaskSummary>,
) -> Result<FinalizeOutcome, DaemonError> {
    let Some(task_id) = ledger.task_id_for_session(session_id)? else {
        return Ok(FinalizeOutcome::NoActiveTask);
    };
    let Some(plan_id) = ledger.in_flight_plan_for_session(session_id)? else {
        return Ok(FinalizeOutcome::NoActiveTask);
    };
    let Some(plan) = ledger.get_plan(plan_id)? else {
        return Ok(FinalizeOutcome::NoActiveTask);
    };

    let now = time::OffsetDateTime::now_utc();
    let started_at = ledger.session_started_at(session_id)?.unwrap_or(now);
    let elapsed_secs = (now - started_at).whole_seconds().max(0) as u64;
    let tool_call_count = ledger.tool_call_count_for_session(session_id)?;

    // MVP 1.0 has no automated Completion Contract verification (no
    // test-running integration): outcome is always `Unknown` here,
    // structurally correct per `ExecutionOutcome`'s own docs — a task
    // must never be inferred "done" merely because the session stopped.
    // Actual resource usage is an honest empty `Vec`, not a zeroed USD
    // amount: Claude Code's `Stop`/`PostToolUse` hook payloads expose no
    // token/cost data (see PR description), so nothing here fabricates a
    // spend figure.
    let receipt = ExecutionReceipt::new(
        task_id,
        plan.contract_revision,
        plan_id,
        elapsed_secs,
        vec![],
        ExecutionOutcome::Unknown,
        now,
    )
    .with_tool_call_count(tool_call_count)
    .with_model(model)
    .with_provider(None)
    .with_task_features(plan.task_features.clone());

    // Settle every reservation still active on this plan (HORO-1141).
    // `None` as the actual cost: Claude Code's hook payloads expose no
    // token/cost usage figure (see the comment above on `actual_usage`),
    // so settlement conservatively falls back to the reserved amount
    // rather than fabricating one — see `Reservation::usage_known` docs.
    let active_reservations: Vec<_> = ledger
        .reservations_for_task(task_id)?
        .into_iter()
        .filter(|r| r.plan_id == Some(plan_id) && r.state == ReservationState::Active)
        .collect();
    for reservation in active_reservations {
        ledger.settle(reservation.id, None, now)?;
    }
    let reservation_evidence = ledger.reservation_evidence(task_id)?;
    let receipt = receipt.with_reservation_evidence(reservation_evidence);

    ledger.insert_receipt(&receipt)?;

    if current_task.as_ref().map(|t| t.task_id) == Some(task_id) {
        *current_task = None;
    }

    Ok(FinalizeOutcome::Finalized(Box::new(FinalizeResult {
        receipt,
        estimate: plan.estimate,
    })))
}

/// A running gateway's thread and its shutdown channel.
///
/// Dropping this closes the channel, which the gateway's own shutdown
/// relay observes, ending its accept loop. The thread is deliberately NOT
/// joined on drop: the daemon's `serve` loop only ends when the process
/// is going away anyway, and blocking process exit on an in-flight
/// streaming response would be worse than letting the OS reclaim it.
pub struct GatewayHandle {
    _shutdown: std::sync::mpsc::Sender<()>,
}

/// Starts the optional enforcement gateway, if one is configured
/// (HORO-1144).
///
/// Every failure path here logs and returns `None`, leaving the gateway
/// off and the daemon fully functional. That asymmetry is deliberate and
/// recorded in ADR 0003 §6: the gateway's own admission fails CLOSED
/// (anything it cannot enforce exactly, it refuses), but the daemon's
/// core function fails OPEN (a mistyped upstream host must not take away
/// preflight, estimation, and the ledger).
fn start_gateway(config: &DaemonConfig) -> Option<GatewayHandle> {
    let raw = config.gateway.clone()?;
    let policy_kind = config.policy.resource.target.kind();

    let validated = match libra_governor_gateway::config::validate(raw, policy_kind) {
        Ok(validated) => validated,
        Err(e) => {
            log::append_line(
                &config.log_path,
                &format!("gateway disabled: configuration rejected — {e}"),
            );
            return None;
        }
    };

    let ledger = match gateway_authority::open_gateway_ledger(&config.ledger_path) {
        Ok(ledger) => ledger,
        Err(e) => {
            log::append_line(
                &config.log_path,
                &format!("gateway disabled: could not open its own ledger connection — {e}"),
            );
            return None;
        }
    };

    let bind_addr = validated.bind_addr;
    let tier = validated.tier;
    let mut runtime_config = libra_governor_gateway::server::GatewayRuntimeConfig::new(
        validated,
        std::sync::Arc::new(gateway_authority::LedgerSpendAuthority::new(
            std::sync::Arc::clone(&ledger),
        )),
        std::sync::Arc::new(gateway_authority::LedgerRequestRecorder::new(
            ledger,
            config.log_path.clone(),
        )),
        std::sync::Arc::clone(&config.gateway_stats),
    );
    runtime_config.session_header = config.gateway_session_header.clone();

    // Resolving the credential and the capability token happens inside
    // `build_state`, eagerly — a gateway that started and only then
    // discovered it has no credential would already have told the user it
    // was protecting them. It is done here, on the daemon's own thread,
    // so the failure is reportable before anything starts listening.
    if let Err(e) = libra_governor_gateway::server::build_state(&runtime_config) {
        log::append_line(
            &config.log_path,
            &format!("gateway disabled: startup failed — {e}"),
        );
        return None;
    }

    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel();
    let log_path = config.log_path.clone();
    std::thread::Builder::new()
        .name("libra-gateway".to_string())
        .spawn(move || {
            if let Err(e) = libra_governor_gateway::server::run_gateway(runtime_config, shutdown_rx)
            {
                log::append_line(&log_path, &format!("gateway stopped: {e}"));
            }
        })
        .ok()?;

    log::append_line(
        &config.log_path,
        &format!("gateway listening on {bind_addr} at tier {tier:?}"),
    );
    Some(GatewayHandle {
        _shutdown: shutdown_tx,
    })
}

/// Answers a `GatewayStatus` request from state the daemon already holds.
///
/// Read-only by construction — there is no request variant that starts,
/// stops, or reconfigures the gateway, because a security boundary a
/// client can switch off over an IPC socket is not one.
fn gateway_status(config: &DaemonConfig) -> GatewayStatusResult {
    let snapshot = config.gateway_stats.snapshot();
    let policy_kind = config.policy.resource.target.kind();

    let (running, disabled_reason, capabilities, bind_addr) = match config.gateway.clone() {
        None => (
            false,
            Some("no gateway is configured (DaemonConfig.gateway is None)".to_string()),
            Some(libra_governor_domain::EnforcementCapabilities::for_tier(
                libra_governor_domain::EnforcementTier::HooksOnly,
                libra_governor_gateway::pricing::PRICING_VERSION,
            )),
            None,
        ),
        Some(raw) => match libra_governor_gateway::config::validate(raw, policy_kind) {
            Ok(validated) => (
                true,
                None,
                Some(libra_governor_domain::EnforcementCapabilities::for_tier(
                    validated.tier,
                    libra_governor_gateway::pricing::PRICING_VERSION,
                )),
                Some(validated.bind_addr.to_string()),
            ),
            // Re-validating rather than caching the startup result keeps
            // this honest after a config change that has not been
            // restarted into: the reported reason is the reason a restart
            // would hit.
            Err(e) => (false, Some(e.to_string()), None, None),
        },
    };

    GatewayStatusResult {
        running,
        disabled_reason,
        capabilities,
        bind_addr,
        forwarded: snapshot.forwarded,
        denied_budget: snapshot.denied_budget,
        denied_unenforceable: snapshot.denied_unenforceable,
        denied_unauthorized: snapshot.denied_unauthorized,
        approval_gated: snapshot.approval_gated,
        settled_with_known_usage: snapshot.settled_with_known_usage,
        settled_without_usage: snapshot.settled_without_usage,
        upstream_errors: snapshot.upstream_errors,
        bound_violations: snapshot.bound_violations,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_or_detect_running_succeeds_on_fresh_path() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("d.sock");
        let listener = bind_or_detect_running(&socket_path).unwrap();
        drop(listener);
    }

    #[test]
    fn bind_or_detect_running_detects_live_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("d.sock");
        let _first = bind_or_detect_running(&socket_path).unwrap();

        let result = bind_or_detect_running(&socket_path);
        assert!(matches!(result, Err(DaemonError::AlreadyRunning(_))));
    }

    #[test]
    fn bind_or_detect_running_recovers_stale_socket_file() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("d.sock");
        {
            // Bind and drop: on Unix this leaves the socket file behind
            // (drop does not unlink), simulating a crashed daemon.
            let _listener = bind_or_detect_running(&socket_path).unwrap();
        }
        assert!(socket_path.exists(), "test setup: stale file must remain");

        let listener = bind_or_detect_running(&socket_path);
        assert!(listener.is_ok(), "must recover from a stale socket file");
    }

    // The Preflight/Status dispatch path (handle_connection -> dispatch ->
    // handle_preflight) is exercised end to end, over a real socket, by
    // `crates/daemon/tests/preflight_integration.rs` via the public
    // `handle_connection` re-export — no need to duplicate that here.

    #[test]
    fn finalize_with_no_active_task_is_a_safe_no_op() {
        let mut ledger = LedgerStore::open_in_memory().unwrap();
        let mut current_task = None;
        let outcome =
            handle_finalize("no-such-session", None, &mut ledger, &mut current_task).unwrap();
        assert_eq!(outcome, FinalizeOutcome::NoActiveTask);
    }

    #[test]
    fn finalize_after_a_real_preflight_persists_a_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let config = DaemonConfig {
            socket_path: dir.path().join("d.sock"),
            ledger_path: dir.path().join("ledger.sqlite3"),
            log_path: dir.path().join("daemon.log"),
            recon_budget: crate::recon::ReconBudget::default(),
            replan_hysteresis: libra_governor_domain::ReplanHysteresisConfig::default(),
            policy: default_admission_policy(),
            reservation_ttl_secs: 900,
            gateway: None,
            gateway_stats: std::sync::Arc::new(Default::default()),
            gateway_session_header: libra_governor_gateway::proxy::DEFAULT_SESSION_HEADER
                .to_string(),
        };
        let mut ledger = LedgerStore::open(&config.ledger_path).unwrap();

        let preflight =
            handle_preflight("fix the bug", dir.path(), "sess-1", &mut ledger, &config).unwrap();
        assert!(
            preflight.estimate.is_some(),
            "preflight must always carry an estimate (cold-start counts)"
        );

        ledger.increment_tool_call_count("sess-1").unwrap();
        ledger.increment_tool_call_count("sess-1").unwrap();

        let mut current_task = Some(TaskSummary {
            task_id: preflight.task_id,
            confidence: preflight.confidence,
            recon_cost_seconds: preflight.recon_cost_seconds,
            plan_id: preflight.plan_id,
            remaining_estimate: preflight.estimate.clone().unwrap(),
            replan_state: ReplanState::Stable,
        });

        let outcome = handle_finalize(
            "sess-1",
            Some("claude-sonnet-5".to_string()),
            &mut ledger,
            &mut current_task,
        )
        .unwrap();
        let FinalizeOutcome::Finalized(result) = outcome else {
            panic!("expected a Finalized outcome after a real preflight");
        };
        assert_eq!(result.receipt.task_id, preflight.task_id);
        assert_eq!(result.receipt.tool_call_count, 2);
        assert_eq!(result.receipt.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(result.receipt.outcome, ExecutionOutcome::Unknown);
        assert!(result.estimate.is_some());
        assert!(
            current_task.is_none(),
            "finalizing the current task must clear statusline state"
        );

        // Machine-readable: the receipt is queryable back out of SQLite.
        let trajectory = ledger.task_trajectory(preflight.task_id).unwrap();
        assert_eq!(trajectory.receipts.len(), 1);
        assert_eq!(trajectory.receipts[0].tool_call_count, 2);
    }

    #[test]
    fn tool_invoked_dispatch_increments_the_session_counter() {
        let mut ledger = LedgerStore::open_in_memory().unwrap();
        let mut current_task = None;
        let dir = tempfile::tempdir().unwrap();
        let config = DaemonConfig {
            socket_path: dir.path().join("d.sock"),
            ledger_path: dir.path().join("ledger.sqlite3"),
            log_path: dir.path().join("daemon.log"),
            recon_budget: crate::recon::ReconBudget::default(),
            replan_hysteresis: libra_governor_domain::ReplanHysteresisConfig::default(),
            policy: default_admission_policy(),
            reservation_ttl_secs: 900,
            gateway: None,
            gateway_stats: std::sync::Arc::new(Default::default()),
            gateway_session_header: libra_governor_gateway::proxy::DEFAULT_SESSION_HEADER
                .to_string(),
        };

        let response = dispatch(
            Request::ToolInvoked {
                session_id: "sess-1".to_string(),
                tool_name: "Bash".to_string(),
            },
            &mut ledger,
            &mut current_task,
            &config,
        );
        assert_eq!(response, Response::Ack);
        assert_eq!(ledger.tool_call_count_for_session("sess-1").unwrap(), 1);
    }
}
