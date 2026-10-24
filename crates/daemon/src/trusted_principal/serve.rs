//! Trusted mode's own bind step and accept loop (ADR-0019 §1/§3, design
//! doc §4.2-§4.4). `serve` and `handle_connection` in `crate::server` are
//! not modified — this is a parallel path, not a patch to the default
//! one.

use std::io::{BufReader, Read};
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::Instant;

use libra_governor_ledger::LedgerStore;
use libra_governor_protocol::{
    wire, Request, RequestEnvelope, Response, ResponseEnvelope, TaskSummary, PROTOCOL_VERSION,
};

use crate::server::{
    bind_or_detect_running, dispatch, reconcile_stale_reservations, DaemonConfig, DaemonError,
};

use super::peer::{classify, peer_uid};
use super::{TrustedPrincipalConfig, MAX_REQUEST_BYTES};

/// Binds the trusted-mode socket. Reuses `bind_or_detect_running`
/// unchanged (its stale-socket `remove_file` is now a provider-only
/// operation, because nobody else can write the socket dir), then widens
/// the socket's mode to `0o660` — **required, not cosmetic**: the
/// default path hardens the socket to `0600`, and on Linux the agent
/// needs write permission on the socket inode to `connect`. Without this
/// step every agent-facing harness test would pass with `EACCES` for the
/// wrong reason. The group comes from the setgid `socket_dir`
/// (`lg-clients`); between the `0600` and `0660` chmods, only the
/// provider can connect, which is harmless.
pub fn bind_trusted_socket(cfg: &TrustedPrincipalConfig) -> Result<UnixListener, DaemonError> {
    use std::os::unix::fs::PermissionsExt;

    let socket_path = cfg.socket_path();
    let listener = bind_or_detect_running(&socket_path)?;
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o660))?;
    Ok(listener)
}

/// A `Read` wrapper that enforces one total per-connection deadline
/// (not a per-read-call timeout): `SO_RCVTIMEO` is per call, so a client
/// sending one byte every N-minus-epsilon seconds would otherwise hold
/// the serial accept loop forever. Before every `read`, this sets the
/// stream's read timeout to whatever time remains until `deadline`, and
/// returns `TimedOut` immediately (without calling `set_read_timeout`,
/// which is an `EINVAL` on `Duration::ZERO`) once the deadline has
/// already passed.
pub(crate) struct DeadlineReader {
    pub stream: UnixStream,
    pub deadline: Instant,
}

impl Read for DeadlineReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::from(std::io::ErrorKind::TimedOut));
        }
        self.stream.set_read_timeout(Some(remaining))?;
        self.stream.read(buf)
    }
}

/// An exhaustive `match` over all 9 [`Request`] variants, with no `_ =>`
/// arm: `Request` is not `#[non_exhaustive]`, so a future path-bearing
/// variant becomes a compile error in feature builds rather than a
/// silently-missed sink. Blanks every agent-named path before `dispatch`
/// sees the request, neutralizing four sinks (design doc §4.4):
///
/// | Sink | Where the deref happens today |
/// |---|---|
/// | `Preflight.cwd` | `handle_preflight` -> recon, and `features::derive_task_features` -> `repo_key` -> `canonicalize` |
/// | `Finalize.transcript_path` | `handle_finalize` -> transcript-measured usage |
/// | `ExecutionOwner` -> `Prompt.cwd` | `execution_owner::handle` -> recon, then `repo_key` |
/// | `ExecutionOwner` -> `Stop.transcript_path` | ignored today; blanked anyway for defense in depth |
///
/// The empty path is the sentinel, not a child of the provider's own
/// state dir: `stat("")`/`realpath("")` return `ENOENT` by POSIX without
/// resolving anything, and no principal can create an entry named `""`.
/// A child of the provider's private state dir would instead point any
/// future `cwd.join(..)` at the provider's own secrets — the exact
/// confused deputy this sanitizer exists to prevent. `run_trusted`
/// additionally pins the process cwd to `/`, so even a future relative
/// join resolves under a root-owned directory.
pub(crate) fn strip_agent_named_paths(request: &mut Request) {
    match request {
        Request::Preflight { cwd, .. } => *cwd = std::path::PathBuf::new(),
        Request::Finalize {
            transcript_path, ..
        } => *transcript_path = None,
        Request::ExecutionOwner { event } => {
            match &mut event.native_context.operation {
                libra_governor_protocol::ExecutionOperation::Prompt { cwd, .. } => {
                    *cwd = std::path::PathBuf::new();
                }
                libra_governor_protocol::ExecutionOperation::Stop {
                    transcript_path, ..
                } => {
                    *transcript_path = None;
                }
                libra_governor_protocol::ExecutionOperation::Tool { .. }
                | libra_governor_protocol::ExecutionOperation::Query {} => {}
            }
            // `event.identity`/`event.native_context.identity` carry no
            // filesystem path (see `ExecutionIdentity`'s own shape) —
            // nothing else in this variant is a sink.
        }
        Request::Status
        | Request::ToolInvoked { .. }
        | Request::CalibrationReport
        | Request::GatewayStatus
        | Request::Doctor
        | Request::RecordOutcome { .. } => {}
        // `LaunchRequest`'s variants carry only `pool_alias: String`,
        // `launch_id`/`reservation_id` (opaque identifiers, never a
        // filesystem path) — see `crates/protocol/src/launch.rs`. No
        // sink exists here to strip.
        Request::Launch { .. } => {}
        // `MeteredPoolRequest`'s variants carry only `owner`/`alias`/
        // `session_id`/`idempotency_key` (opaque identifiers) and a
        // `reservation_id` — see `crates/protocol/src/metered_pool.rs`.
        // No filesystem-path field exists here to strip.
        Request::MeteredPool { .. } => {}
        // `SharedBudgetRequest` carries only opaque session/agent/prompt
        // identifiers, an amount, an idempotency key and an RFC 3339
        // event time -- no filesystem path. Trusted mode also runs with
        // no shared-budget file, so every such request answers
        // `not_configured`.
        Request::SharedBudget { .. } => {}
    }
}

/// Trusted mode's own accept loop. Starts no gateway and no extensions
/// (both config fields are `None` anyway in trusted mode, but the calls
/// are omitted entirely so the provider spawns no subprocess and no
/// thread). For each accepted stream: classify the peer (dropping any
/// unclassified one, including root, before a byte is read), compute a
/// per-connection deadline, then handle exactly one request/response
/// exchange. There is no `break` anywhere in this loop: a
/// newer-protocol-version client gets `Error` and the daemon keeps
/// serving (ADR-0019 §1 "No client-triggered shutdown").
pub fn serve_trusted(
    listener: UnixListener,
    mut ledger: LedgerStore,
    config: &DaemonConfig,
    trusted: &TrustedPrincipalConfig,
) -> Result<(), DaemonError> {
    let mut current_task: Option<TaskSummary> = None;
    reconcile_stale_reservations(&mut ledger, config);

    for incoming in listener.incoming() {
        let stream = match incoming {
            Ok(stream) => stream,
            Err(e) => {
                crate::log::append_line(&config.log_path, &format!("trusted accept error: {e}"));
                continue;
            }
        };

        let class = match peer_uid(&stream)
            .ok()
            .and_then(|uid| classify(uid, trusted))
        {
            Some(class) => class,
            None => {
                crate::log::append_line(&config.log_path, "trusted: dropped unclassified peer");
                continue;
            }
        };
        crate::log::append_line(
            &config.log_path,
            &format!("trusted: accepted connection from {class:?}"),
        );

        let deadline = Instant::now() + trusted.connection_timeout();
        if let Err(e) =
            handle_trusted_connection(stream, &mut ledger, &mut current_task, config, deadline)
        {
            crate::log::append_line(&config.log_path, &format!("trusted connection error: {e}"));
        }
    }
    Ok(())
}

fn handle_trusted_connection(
    stream: UnixStream,
    ledger: &mut LedgerStore,
    current_task: &mut Option<TaskSummary>,
    config: &DaemonConfig,
    deadline: Instant,
) -> Result<(), DaemonError> {
    let write_timeout = deadline.saturating_duration_since(Instant::now());
    if write_timeout.is_zero() {
        return Err(DaemonError::Io(std::io::Error::from(
            std::io::ErrorKind::TimedOut,
        )));
    }
    stream.set_write_timeout(Some(write_timeout))?;

    let deadline_reader = DeadlineReader {
        stream: stream.try_clone()?,
        deadline,
    };
    let reader = BufReader::new(deadline_reader.take(MAX_REQUEST_BYTES));

    let response = match wire::read_message::<_, RequestEnvelope>(reader) {
        Ok(envelope) if envelope.protocol_version != PROTOCOL_VERSION => Response::Error {
            message: format!(
                "protocol version mismatch: daemon speaks {PROTOCOL_VERSION}, client sent {}",
                envelope.protocol_version
            ),
        },
        Ok(mut envelope) => {
            strip_agent_named_paths(&mut envelope.request);
            dispatch(envelope.request, ledger, current_task, config)
        }
        Err(e) => {
            crate::log::append_line(
                &config.log_path,
                &format!("trusted: malformed request: {e}"),
            );
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

#[cfg(test)]
mod tests {
    use super::*;
    use libra_governor_protocol::ExecutionOwnerRequest;
    use std::path::PathBuf;
    use std::time::Duration;

    #[test]
    fn strip_agent_named_paths_blanks_preflight_cwd() {
        let mut request = Request::Preflight {
            task_hint: "fix the bug".to_string(),
            cwd: PathBuf::from("/home/agent/project"),
            session_id: "s1".to_string(),
        };
        strip_agent_named_paths(&mut request);
        match request {
            Request::Preflight { cwd, .. } => assert_eq!(cwd, PathBuf::new()),
            _ => panic!("expected Preflight"),
        }
    }

    #[test]
    fn strip_agent_named_paths_blanks_finalize_transcript_path() {
        let mut request = Request::Finalize {
            session_id: "s1".to_string(),
            model: None,
            provider: None,
            transcript_path: Some("/home/agent/.transcript".to_string()),
        };
        strip_agent_named_paths(&mut request);
        match request {
            Request::Finalize {
                transcript_path, ..
            } => assert_eq!(transcript_path, None),
            _ => panic!("expected Finalize"),
        }
    }

    #[test]
    fn strip_agent_named_paths_leaves_non_path_bearing_variants_untouched() {
        let mut request = Request::Status;
        strip_agent_named_paths(&mut request);
        assert_eq!(request, Request::Status);

        let mut request = Request::ToolInvoked {
            session_id: "s1".to_string(),
            tool_name: "bash".to_string(),
        };
        strip_agent_named_paths(&mut request);
        assert!(matches!(request, Request::ToolInvoked { .. }));
    }

    fn sample_identity() -> libra_governor_domain::ExecutionIdentity {
        serde_json::from_value(serde_json::json!({
            "envelope_version": 1,
            "observed_at": "2026-10-11T00:00:00Z",
            "host_id": "claude-code",
            "tool_provider": "anthropic",
            "tool_instance_id": "i1",
            "provider_session_id": "sess1",
            "agent_id": "agent1",
            "turn_id": "turn1",
            "lineage_status": "root"
        }))
        .unwrap()
    }

    #[test]
    fn strip_agent_named_paths_blanks_execution_owner_prompt_cwd() {
        let mut request = Request::ExecutionOwner {
            event: Box::new(ExecutionOwnerRequest {
                association_version: libra_governor_domain::EXECUTION_ASSOCIATION_VERSION,
                identity: sample_identity(),
                native_context: libra_governor_protocol::NativeExecutionContext {
                    identity: sample_identity(),
                    operation: libra_governor_protocol::ExecutionOperation::Prompt {
                        task_hint: "fix it".to_string(),
                        cwd: PathBuf::from("/home/agent/project"),
                        supersedes_turn: None,
                    },
                },
            }),
        };
        strip_agent_named_paths(&mut request);
        match request {
            Request::ExecutionOwner { event } => match event.native_context.operation {
                libra_governor_protocol::ExecutionOperation::Prompt { cwd, .. } => {
                    assert_eq!(cwd, PathBuf::new())
                }
                _ => panic!("expected Prompt"),
            },
            _ => panic!("expected ExecutionOwner"),
        }
    }

    #[test]
    fn strip_agent_named_paths_blanks_execution_owner_stop_transcript_path() {
        let mut request = Request::ExecutionOwner {
            event: Box::new(ExecutionOwnerRequest {
                association_version: libra_governor_domain::EXECUTION_ASSOCIATION_VERSION,
                identity: sample_identity(),
                native_context: libra_governor_protocol::NativeExecutionContext {
                    identity: sample_identity(),
                    operation: libra_governor_protocol::ExecutionOperation::Stop {
                        model: None,
                        transcript_path: Some("/home/agent/.transcript".to_string()),
                    },
                },
            }),
        };
        strip_agent_named_paths(&mut request);
        match request {
            Request::ExecutionOwner { event } => match event.native_context.operation {
                libra_governor_protocol::ExecutionOperation::Stop {
                    transcript_path, ..
                } => {
                    assert_eq!(transcript_path, None)
                }
                _ => panic!("expected Stop"),
            },
            _ => panic!("expected ExecutionOwner"),
        }
    }

    #[test]
    fn deadline_reader_reports_timed_out_once_the_deadline_has_passed() {
        let (a, _b) = UnixStream::pair().unwrap();
        let mut reader = DeadlineReader {
            stream: a,
            deadline: Instant::now() - Duration::from_secs(1),
        };
        let mut buf = [0u8; 8];
        let result = reader.read(&mut buf);
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
    }
}
