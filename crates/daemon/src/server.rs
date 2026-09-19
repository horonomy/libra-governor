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
use std::sync::OnceLock;

use libra_governor_domain::{
    apply_business_context, apply_external_approval, completion_reserve_for, evaluate_hysteresis,
    possible_tool_loop, tool_call_count_is_material, Admission, AttestationSource,
    BusinessContextSummary, CompletionContract, CompletionCriterion, Estimate, ExecutionOutcome,
    ExecutionPlan, ExecutionReceipt, ExternalApproval, ExternalVerdict, HysteresisOutcome, PlanId,
    Policy, PolicyPresetInputs, RemainingEstimate, ReplanId, ReplanReason, ReplanRecord,
    ReplanTriggerKind, ReservationClass, ReservationState, ResourceAmount, TaskId,
    ABSOLUTE_TOOL_CALL_COUNT_FALLBACK, DEFAULT_LOOP_STREAK_THRESHOLD,
};
use libra_governor_estimator::{
    admission_replay, duration_coverage, estimate_bucketed, typical_tool_call_count_bucketed,
    AdmissionPolicy,
};
use libra_governor_extension::{
    AdmissionEventData, ApprovalEventData, BusinessContextEventRef, BusinessContextRequest,
    EventEnvelope, EventKind, ExternalApprovalEventRef, OutcomeEventData, PolicyWebhookRequest,
    ProviderClient, ValidatedEventsConfig, ValidatedSurfaceConfig, WireVerdict,
    MAX_ADVISORY_CRITERIA, MAX_ADVISORY_CRITERION_CHARS,
};
use libra_governor_ledger::{
    BusinessContextInsert, LedgerStore, OutcomeAttestationInsert, ReserveOutcome, ReserveRequest,
};
use libra_governor_protocol::{
    wire, AdmissionPolicyReport, CalibrationReportResult, DoctorResult, FinalizeOutcome,
    FinalizeResult, GatewayStatusResult, OutcomeRecordedOutcome, OutcomeRecordedResult,
    PreflightResult, ReconSummary, ReplanState, Request, RequestEnvelope, Response,
    ResponseEnvelope, StatusResult, TaskSummary, PROTOCOL_VERSION,
};

use crate::{
    contract, extension_authority, features, gateway_authority, log, recon, recon::ReconBudget,
};

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
    /// Only reachable if serializing an already-in-memory, well-typed
    /// value (an `ExecutionOutcome` or a `Vec<String>` of evidence
    /// references) somehow fails — `serde_json` has no real failure mode
    /// for these types, so this is a defensive path (HORO-1174), same
    /// spirit as [`Self::Policy`] above.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
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
    /// The optional local extension points (HORO-1174): Business Context
    /// Provider, Policy Webhook, and signed event delivery. `None` — the
    /// default — means every extension surface is off. Raw/unvalidated,
    /// mirroring `gateway`'s field above; see [`ExtensionRuntime`] for
    /// the validated, cached-at-first-use form actually used at request
    /// time.
    pub extensions: Option<libra_governor_extension::ExtensionConfig>,
    /// Lazily built and cached on first access (by [`extension_runtime`])
    /// — validating an `[extensions]` block resolves each configured
    /// `secret_command` by spawning a subprocess, which must happen once
    /// at startup, never per-request. [`serve`] forces this exactly once,
    /// before the accept loop begins, by calling [`start_extensions`].
    pub extension_runtime: OnceLock<ExtensionRuntime>,
}

/// The validated, ready-to-use extension surfaces, built once by
/// [`extension_runtime`] and cached on [`DaemonConfig::extension_runtime`]
/// for the life of the daemon process. See that field's docs for why this
/// must not be re-validated per request.
pub struct ExtensionRuntime {
    pub business_context_provider: Option<ValidatedSurfaceConfig>,
    pub policy_webhook: Option<ValidatedSurfaceConfig>,
    pub events: Option<ValidatedEventsConfig>,
    /// `None` when `config.extensions` was `None` (nothing configured —
    /// not an error), or when the internal Tokio runtime could not be
    /// built (a real, if exceedingly rare, failure — logged and treated
    /// as "no extensions available", matching this daemon's fail-open
    /// doctrine).
    pub client: Option<ProviderClient>,
    /// The validation error, if `config.extensions` was `Some` but
    /// rejected. Cached, not re-derived — see
    /// [`libra_governor_protocol::DoctorResult::extension_config_error`]
    /// docs for why `Doctor` reports this cached value rather than
    /// re-running validation (which would re-spawn every configured
    /// secret command on every `doctor` call).
    pub config_error: Option<String>,
}

fn build_extension_runtime(config: &DaemonConfig) -> ExtensionRuntime {
    let Some(raw) = config.extensions.clone() else {
        return ExtensionRuntime {
            business_context_provider: None,
            policy_webhook: None,
            events: None,
            client: None,
            config_error: None,
        };
    };

    match libra_governor_extension::validate(raw) {
        Ok(validated) => {
            let client = match ProviderClient::new() {
                Ok(client) => Some(client),
                Err(e) => {
                    log::append_line(
                        &config.log_path,
                        &format!("extensions: could not build the provider client — {e}"),
                    );
                    None
                }
            };
            ExtensionRuntime {
                business_context_provider: validated.business_context_provider,
                policy_webhook: validated.policy_webhook,
                events: validated.events,
                client,
                config_error: None,
            }
        }
        Err(e) => {
            log::append_line(
                &config.log_path,
                &format!("extensions disabled: configuration rejected — {e}"),
            );
            ExtensionRuntime {
                business_context_provider: None,
                policy_webhook: None,
                events: None,
                client: None,
                config_error: Some(e.to_string()),
            }
        }
    }
}

/// The validated extension runtime, built and cached on first access. See
/// [`DaemonConfig::extension_runtime`] docs.
fn extension_runtime(config: &DaemonConfig) -> &ExtensionRuntime {
    config
        .extension_runtime
        .get_or_init(|| build_extension_runtime(config))
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
///
/// The socket file is hardened to owner-only (`0600`) immediately after
/// each successful `bind` (HORO-1146 security review finding #5) —
/// defense in depth on top of the containing state directory's own
/// `0700` mode (`libra_governor_daemon::paths::ensure_state_dir`), which
/// is the real boundary protecting the brief window between `bind`
/// creating the file and this function's own `chmod` running.
pub fn bind_or_detect_running(socket_path: &PathBuf) -> Result<UnixListener, DaemonError> {
    match UnixListener::bind(socket_path) {
        Ok(listener) => {
            harden_socket_permissions(socket_path)?;
            Ok(listener)
        }
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
            let listener = UnixListener::bind(socket_path)?;
            harden_socket_permissions(socket_path)?;
            Ok(listener)
        }
        Err(e) => Err(e.into()),
    }
}

fn harden_socket_permissions(socket_path: &PathBuf) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))
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
    // `let _gateway = ...`, NOT `let _ = ...`: an underscore-prefixed
    // binding holds the value for the scope, while a bare `_` pattern
    // drops it immediately — which would close the shutdown channel and
    // stop the gateway the instant it started.
    let _gateway = start_gateway(config);
    // Same `let _x = ...` (not `let _ = ...`) discipline as `_gateway`
    // above, for the same reason: an underscore-prefixed binding holds
    // the dispatcher's shutdown channel open for the scope, while a bare
    // `_` pattern would drop it immediately and stop the dispatcher the
    // instant it started.
    let _extension_dispatcher = start_extensions(config);

    for incoming in listener.incoming() {
        let stream = match incoming {
            Ok(stream) => stream,
            Err(e) => {
                log::append_line(&config.log_path, &format!("accept error: {e}"));
                continue;
            }
        };
        match handle_connection(stream, &mut ledger, &mut current_task, config) {
            Ok(should_shut_down) if should_shut_down => {
                log::append_line(
                    &config.log_path,
                    "shutting down: a client speaking a newer protocol version connected \
                     (this binary was upgraded while this daemon was still running) -- the \
                     next hook invocation's ensure_daemon_connection will spawn a fresh, \
                     up-to-date daemon against the same socket path",
                );
                break;
            }
            Ok(_) => {}
            Err(e) => {
                log::append_line(&config.log_path, &format!("connection error: {e}"));
            }
        }
    }
    Ok(())
}

/// Handles exactly one request/response exchange on an already-accepted
/// connection: read, dispatch, write, close. `pub` (rather than crate-
/// private) so integration tests can drive the real dispatch logic
/// against a hand-rolled single-connection server without duplicating
/// it — see `crates/daemon/tests/preflight_integration.rs`.
///
/// Returns `Ok(true)` exactly once: when the connecting client speaks a
/// *newer* protocol version than this daemon does. That can only happen
/// because this binary was upgraded while an old daemon process was
/// still running against the same socket (HORO-1169 dogfood finding —
/// v0.0.1 -> v0.0.2 with no daemon restart in between). Rather than
/// leaving that stale daemon serving `Error` responses forever (every
/// hook call fails open with "proceeding without governance" until a
/// human notices `doctor`'s warning and manually kills it), the caller
/// (`serve`) shuts the accept loop down after this one response so the
/// *next* hook invocation's `ensure_daemon_connection` finds no live
/// daemon, and spawns a fresh, current one automatically —
/// self-healing, using the exact same spawn-on-absence path a totally
/// fresh install already relies on. A client speaking an *older*
/// protocol than this daemon is a different, ambiguous situation (which
/// binary should give way is not obvious) and is left exactly as before:
/// a returned `Error`, no shutdown.
pub fn handle_connection(
    stream: UnixStream,
    ledger: &mut LedgerStore,
    current_task: &mut Option<TaskSummary>,
    config: &DaemonConfig,
) -> Result<bool, DaemonError> {
    let reader = BufReader::new(stream.try_clone()?);
    let envelope: Result<RequestEnvelope, _> = wire::read_message(reader);

    let mut shut_down_after_reply = false;
    let response = match envelope {
        Ok(envelope) if envelope.protocol_version != PROTOCOL_VERSION => {
            shut_down_after_reply = envelope.protocol_version > PROTOCOL_VERSION;
            Response::Error {
                message: format!(
                    "protocol version mismatch: daemon speaks {PROTOCOL_VERSION}, client sent {}",
                    envelope.protocol_version
                ),
            }
        }
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
    Ok(shut_down_after_reply)
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
            match handle_finalize(&session_id, model, ledger, current_task, config) {
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
        Request::Doctor => Response::Doctor(Box::new(handle_doctor(ledger, config))),
        Request::CalibrationReport => match handle_calibration_report(ledger, config) {
            Ok(result) => Response::CalibrationReport(Box::new(result)),
            Err(e) => {
                log::append_line(&config.log_path, &format!("calibration_report error: {e}"));
                Response::Error {
                    message: "internal error computing calibration report".to_string(),
                }
            }
        },
        Request::RecordOutcome {
            task_id,
            plan_id,
            source_id,
            idempotency_key,
            outcome,
        } => match handle_record_outcome(
            task_id,
            plan_id,
            &source_id,
            &idempotency_key,
            outcome,
            ledger,
            config,
        ) {
            Ok(result) => Response::OutcomeRecorded(result),
            Err(e) => {
                log::append_line(&config.log_path, &format!("record_outcome error: {e}"));
                Response::Error {
                    message: "internal error recording outcome".to_string(),
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

    // Business Context Provider fetch (HORO-1174). Deliberately BEFORE
    // the Completion Reserve/budget block below: `initialize_task_budget`/
    // `adjust_completion_reserve` must always use `config.policy` (the
    // durable, unnarrowed policy) — never `effective_policy` — so the
    // ordering here is not load-bearing, but placing the fetch first
    // keeps every use of `effective_policy` textually after the one
    // place it is computed. On any failure (not configured, fetch error,
    // malformed/oversized/wrong-schema-version response, or a deadline
    // that fails to narrow validly) this fails open: `effective_policy`
    // stays `config.policy.clone()` and `business_context_summary` stays
    // `None`.
    let extension_rt = extension_runtime(config);
    let mut effective_policy = config.policy.clone();
    let mut business_context_summary: Option<BusinessContextSummary> = None;
    if let (Some(surface), Some(client)) = (
        &extension_rt.business_context_provider,
        &extension_rt.client,
    ) {
        let repo_key = features::repo_key(cwd);
        let request = BusinessContextRequest::new(
            uuid::Uuid::new_v4().to_string(),
            task_id,
            session_id.to_string(),
            repo_key,
            cwd.to_path_buf(),
            now,
        );
        match client.fetch_business_context(surface, &request) {
            Ok(response) => {
                let advisory_criteria: Vec<String> = response
                    .advisory_criteria
                    .into_iter()
                    .take(MAX_ADVISORY_CRITERIA)
                    .map(|s| {
                        libra_governor_extension::truncate_chars(&s, MAX_ADVISORY_CRITERION_CHARS)
                    })
                    .collect();
                let (narrowed, applied) =
                    apply_business_context(&config.policy, response.deadline, now);
                if response.deadline.is_some() && !applied {
                    log::append_line(
                        &config.log_path,
                        &format!(
                            "task {task_id}: business context deadline could not be applied \
                             (past deadline or failed validation) — using the unnarrowed policy"
                        ),
                    );
                }
                effective_policy = narrowed;
                let summary = BusinessContextSummary {
                    provider_id: response.provider_id,
                    schema_version: response.schema_version,
                    priority: response.priority,
                    cost_center: response.cost_center,
                    deadline: response.deadline,
                    advisory_criteria,
                    external_refs: response.external_refs,
                    applied,
                    received_at: now,
                };
                let priority_str = summary.priority.map(priority_to_str);
                let advisory_json = serde_json::to_string(&summary.advisory_criteria).ok();
                let refs_json = serde_json::to_string(&summary.external_refs).ok();
                if let Err(e) = ledger.insert_business_context(BusinessContextInsert {
                    id: &uuid::Uuid::new_v4().to_string(),
                    task_id,
                    plan_id: Some(plan.id),
                    provider_id: &summary.provider_id,
                    schema_version: &summary.schema_version,
                    priority: priority_str,
                    deadline: summary.deadline,
                    cost_center: summary.cost_center.as_deref(),
                    advisory_criteria_json: advisory_json.as_deref(),
                    external_refs_json: refs_json.as_deref(),
                    applied: summary.applied,
                    received_at: summary.received_at,
                }) {
                    log::append_line(
                        &config.log_path,
                        &format!("task {task_id}: could not persist business context row: {e}"),
                    );
                }
                business_context_summary = Some(summary);
            }
            Err(e) => {
                log::append_line(
                    &config.log_path,
                    &format!(
                        "task {task_id}: business context fetch failed (fail-open, unnarrowed \
                         policy) — {e}"
                    ),
                );
            }
        }
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
    let projected_duration_secs = estimate.duration_p80_secs.unwrap_or(0);
    // `effective_policy`, not `config.policy` — the one deliberate lever
    // business context has on admission (HORO-1174): deadline narrowing
    // only. `initialize_task_budget`/`adjust_completion_reserve` above
    // still used `config.policy` unconditionally.
    let mut decision =
        effective_policy.evaluate(projected, projected_duration_secs, estimate.confidence)?;

    // Policy Webhook (HORO-1174): only ever called when the projected
    // admission is ApprovalRequired — a hard-ceiling Deny issues ZERO
    // webhook requests, and a clean Admit never asks. Fail-open on any
    // failure: admission stays ApprovalRequired, matching the pre-ticket
    // behavior.
    let mut external_approval_ref: Option<ExternalApprovalEventRef> = None;
    if let Admission::ApprovalRequired(ref approval_requests) = decision.admission {
        if let (Some(surface), Some(client)) = (&extension_rt.policy_webhook, &extension_rt.client)
        {
            let webhook_request = PolicyWebhookRequest::new(
                uuid::Uuid::new_v4().to_string(),
                task_id,
                plan.id,
                session_id.to_string(),
                effective_policy.name.clone(),
                effective_policy.policy_schema_version.clone(),
                approval_requests.clone(),
                projected,
                projected_duration_secs,
                estimate.confidence,
                now,
            );
            match client.request_policy_decision(surface, &webhook_request) {
                Ok(response) => {
                    let verdict = match response.verdict {
                        WireVerdict::Approve => ExternalVerdict::Approve,
                        WireVerdict::Reject => ExternalVerdict::Reject {
                            reason: libra_governor_extension::truncate_chars(
                                response.reason.as_deref().unwrap_or(""),
                                MAX_ADVISORY_CRITERION_CHARS,
                            ),
                        },
                        WireVerdict::Abstain => ExternalVerdict::Abstain,
                    };
                    external_approval_ref = Some(ExternalApprovalEventRef {
                        provider_id: response.provider_id.clone(),
                        verdict: verdict_to_str(&verdict).to_string(),
                    });
                    let approval = ExternalApproval {
                        provider_id: response.provider_id,
                        verdict,
                        decided_at: now,
                    };
                    decision = apply_external_approval(decision, &approval);
                }
                Err(e) => {
                    log::append_line(
                        &config.log_path,
                        &format!(
                            "task {task_id}: policy webhook call failed (fail-open, admission \
                             stays ApprovalRequired) — {e}"
                        ),
                    );
                }
            }
        }
    }

    // Persist the verdict onto the plan (HORO-1146): a later
    // material-event replan needs to be able to look up whether this
    // plan was ever actually admitted before it reserves capacity on the
    // plan's behalf — see `handle_tool_invoked`.
    ledger.set_plan_admission(plan.id, &decision.admission)?;

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

    // Event delivery (HORO-1174): enqueue `admission` (the FINAL
    // admission, post-webhook) always; enqueue `approval` only when the
    // final admission is still ApprovalRequired (a human must still
    // act). Both are inert no-ops when no `events` surface is
    // configured, or when `admission`/`approval` is not in its `kinds`
    // list — `enqueue_admission_and_approval_events` checks both.
    let business_context_ref = business_context_summary
        .as_ref()
        .map(|s| BusinessContextEventRef {
            provider_id: s.provider_id.clone(),
            applied: s.applied,
        });
    enqueue_admission_and_approval_events(
        extension_rt,
        ledger,
        task_id,
        plan.id,
        session_id,
        &decision,
        projected,
        projected_duration_secs,
        &effective_policy,
        business_context_ref,
        external_approval_ref,
        now,
        &config.log_path,
    );

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
        business_context: business_context_summary,
    })
}

/// Maps a [`libra_governor_domain::Priority`] onto its lowercase wire
/// string — the same string the `business_context.priority` ledger
/// column stores.
fn priority_to_str(priority: libra_governor_domain::Priority) -> &'static str {
    match priority {
        libra_governor_domain::Priority::Low => "low",
        libra_governor_domain::Priority::Normal => "normal",
        libra_governor_domain::Priority::High => "high",
        libra_governor_domain::Priority::Urgent => "urgent",
    }
}

/// Maps an [`ExternalVerdict`] onto its closed-set wire string, for the
/// `admission`/`approval` events' `external_approval.verdict` field.
fn verdict_to_str(verdict: &ExternalVerdict) -> &'static str {
    match verdict {
        ExternalVerdict::Approve => "approve",
        ExternalVerdict::Reject { .. } => "reject",
        ExternalVerdict::Abstain => "abstain",
    }
}

/// Builds and enqueues the `admission` event (always) and the `approval`
/// event (only when `decision.admission` is still `ApprovalRequired`
/// after any Policy Webhook resolution). Errors are logged and
/// swallowed — a failure to enqueue an event must never fail the
/// preflight itself.
#[allow(clippy::too_many_arguments)]
fn enqueue_admission_and_approval_events(
    extension_rt: &ExtensionRuntime,
    ledger: &mut LedgerStore,
    task_id: TaskId,
    plan_id: PlanId,
    session_id: &str,
    decision: &libra_governor_domain::PolicyDecision,
    projected_resource: ResourceAmount,
    projected_duration_secs: u64,
    policy: &Policy,
    business_context: Option<BusinessContextEventRef>,
    external_approval: Option<ExternalApprovalEventRef>,
    now: time::OffsetDateTime,
    log_path: &std::path::Path,
) {
    let Some(events) = &extension_rt.events else {
        return;
    };

    if events.kinds.contains(&EventKind::Admission) {
        let data = AdmissionEventData {
            task_id,
            plan_id,
            session_id: session_id.to_string(),
            admission: decision.admission.clone(),
            resource_outcome: decision.resource_outcome.clone(),
            time_outcome: decision.time_outcome.clone(),
            confidence_ok: decision.confidence_ok,
            projected_resource,
            projected_duration_secs,
            policy_name: policy.name.clone(),
            policy_schema_version: policy.policy_schema_version.clone(),
            business_context: business_context.clone(),
            external_approval: external_approval.clone(),
        };
        let envelope =
            EventEnvelope::new(EventKind::Admission, env!("CARGO_PKG_VERSION"), data, now);
        if let Ok(bytes) = envelope.to_json_bytes() {
            if let Err(e) = extension_authority::enqueue_event(
                ledger,
                envelope.event_id,
                EventKind::Admission,
                &format!("plan:{}", plan_id.0),
                Some(task_id),
                &bytes,
                now,
            ) {
                log::append_line(
                    log_path,
                    &format!("task {task_id}: could not enqueue admission event: {e}"),
                );
            }
        }
    }

    if events.kinds.contains(&EventKind::Approval) {
        if let Admission::ApprovalRequired(ref approval_requests) = decision.admission {
            let data = ApprovalEventData {
                task_id,
                plan_id,
                approval_requests: approval_requests.clone(),
                external_approval,
            };
            let envelope =
                EventEnvelope::new(EventKind::Approval, env!("CARGO_PKG_VERSION"), data, now);
            if let Ok(bytes) = envelope.to_json_bytes() {
                if let Err(e) = extension_authority::enqueue_event(
                    ledger,
                    envelope.event_id,
                    EventKind::Approval,
                    &format!("plan:{}", plan_id.0),
                    Some(task_id),
                    &bytes,
                    now,
                ) {
                    log::append_line(
                        log_path,
                        &format!("task {task_id}: could not enqueue approval event: {e}"),
                    );
                }
            }
        }
    }
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

    // HORO-1146 gate finding #2: the plan this replan is about to
    // supersede may itself have been Denied, or left ApprovalRequired,
    // at its own preflight — work still proceeds after either outcome
    // (hooks are advisory-only, ADR 0001), so a task reaching this
    // replan trigger with a non-admitted plan is expected, not a bug to
    // filter upstream. What must not happen is this replan silently
    // committing real ledger capacity (`ReservationClass::RequiredWork`)
    // on behalf of a task that was never actually authorized to spend
    // it — `handle_preflight` writes no reservation for either Deny or
    // ApprovalRequired, so the replan path must honor the same rule for
    // both, not just Deny. The prior plan's admission is carried forward
    // onto the new plan (rather than defaulting to `None`) so a second,
    // later replan in the same non-admitted chain also sees it and also
    // skips reserving, instead of the signal being lost the moment this
    // replan's own plan is superseded in turn.
    let was_not_admitted = matches!(
        plan.admission,
        Some(Admission::Deny(_)) | Some(Admission::ApprovalRequired(_))
    );
    let mut new_plan = ExecutionPlan::new(
        task_id,
        plan.contract_revision,
        plan.recon_snapshot_ref.clone(),
        now,
    )
    .with_estimate(remaining.estimate.clone())
    .with_task_features(Some(task_features))
    .with_replan_linkage(plan.id, reason.clone());
    if let Some(admission) = plan.admission.clone() {
        new_plan = new_plan.with_admission(admission);
    }
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
            if was_not_admitted {
                // HORO-1146 gate finding #2: the plan this replan
                // supersedes was Denied or left ApprovalRequired at its
                // own preflight — do not reserve capacity on behalf of a
                // task that was never actually admitted. The Completion
                // Reserve adjustment above still runs (it protects
                // required completion work regardless of admission
                // outcome); only this optional-work reservation is
                // skipped.
                log::append_line(
                    &config.log_path,
                    &format!(
                        "task {task_id}: replan skipped reserving the recomputed work envelope \
                         — the plan being replaced was not admitted (Deny or ApprovalRequired)"
                    ),
                );
            } else if config.gateway.is_some() {
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

    // Event delivery (HORO-1174): a `replan` event, whenever a replan
    // actually lands. Deliberately no HTTP or ledger-enqueue on the
    // `ToolInvoked` path itself — only here, after a replan has cleared
    // hysteresis and been recorded, keeping `handle_tool_invoked`'s cheap
    // common case (no material event) untouched.
    if let Some(events) = &extension_runtime(config).events {
        if events.kinds.contains(&EventKind::Replan) {
            let data = libra_governor_extension::ReplanEventData {
                task_id,
                prior_plan_id: plan.id,
                new_plan_id: new_plan.id,
                trigger: reason.trigger,
                detail: reason.detail.clone().unwrap_or_default(),
                auto_replan_count,
                remaining_duration_p80_secs: remaining.estimate.duration_p80_secs,
                remaining_confidence: remaining.estimate.confidence,
            };
            let envelope =
                EventEnvelope::new(EventKind::Replan, env!("CARGO_PKG_VERSION"), data, now);
            if let Ok(bytes) = envelope.to_json_bytes() {
                if let Err(e) = extension_authority::enqueue_event(
                    ledger,
                    envelope.event_id,
                    EventKind::Replan,
                    &format!("plan:{}", new_plan.id.0),
                    Some(task_id),
                    &bytes,
                    now,
                ) {
                    log::append_line(
                        &config.log_path,
                        &format!("task {task_id}: could not enqueue replan event: {e}"),
                    );
                }
            }
        }
    }

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
    config: &DaemonConfig,
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

    // Event delivery (HORO-1174): an `outcome` event reflecting the
    // receipt's outcome at finalize time. Source is `governor_local` —
    // this is the daemon's own finalize logic, not an external push.
    if let Some(events) = &extension_runtime(config).events {
        if events.kinds.contains(&EventKind::Outcome) {
            let data = OutcomeEventData {
                task_id,
                plan_id: Some(plan_id),
                outcome_kind: outcome_kind_str(&receipt.outcome).to_string(),
                evidence: receipt.outcome.evidence().to_vec(),
                source: "governor_local".to_string(),
                source_id: None,
                attested_at: now,
            };
            let envelope =
                EventEnvelope::new(EventKind::Outcome, env!("CARGO_PKG_VERSION"), data, now);
            if let Ok(bytes) = envelope.to_json_bytes() {
                if let Err(e) = extension_authority::enqueue_event(
                    ledger,
                    envelope.event_id,
                    EventKind::Outcome,
                    &format!("{task_id}:{}", plan_id.0),
                    Some(task_id),
                    &bytes,
                    now,
                ) {
                    log::append_line(
                        &config.log_path,
                        &format!("task {task_id}: could not enqueue outcome event: {e}"),
                    );
                }
            }
        }
    }

    if current_task.as_ref().map(|t| t.task_id) == Some(task_id) {
        *current_task = None;
    }

    Ok(FinalizeOutcome::Finalized(Box::new(FinalizeResult {
        receipt,
        estimate: plan.estimate,
    })))
}

/// The closed-set tag of an [`ExecutionOutcome`] — used for both the
/// `outcome` event's `outcome_kind` field and `outcome_attestations.outcome_kind`.
fn outcome_kind_str(outcome: &ExecutionOutcome) -> &'static str {
    match outcome {
        ExecutionOutcome::Completed { .. } => "completed",
        ExecutionOutcome::Failed { .. } => "failed",
        ExecutionOutcome::Aborted { .. } => "aborted",
        ExecutionOutcome::Unknown => "unknown",
    }
}

/// Handles a `RecordOutcome` push (HORO-1174): the one new *inbound*
/// path this ticket adds, over the daemon's existing Unix socket. Resolves
/// `task_id`, dedupes on `(task_id, source_id, idempotency_key)`, records
/// the attestation, promotes `receipts.outcome_json` when the source is
/// authoritative, enqueues an `outcome` event, and replies.
///
/// Every push through this `Request` variant is attributed
/// `AttestationSource::Provider { provider_id: source_id }` — this
/// specific inbound path exists for external Outcome Providers (see
/// `examples/local-providers/report_outcome.sh`); `GovernorLocal` is used
/// internally by `handle_finalize`, and no path yet exercises
/// `AttestationSource::Agent`.
fn handle_record_outcome(
    task_id: TaskId,
    plan_id: Option<PlanId>,
    source_id: &str,
    idempotency_key: &str,
    outcome: ExecutionOutcome,
    ledger: &mut LedgerStore,
    config: &DaemonConfig,
) -> Result<OutcomeRecordedOutcome, DaemonError> {
    let now = time::OffsetDateTime::now_utc();

    if !ledger.task_exists(task_id)? {
        return Ok(OutcomeRecordedOutcome::NoSuchTask);
    }

    let source = AttestationSource::Provider {
        provider_id: source_id.to_string(),
    };
    let authoritative = source.is_authoritative();
    let outcome_kind = outcome_kind_str(&outcome);
    let evidence_json = serde_json::to_string(outcome.evidence())?;

    let inserted = ledger.insert_outcome_attestation(OutcomeAttestationInsert {
        id: &uuid::Uuid::new_v4().to_string(),
        task_id,
        plan_id,
        source: "provider",
        source_id: Some(source_id),
        outcome_kind,
        evidence_json: &evidence_json,
        idempotency_key,
        authoritative,
        attested_at: now,
    })?;
    if !inserted {
        return Ok(OutcomeRecordedOutcome::Duplicate);
    }

    let receipt_updated = if authoritative {
        let outcome_json = serde_json::to_string(&outcome)?;
        ledger.promote_receipt_outcome(task_id, plan_id, &outcome_json)?
    } else {
        false
    };

    if let Some(events) = &extension_runtime(config).events {
        if events.kinds.contains(&EventKind::Outcome) {
            let data = OutcomeEventData {
                task_id,
                plan_id,
                outcome_kind: outcome_kind.to_string(),
                evidence: outcome.evidence().to_vec(),
                source: "provider".to_string(),
                source_id: Some(source_id.to_string()),
                attested_at: now,
            };
            let dedupe_key = match plan_id {
                Some(plan_id) => format!("{task_id}:{}", plan_id.0),
                None => format!("{task_id}:none:{idempotency_key}"),
            };
            let envelope =
                EventEnvelope::new(EventKind::Outcome, env!("CARGO_PKG_VERSION"), data, now);
            if let Ok(bytes) = envelope.to_json_bytes() {
                if let Err(e) = extension_authority::enqueue_event(
                    ledger,
                    envelope.event_id,
                    EventKind::Outcome,
                    &dedupe_key,
                    Some(task_id),
                    &bytes,
                    now,
                ) {
                    log::append_line(
                        &config.log_path,
                        &format!("task {task_id}: could not enqueue outcome event: {e}"),
                    );
                }
            }
        }
    }

    Ok(OutcomeRecordedOutcome::Recorded(Box::new(
        OutcomeRecordedResult {
            attested: outcome,
            receipt_updated,
        },
    )))
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

/// A running extension event dispatcher's thread and its shutdown
/// channel. Same drop semantics as [`GatewayHandle`].
pub struct ExtensionDispatcherHandle {
    _shutdown: std::sync::mpsc::Sender<()>,
}

/// Forces [`extension_runtime`]'s one-time validation (and therefore any
/// configured `secret_command` subprocess resolution) before the accept
/// loop begins, and starts the event-dispatcher thread if and only if
/// `extensions.events` is configured — see `crates/extension::dispatcher`
/// docs on why an absent `events` surface must not start a thread at
/// all.
fn start_extensions(config: &DaemonConfig) -> Option<ExtensionDispatcherHandle> {
    let runtime = extension_runtime(config);
    let events = runtime.events.clone()?;

    let ledger = match extension_authority::open_extension_ledger(&config.ledger_path) {
        Ok(ledger) => ledger,
        Err(e) => {
            log::append_line(
                &config.log_path,
                &format!(
                    "extension events disabled: could not open its own ledger connection — {e}"
                ),
            );
            return None;
        }
    };
    let queue: std::sync::Arc<dyn libra_governor_extension::DeliveryQueue> =
        std::sync::Arc::new(extension_authority::LedgerDeliveryQueue::new(ledger));

    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("libra-extension-dispatcher".to_string())
        .spawn(move || {
            libra_governor_extension::run_dispatcher(events, queue, shutdown_rx);
        })
        .ok()?;

    Some(ExtensionDispatcherHandle {
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

/// Answers a `Doctor` request (HORO-1150) from state the daemon already
/// holds, plus a fresh re-read of `config.json` (the same
/// re-validate-rather-than-trust-startup-cache pattern [`gateway_status`]
/// already uses, for the same "honest after an unrestarted config
/// change" reason). Every field is a version/count or a
/// presence/absence boolean — never a credential value, and never the
/// `credential_command`'s own captured stdout (this function never runs
/// it).
fn handle_doctor(ledger: &LedgerStore, config: &DaemonConfig) -> DoctorResult {
    let state_dir = config.socket_path.parent();
    let (config_file_present, config_file_valid, config_file_error, running_config_matches_disk) =
        match state_dir {
            Some(dir) => {
                let path = dir.join(crate::config_file::CONFIG_FILE_NAME);
                if !path.exists() {
                    (false, true, None, true)
                } else {
                    match crate::config_file::load_overrides(dir) {
                        Ok((policy_on_disk, gateway_on_disk, _extensions_on_disk)) => {
                            // A present, *valid* config.json can still
                            // disagree with what this already-running
                            // daemon holds in memory — the common "edited
                            // config.json, forgot to restart the daemon"
                            // case. Compare the two fields doctor already
                            // reports elsewhere (policy preset, whether a
                            // gateway is configured at all) rather than a
                            // full struct equality, since those are the
                            // only two things `load_overrides` can even
                            // change.
                            let policy_matches = policy_on_disk
                                .as_ref()
                                .map(|p| p.name == config.policy.name)
                                .unwrap_or(true);
                            let gateway_matches =
                                gateway_on_disk.is_some() == config.gateway.is_some();
                            (true, true, None, policy_matches && gateway_matches)
                        }
                        Err(e) => (true, false, Some(e.to_string()), true),
                    }
                }
            }
            // Only reachable if `socket_path` was hand-built with no parent
            // (e.g. a bare relative filename) — never true for a real daemon
            // started via `daemon_cmd::run`, which always joins onto
            // `paths::state_dir()`.
            None => (false, true, None, true),
        };

    let schema_version_applied = ledger.schema_version().unwrap_or(0);
    let schema_version_known = libra_governor_ledger::latest_known_version();

    let gw = gateway_status(config);
    // Narrowed to the one distinction that actually varies: whether the
    // daemon itself holds a credential (`GovernorHeld`) versus relaying
    // the agent's own subscription credential through unmodified
    // (`PassThroughSubscription`, where the daemon holds nothing). Any
    // configured `[gateway]` table implies *some* credential_mode, so
    // matching both variants here would just restate `gateway_configured`.
    let gateway_credential_configured = config.gateway.as_ref().is_some_and(|g| {
        matches!(
            g.credential_mode,
            libra_governor_gateway::config::GatewayCredentialMode::GovernorHeld { .. }
        )
    });

    // Presence checks against the raw `config.extensions` (mirrors
    // `gateway_configured` above — no validation, no subprocess exec).
    // `extension_config_error` is deliberately the CACHED startup
    // validation error (from `config.extension_runtime`, read via `get`,
    // never `get_or_init`) rather than a freshly recomputed one: unlike
    // `config_file_error`'s cheap re-parse, re-validating `[extensions]`
    // would re-spawn every configured `secret_command` on every `doctor`
    // call. `get()` returns `None` before `serve()`'s one-time
    // initialization has run (never true for a real daemon reached via
    // its socket — `Doctor` only dispatches after `serve` has already
    // called `start_extensions`).
    let extension_business_context_configured = config
        .extensions
        .as_ref()
        .is_some_and(|e| e.business_context_provider.is_some());
    let extension_policy_webhook_configured = config
        .extensions
        .as_ref()
        .is_some_and(|e| e.policy_webhook.is_some());
    let extension_events_configured = config
        .extensions
        .as_ref()
        .is_some_and(|e| e.events.is_some());
    let extension_events_pending = ledger.pending_delivery_count().unwrap_or(0);
    let extension_config_error = config
        .extension_runtime
        .get()
        .and_then(|rt| rt.config_error.clone());

    DoctorResult {
        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_version: PROTOCOL_VERSION,
        schema_version_applied,
        schema_version_known,
        schema_ahead_of_binary: schema_version_applied > schema_version_known,
        policy_preset: config.policy.name.clone(),
        config_file_present,
        config_file_valid,
        config_file_error,
        running_config_matches_disk,
        gateway_configured: config.gateway.is_some(),
        gateway_running: gw.running,
        gateway_disabled_reason: gw.disabled_reason,
        gateway_capabilities: gw.capabilities,
        gateway_credential_configured,
        telemetry_enabled: false,
        extension_business_context_configured,
        extension_policy_webhook_configured,
        extension_events_configured,
        extension_events_pending,
        extension_config_error,
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
        let dir = tempfile::tempdir().unwrap();
        let config = test_daemon_config(dir.path());
        let outcome = handle_finalize(
            "no-such-session",
            None,
            &mut ledger,
            &mut current_task,
            &config,
        )
        .unwrap();
        assert_eq!(outcome, FinalizeOutcome::NoActiveTask);
    }

    #[test]
    fn finalize_after_a_real_preflight_persists_a_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_daemon_config(dir.path());
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
            &config,
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
        let config = test_daemon_config(dir.path());

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

    /// Shared `DaemonConfig` builder for this module's tests — every
    /// extension surface off by default. `dir` must outlive the returned
    /// config's paths (caller keeps the `tempfile::TempDir` alive).
    fn test_daemon_config(dir: &std::path::Path) -> DaemonConfig {
        DaemonConfig {
            socket_path: dir.join("d.sock"),
            ledger_path: dir.join("ledger.sqlite3"),
            log_path: dir.join("daemon.log"),
            recon_budget: crate::recon::ReconBudget::default(),
            replan_hysteresis: libra_governor_domain::ReplanHysteresisConfig::default(),
            policy: default_admission_policy(),
            reservation_ttl_secs: 900,
            gateway: None,
            gateway_stats: std::sync::Arc::new(Default::default()),
            gateway_session_header: libra_governor_gateway::proxy::DEFAULT_SESSION_HEADER
                .to_string(),
            extensions: None,
            extension_runtime: OnceLock::new(),
        }
    }

    fn config_for_doctor_tests(dir: &std::path::Path) -> DaemonConfig {
        test_daemon_config(dir)
    }

    #[test]
    fn doctor_reports_healthy_state_with_no_config_file_and_matching_schema() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_for_doctor_tests(dir.path());
        let ledger = LedgerStore::open(&config.ledger_path).unwrap();

        let result = handle_doctor(&ledger, &config);

        assert_eq!(result.daemon_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(result.protocol_version, PROTOCOL_VERSION);
        assert!(!result.config_file_present);
        assert!(result.config_file_valid);
        assert!(result.config_file_error.is_none());
        assert!(result.running_config_matches_disk);
        assert_eq!(result.policy_preset, "balanced");
        assert!(!result.gateway_configured);
        assert!(!result.gateway_running);
        assert_eq!(
            result.schema_version_applied,
            libra_governor_ledger::latest_known_version()
        );
        assert!(!result.schema_ahead_of_binary);
        assert!(!result.telemetry_enabled);
    }

    #[test]
    fn doctor_flags_a_malformed_config_file_without_crashing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"), "{ not json").unwrap();
        let config = config_for_doctor_tests(dir.path());
        let ledger = LedgerStore::open(&config.ledger_path).unwrap();

        let result = handle_doctor(&ledger, &config);

        assert!(result.config_file_present);
        assert!(!result.config_file_valid);
        assert!(result.config_file_error.is_some());
        assert!(
            result.running_config_matches_disk,
            "an invalid file changes nothing about the running config, so there is no drift to report"
        );
    }

    #[test]
    fn doctor_flags_stale_config_when_disk_preset_disagrees_with_the_running_daemon() {
        let dir = tempfile::tempdir().unwrap();
        // The running config (built by config_for_doctor_tests) uses the
        // "balanced" preset; config.json on disk now names a different,
        // valid preset — the classic "edited the file, forgot to restart
        // the daemon" case.
        std::fs::write(
            dir.path().join("config.json"),
            r#"{"policy": {"preset": "deadline_first"}}"#,
        )
        .unwrap();
        let config = config_for_doctor_tests(dir.path());
        let ledger = LedgerStore::open(&config.ledger_path).unwrap();

        let result = handle_doctor(&ledger, &config);

        assert!(result.config_file_present);
        assert!(result.config_file_valid);
        assert_eq!(
            result.policy_preset, "balanced",
            "still reports the running preset"
        );
        assert!(
            !result.running_config_matches_disk,
            "a valid but disagreeing config.json must be flagged as stale"
        );
    }

    /// HORO-1169 dogfood finding: a stale daemon (still running an older
    /// binary) must shut itself down after replying to a client speaking
    /// a *newer* protocol, so the next hook invocation's
    /// `ensure_daemon_connection` spawns a fresh one automatically
    /// instead of leaving every subsequent hook call failing open
    /// forever until a human notices and manually kills the process.
    #[test]
    fn handle_connection_signals_shutdown_for_a_newer_client_protocol() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_daemon_config(dir.path());
        let mut ledger = LedgerStore::open(&config.ledger_path).unwrap();
        let mut current_task = None;

        let (client, server) = UnixStream::pair().unwrap();
        let envelope = RequestEnvelope {
            protocol_version: PROTOCOL_VERSION + 1,
            request: Request::Status,
        };
        wire::write_message(&client, &envelope).unwrap();

        let should_shut_down =
            handle_connection(server, &mut ledger, &mut current_task, &config).unwrap();
        assert!(
            should_shut_down,
            "a newer client protocol must signal the accept loop to shut down"
        );

        let reader = BufReader::new(client);
        let response: ResponseEnvelope = wire::read_message(reader).unwrap();
        assert!(matches!(response.response, Response::Error { .. }));
    }

    /// The mirror case: an *older* client than this daemon is a
    /// different, ambiguous situation (unclear which binary should give
    /// way) and must not trigger a shutdown — this is exactly the
    /// existing, unchanged "return an Error and keep serving" behavior.
    #[test]
    fn handle_connection_does_not_shut_down_for_an_older_client_protocol() {
        let dir = tempfile::tempdir().unwrap();
        let config = test_daemon_config(dir.path());
        let mut ledger = LedgerStore::open(&config.ledger_path).unwrap();
        let mut current_task = None;

        let (client, server) = UnixStream::pair().unwrap();
        let envelope = RequestEnvelope {
            protocol_version: PROTOCOL_VERSION.saturating_sub(1).max(1),
            request: Request::Status,
        };
        wire::write_message(&client, &envelope).unwrap();

        let should_shut_down =
            handle_connection(server, &mut ledger, &mut current_task, &config).unwrap();
        assert!(
            !should_shut_down,
            "an older client protocol must not shut the daemon down"
        );
    }
}
