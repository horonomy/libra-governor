//! `libra-governor launch status` (HORO-1766 PR2): read-only rendering of
//! `LaunchRequest::Status`.
//!
//! Never spawns a daemon — mirrors `gateway status`'s `connect_only`
//! precedent (HORO-1144, `crate::gateway_cmd::run_status`): asking about
//! the launch-gate's admission posture should report the truth about
//! whatever daemon is actually running, not create one merely to answer.
//!
//! No `--json` output. This CLI's existing precedent (confirmed against
//! `quota_observation_cmd`, which deliberately has no `--json` on any of
//! its subcommands) is plain, line-oriented human output for an operator
//! command like this one; `doctor`/`agents`/`economics explain` add
//! `--json` specifically because they feed machine consumers (dashboards,
//! evidence export). `launch status` has no such consumer yet, so it
//! follows the narrower precedent rather than the broader one.

use libra_governor_protocol::{LaunchOutcome, LaunchRequest, PoolStatus, Request, Response};

use crate::client;

/// `libra-governor launch status`. Returns the process exit code to use.
pub fn status() -> i32 {
    let socket_path = match libra_governor_daemon::paths::socket_path() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("libra-governor launch status: could not resolve state dir: {e}");
            return 1;
        }
    };

    let stream = match client::connect_only(&socket_path) {
        Ok(stream) => stream,
        Err(e) => {
            eprintln!("libra-governor launch status: daemon unavailable: {e}");
            return 1;
        }
    };

    match client::roundtrip(
        &stream,
        Request::Launch {
            launch: LaunchRequest::Status {},
        },
    ) {
        Ok(Response::Launch(outcome)) => match *outcome {
            LaunchOutcome::StatusReport { pools } => {
                println!("{}", render_status(&pools));
                0
            }
            other => {
                eprintln!("libra-governor launch status: unexpected daemon response: {other:?}");
                1
            }
        },
        Ok(other) => {
            eprintln!("libra-governor launch status: unexpected daemon response: {other:?}");
            1
        }
        Err(e) => {
            eprintln!("libra-governor launch status: request failed: {e}");
            1
        }
    }
}

/// Renders a `Status` reply as a short human-readable report. Pure — a
/// result in, a string out, nothing touched — so the exact wording is
/// directly testable rather than buried behind a socket.
pub fn render_status(pools: &[PoolStatus]) -> String {
    let mut out = String::new();
    out.push_str("libra-governor launch status\n");
    out.push_str("=============================\n\n");

    if pools.is_empty() {
        out.push_str("No [launcher] pools configured (the launch gate is off).\n");
        return out;
    }

    for pool in pools {
        out.push_str(&format!(
            "alias={} enabled={} active={}/{} (configured_capacity={}) scope={}\n",
            pool.alias,
            pool.enabled,
            pool.active,
            pool.effective_capacity,
            pool.configured_capacity,
            pool.enforcement_scope,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_pools_reports_the_gate_is_off() {
        let rendered = render_status(&[]);
        assert!(rendered.contains("No [launcher] pools configured"));
    }

    #[test]
    fn a_configured_pool_reports_its_occupancy_and_scope() {
        let rendered = render_status(&[PoolStatus {
            alias: "agents".to_string(),
            configured_capacity: 2,
            effective_capacity: 2,
            active: 1,
            enabled: true,
            enforcement_scope: libra_governor_protocol::LAUNCH_GATE_ENFORCEMENT_SCOPE.to_string(),
        }]);
        assert!(rendered.contains("alias=agents"));
        assert!(rendered.contains("enabled=true"));
        assert!(rendered.contains("active=1/2"));
        assert!(rendered.contains("configured_capacity=2"));
        assert!(rendered.contains("launcher_owned_direct_children_only"));
    }
}
