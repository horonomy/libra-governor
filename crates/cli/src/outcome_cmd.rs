//! `libra-governor outcome record` — pushes an outcome attestation for a
//! task over the daemon's existing Unix socket (HORO-1174).
//!
//! Reads one JSON object from stdin, same convention as every `hook`
//! subcommand (see `crates/cli/src/hook.rs` module docs): a stdin/stdout
//! contract, not flags, and no independent policy logic — this command is
//! a thin, deterministic translation from a JSON payload to
//! `Request::RecordOutcome`. This is the CLI entry point
//! `examples/local-providers/report_outcome.sh` drives; an external
//! Outcome Provider is expected to shell out to
//! `libra-governor outcome record` rather than speak the wire protocol
//! directly.

use libra_governor_domain::{ExecutionOutcome, PlanId, TaskId};
use libra_governor_protocol::{OutcomeRecordedOutcome, Request, Response};
use serde::Deserialize;

use crate::client;

/// The stdin JSON shape this command accepts.
#[derive(Debug, Deserialize)]
struct OutcomeInput {
    task_id: TaskId,
    #[serde(default)]
    plan_id: Option<PlanId>,
    source_id: String,
    idempotency_key: String,
    outcome: ExecutionOutcome,
}

pub fn run() {
    let mut raw = String::new();
    if let Err(e) = std::io::Read::read_to_string(&mut std::io::stdin(), &mut raw) {
        eprintln!("libra-governor outcome record: could not read stdin: {e}");
        std::process::exit(1);
    }

    let input: OutcomeInput = match serde_json::from_str(&raw) {
        Ok(input) => input,
        Err(e) => {
            eprintln!("libra-governor outcome record: malformed JSON on stdin: {e}");
            std::process::exit(1);
        }
    };

    let socket_path = match libra_governor_daemon::paths::socket_path() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("libra-governor outcome record: could not resolve state dir: {e}");
            std::process::exit(1);
        }
    };

    let stream = match client::ensure_daemon_connection(&socket_path) {
        Ok(stream) => stream,
        Err(e) => {
            eprintln!("libra-governor outcome record: daemon unavailable: {e}");
            std::process::exit(1);
        }
    };

    let request = Request::RecordOutcome {
        task_id: input.task_id,
        plan_id: input.plan_id,
        source_id: input.source_id,
        idempotency_key: input.idempotency_key,
        outcome: input.outcome,
    };

    match client::roundtrip(&stream, request) {
        Ok(Response::OutcomeRecorded(outcome)) => {
            print_result(&outcome);
            if matches!(outcome, OutcomeRecordedOutcome::NoSuchTask) {
                std::process::exit(1);
            }
        }
        Ok(other) => {
            eprintln!("libra-governor outcome record: unexpected daemon response: {other:?}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("libra-governor outcome record: request failed: {e}");
            std::process::exit(1);
        }
    }
}

fn print_result(outcome: &OutcomeRecordedOutcome) {
    match serde_json::to_string(outcome) {
        Ok(json) => println!("{json}"),
        Err(_) => println!("{outcome:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_input_deserializes_a_real_provider_shape() {
        let json = format!(
            r#"{{
                "task_id": "{}",
                "source_id": "example-provider",
                "idempotency_key": "ci-run-42",
                "outcome": {{"kind": "completed", "evidence": ["https://ci.example.com/runs/42"]}}
            }}"#,
            TaskId::new()
        );
        let input: OutcomeInput = serde_json::from_str(&json).unwrap();
        assert_eq!(input.source_id, "example-provider");
        assert!(input.plan_id.is_none());
        assert!(matches!(input.outcome, ExecutionOutcome::Completed { .. }));
    }
}
