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

use libra_governor_ledger::LedgerStore;
use libra_governor_protocol::{
    wire, PreflightResult, ReconSummary, Request, RequestEnvelope, Response, ResponseEnvelope,
    StatusResult, TaskSummary, PROTOCOL_VERSION,
};

use crate::{contract, log, recon, recon::ReconBudget};

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
}

pub struct DaemonConfig {
    pub socket_path: PathBuf,
    pub ledger_path: PathBuf,
    pub log_path: PathBuf,
    pub recon_budget: ReconBudget,
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

/// Runs the daemon's blocking accept loop against an already-bound
/// listener. Returns only on an unrecoverable I/O error accepting a new
/// connection; per-connection errors are caught and logged, never
/// propagated (one bad request must not take the daemon down).
pub fn serve(listener: UnixListener, config: &DaemonConfig) -> Result<(), DaemonError> {
    let mut ledger = LedgerStore::open(&config.ledger_path)?;
    let mut current_task: Option<TaskSummary> = None;

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
                *current_task = Some(TaskSummary {
                    task_id: result.task_id,
                    confidence: result.confidence,
                    recon_cost_seconds: result.recon_cost_seconds,
                });
                Response::Preflight(result)
            }
            Err(e) => {
                log::append_line(&config.log_path, &format!("preflight error: {e}"));
                Response::Error {
                    message: "internal error handling preflight request".to_string(),
                }
            }
        },
        Request::Status => Response::Status(StatusResult {
            current_task: current_task.clone(),
        }),
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

    let task_id = ledger.resolve_or_create_task_for_session(session_id, now)?;
    let previous_contract = ledger.latest_contract(task_id)?;

    let recon = recon::run_recon(cwd, task_hint, &config.recon_budget);
    let contract = contract::draft_contract(previous_contract.as_ref(), &recon);
    ledger.insert_contract(task_id, &contract, now)?;

    let plan = libra_governor_domain::ExecutionPlan::new(task_id, contract.revision, None, now);
    ledger.insert_plan(&plan)?;

    // Supersede any prior in-flight preflight for this session before
    // recording the new one, so at most one is ever in_flight at a time
    // — this is what keeps a re-invoked hook (new prompt, or a user
    // cancelling mid-session) from leaving orphaned "active" state.
    ledger.supersede_in_flight_preflights(session_id)?;
    ledger.record_preflight(session_id, task_id, plan.id, now)?;

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
        estimate: None,
    })
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
}
