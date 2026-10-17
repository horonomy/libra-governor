//! `libra-governor metered-pool <reserve|settle|release|status>` (HORO-1915):
//! the operator/agent-facing CLI for the metered shared-pool IPC. See
//! `libra_governor_protocol::metered_pool` module docs for the wire
//! contract and `libra_governor_daemon::metered_pool` for the handler
//! semantics this surface drives.
//!
//! `reserve`/`settle`/`release` spawn a daemon if none is running (same
//! precedent as `run_cmd`'s `ensure_daemon_connection`) — an agent step
//! reserving capacity should not fail merely because no daemon happened
//! to be running yet. `status` is read-only and never spawns one (same
//! precedent as `launch_cmd::status`'s `connect_only`) — asking about a
//! pool's posture should report the truth about whatever daemon is
//! actually running, never create one merely to answer.
//!
//! Output is one JSON line (`serde_json::to_string(&MeteredPoolOutcome)`)
//! on every path — this surface's consumers are scripts and agents, not
//! a human terminal, unlike `launch status`'s plain-text precedent.

use libra_governor_domain::{QuotaUnit, SharedPoolReservationId};
use libra_governor_protocol::{MeteredPoolOutcome, MeteredPoolRequest, Request, Response};

use crate::client;

const HELP: &str = "Usage:\n  \
    libra-governor metered-pool reserve --owner <owner> --pool <alias> --unit usd_cents|tokens|requests \
    --amount <n> --session <session-id> --key <idempotency-key> [--session-cap <n>]\n  \
    libra-governor metered-pool settle --reservation <uuid> --owner <owner> --session <session-id> [--actual <n>]\n  \
    libra-governor metered-pool release --reservation <uuid> --owner <owner> --session <session-id>\n  \
    libra-governor metered-pool status --owner <owner> --pool <alias>\n\n\
    Every subcommand prints one JSON line (a MeteredPoolOutcome). Exit codes: 0 success/status, \
    3 refused, 1 daemon/transport error, 2 usage.";

fn usage_error(message: &str) -> i32 {
    eprintln!("libra-governor metered-pool: {message}\n\n{HELP}");
    2
}

fn parse_unit(s: &str) -> Option<QuotaUnit> {
    match s {
        "usd_cents" => Some(QuotaUnit::UsdCents),
        "tokens" => Some(QuotaUnit::Tokens),
        "requests" => Some(QuotaUnit::Requests),
        _ => None,
    }
}

fn parse_reservation_id(s: &str) -> Option<SharedPoolReservationId> {
    uuid::Uuid::parse_str(s).ok().map(SharedPoolReservationId)
}

/// A minimal `--flag value` parser shared by every subcommand here. Not a
/// general-purpose arg-parsing library: this surface has few, simple,
/// always-valued flags, and pulling in a dependency for it would be the
/// kind of premature abstraction this repo's conventions avoid.
struct Flags<'a> {
    values: std::collections::HashMap<&'a str, &'a str>,
}

impl<'a> Flags<'a> {
    fn parse(args: &'a [String]) -> Result<Self, String> {
        let mut values = std::collections::HashMap::new();
        let mut i = 0;
        while i < args.len() {
            let flag = args[i].as_str();
            if !flag.starts_with("--") {
                return Err(format!("unexpected argument {flag:?}"));
            }
            let Some(value) = args.get(i + 1) else {
                return Err(format!("{flag} requires a value"));
            };
            values.insert(&flag[2..], value.as_str());
            i += 2;
        }
        Ok(Self { values })
    }

    fn require(&self, name: &str) -> Result<&str, String> {
        self.values
            .get(name)
            .copied()
            .ok_or_else(|| format!("--{name} is required"))
    }

    fn optional(&self, name: &str) -> Option<&str> {
        self.values.get(name).copied()
    }
}

fn print_outcome(outcome: &MeteredPoolOutcome) -> i32 {
    println!(
        "{}",
        serde_json::to_string(outcome).expect("MeteredPoolOutcome always serializes")
    );
    match outcome {
        MeteredPoolOutcome::Refused { .. } => 3,
        _ => 0,
    }
}

fn roundtrip(request: MeteredPoolRequest) -> i32 {
    let socket_path = match libra_governor_daemon::paths::socket_path() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("libra-governor metered-pool: could not resolve state dir: {e}");
            return 1;
        }
    };
    let stream = match client::ensure_daemon_connection(&socket_path) {
        Ok(stream) => stream,
        Err(e) => {
            eprintln!("libra-governor metered-pool: daemon unavailable: {e}");
            return 1;
        }
    };
    match client::roundtrip(
        &stream,
        Request::MeteredPool {
            metered_pool: request,
        },
    ) {
        Ok(Response::MeteredPool(outcome)) => print_outcome(&outcome),
        Ok(other) => {
            eprintln!("libra-governor metered-pool: unexpected daemon response: {other:?}");
            1
        }
        Err(e) => {
            eprintln!("libra-governor metered-pool: request failed: {e}");
            1
        }
    }
}

fn status_roundtrip(request: MeteredPoolRequest) -> i32 {
    let socket_path = match libra_governor_daemon::paths::socket_path() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("libra-governor metered-pool: could not resolve state dir: {e}");
            return 1;
        }
    };
    let stream = match client::connect_only(&socket_path) {
        Ok(stream) => stream,
        Err(e) => {
            eprintln!("libra-governor metered-pool: daemon unavailable: {e}");
            return 1;
        }
    };
    match client::roundtrip(
        &stream,
        Request::MeteredPool {
            metered_pool: request,
        },
    ) {
        Ok(Response::MeteredPool(outcome)) => print_outcome(&outcome),
        Ok(other) => {
            eprintln!("libra-governor metered-pool: unexpected daemon response: {other:?}");
            1
        }
        Err(e) => {
            eprintln!("libra-governor metered-pool: request failed: {e}");
            1
        }
    }
}

fn run_reserve(args: &[String]) -> i32 {
    let flags = match Flags::parse(args) {
        Ok(f) => f,
        Err(e) => return usage_error(&e),
    };
    let owner = match flags.require("owner") {
        Ok(v) => v,
        Err(e) => return usage_error(&e),
    };
    let alias = match flags.require("pool") {
        Ok(v) => v,
        Err(e) => return usage_error(&e),
    };
    let unit = match flags.require("unit") {
        Ok(v) => v,
        Err(e) => return usage_error(&e),
    };
    let Some(unit) = parse_unit(unit) else {
        return usage_error(&format!(
            "--unit must be one of usd_cents|tokens|requests, got {unit:?}"
        ));
    };
    let amount = match flags.require("amount").and_then(|v| {
        v.parse::<u64>()
            .map_err(|_| format!("--amount must be a non-negative integer, got {v:?}"))
    }) {
        Ok(v) => v,
        Err(e) => return usage_error(&e),
    };
    let session_id = match flags.require("session") {
        Ok(v) => v,
        Err(e) => return usage_error(&e),
    };
    let idempotency_key = match flags.require("key") {
        Ok(v) => v,
        Err(e) => return usage_error(&e),
    };
    let session_cap = match flags
        .optional("session-cap")
        .map(|v| {
            v.parse::<u64>()
                .map_err(|_| format!("--session-cap must be a non-negative integer, got {v:?}"))
        })
        .transpose()
    {
        Ok(v) => v,
        Err(e) => return usage_error(&e),
    };

    roundtrip(MeteredPoolRequest::Reserve {
        owner: owner.to_string(),
        alias: alias.to_string(),
        unit,
        amount,
        session_id: session_id.to_string(),
        idempotency_key: idempotency_key.to_string(),
        session_cap,
    })
}

fn run_settle(args: &[String]) -> i32 {
    let flags = match Flags::parse(args) {
        Ok(f) => f,
        Err(e) => return usage_error(&e),
    };
    let reservation_id = match flags.require("reservation") {
        Ok(v) => v,
        Err(e) => return usage_error(&e),
    };
    let Some(reservation_id) = parse_reservation_id(reservation_id) else {
        return usage_error(&format!(
            "--reservation must be a UUID, got {reservation_id:?}"
        ));
    };
    let owner = match flags.require("owner") {
        Ok(v) => v,
        Err(e) => return usage_error(&e),
    };
    let session_id = match flags.require("session") {
        Ok(v) => v,
        Err(e) => return usage_error(&e),
    };
    let actual = match flags
        .optional("actual")
        .map(|v| {
            v.parse::<u64>()
                .map_err(|_| format!("--actual must be a non-negative integer, got {v:?}"))
        })
        .transpose()
    {
        Ok(v) => v,
        Err(e) => return usage_error(&e),
    };

    roundtrip(MeteredPoolRequest::Settle {
        reservation_id,
        owner: owner.to_string(),
        session_id: session_id.to_string(),
        actual,
    })
}

fn run_release(args: &[String]) -> i32 {
    let flags = match Flags::parse(args) {
        Ok(f) => f,
        Err(e) => return usage_error(&e),
    };
    let reservation_id = match flags.require("reservation") {
        Ok(v) => v,
        Err(e) => return usage_error(&e),
    };
    let Some(reservation_id) = parse_reservation_id(reservation_id) else {
        return usage_error(&format!(
            "--reservation must be a UUID, got {reservation_id:?}"
        ));
    };
    let owner = match flags.require("owner") {
        Ok(v) => v,
        Err(e) => return usage_error(&e),
    };
    let session_id = match flags.require("session") {
        Ok(v) => v,
        Err(e) => return usage_error(&e),
    };

    roundtrip(MeteredPoolRequest::Release {
        reservation_id,
        owner: owner.to_string(),
        session_id: session_id.to_string(),
    })
}

fn run_status(args: &[String]) -> i32 {
    let flags = match Flags::parse(args) {
        Ok(f) => f,
        Err(e) => return usage_error(&e),
    };
    let owner = match flags.require("owner") {
        Ok(v) => v,
        Err(e) => return usage_error(&e),
    };
    let alias = match flags.require("pool") {
        Ok(v) => v,
        Err(e) => return usage_error(&e),
    };

    status_roundtrip(MeteredPoolRequest::Status {
        owner: owner.to_string(),
        alias: alias.to_string(),
    })
}

/// Entry point for `["metered-pool", ...]`. Returns the process exit
/// code; `main.rs` is responsible for calling `std::process::exit` with
/// it. A `--help`/`-h` anywhere in `args` (including no args at all)
/// prints usage to stderr and exits 2 without ever attempting a daemon
/// connection — `subcommand_help_purity.rs`'s own regression guard
/// (`main.rs` has no `["metered-pool", "--help"]` match arm, so this is
/// the only place that can special-case it, and it does so before any
/// subcommand dispatch).
pub fn run(args: &[String]) -> i32 {
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        return usage_error("missing subcommand");
    }
    let (operation, rest) = (args[0].as_str(), &args[1..]);
    match operation {
        "reserve" => run_reserve(rest),
        "settle" => run_settle(rest),
        "release" => run_release(rest),
        "status" => run_status(rest),
        other => usage_error(&format!("unknown operation {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserve_missing_required_flag_is_a_usage_error_not_a_panic() {
        let args: Vec<String> = vec!["--owner".to_string(), "acme".to_string()];
        assert_eq!(run_reserve(&args), 2);
    }

    #[test]
    fn reserve_rejects_an_invalid_unit() {
        let args: Vec<String> = [
            "--owner",
            "acme",
            "--pool",
            "usd-2026-10",
            "--unit",
            "euros",
            "--amount",
            "100",
            "--session",
            "s1",
            "--key",
            "k1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(run_reserve(&args), 2);
    }

    #[test]
    fn settle_rejects_a_non_uuid_reservation_id() {
        let args: Vec<String> = [
            "--reservation",
            "not-a-uuid",
            "--owner",
            "acme",
            "--session",
            "s1",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(run_settle(&args), 2);
    }

    #[test]
    fn run_with_no_args_is_a_usage_error() {
        assert_eq!(run(&[]), 2);
    }

    #[test]
    fn run_with_help_is_a_usage_error_and_never_dispatches() {
        assert_eq!(run(&["--help".to_string()]), 2);
        assert_eq!(run(&["reserve".to_string(), "--help".to_string()]), 2);
    }

    #[test]
    fn run_with_unknown_operation_is_a_usage_error() {
        assert_eq!(run(&["bogus".to_string()]), 2);
    }
}
