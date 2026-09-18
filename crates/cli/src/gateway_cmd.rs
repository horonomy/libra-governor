//! `libra-governor gateway token` and `libra-governor gateway status`
//! (HORO-1144).
//!
//! # Exactly two subcommands, and neither one decides anything
//!
//! `CLAUDE.md`: *"`crates/cli`: the `libra-governor` binary. Thin client
//! over the daemon; no independent policy logic."* So there is no
//! `gateway start`, no `gateway stop`, and no `gateway set-budget`. The
//! gateway is a security boundary, and a boundary a user (or anything
//! running as that user) can switch off with a one-line command is not
//! one. Turning it on is a daemon configuration change followed by a
//! restart — a deliberate act, not a keystroke.
//!
//! `gateway token` reads a local file; `gateway status` relays one
//! read-only request to the daemon and renders what comes back. That is
//! the whole surface.
//!
//! # What `gateway token` prints
//!
//! The **local capability token** — 32 bytes of OS randomness that
//! authorize use of a loopback-bound proxy on this machine. It is not a
//! provider credential, it is worth nothing anywhere else, and it is what
//! Claude Code's `apiKeyHelper` is meant to print. The real upstream API
//! key never passes through this command, this process, or Claude Code's
//! configuration at all — see
//! `docs/adr/0003-gateway-enforcement-boundary.md` §5.

use libra_governor_gateway::credential::LocalCapabilityToken;
use libra_governor_protocol::{
    EnforcementCapabilities, GatewayStatusResult, MonetaryEnforcement, Request, Response,
};

use crate::client;

/// The gateway token's filename inside the daemon's state directory.
pub const TOKEN_FILE_NAME: &str = "gateway.token";

/// Prints the local capability token, creating it if the gateway has
/// never run.
///
/// Creating on demand matters for setup order: a user wiring up
/// `apiKeyHelper` runs this command before the gateway has ever started,
/// and a command that answered "no token yet, start the gateway first"
/// would make the two steps circular.
pub fn run_token() {
    let state_dir = match libra_governor_daemon::paths::ensure_state_dir() {
        Ok(dir) => dir,
        Err(e) => {
            eprintln!("libra-governor gateway token: could not resolve state dir: {e}");
            std::process::exit(1);
        }
    };
    let path = state_dir.join(TOKEN_FILE_NAME);
    match LocalCapabilityToken::load_or_create(&path) {
        Ok(token) => println!("{}", token.expose_for_agent()),
        Err(e) => {
            eprintln!("libra-governor gateway token: could not read or create {path:?}: {e}");
            std::process::exit(1);
        }
    }
}

/// Relays `Request::GatewayStatus` and renders the answer.
pub fn run_status() {
    let socket_path = match libra_governor_daemon::paths::socket_path() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("libra-governor gateway status: could not resolve state dir: {e}");
            std::process::exit(1);
        }
    };

    // Connects only; never spawns. A user asking about the gateway's
    // state wants the truth about the daemon that is running, and
    // spawning a fresh one to answer would replace the question with a
    // different one.
    let stream = match client::connect_only(&socket_path) {
        Ok(stream) => stream,
        Err(e) => {
            eprintln!("libra-governor gateway status: daemon unavailable: {e}");
            std::process::exit(1);
        }
    };

    match client::roundtrip(&stream, Request::GatewayStatus) {
        Ok(Response::GatewayStatus(result)) => println!("{}", render_status(&result)),
        Ok(other) => {
            eprintln!("libra-governor gateway status: unexpected daemon response: {other:?}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("libra-governor gateway status: request failed: {e}");
            std::process::exit(1);
        }
    }
}

/// Renders a status result as a short human-readable report.
///
/// Pure — takes a result, returns a string, touches nothing — so the
/// exact wording of an honesty claim is directly testable rather than
/// buried behind a socket.
pub fn render_status(result: &GatewayStatusResult) -> String {
    let mut out = String::new();
    out.push_str("libra-governor gateway status\n");
    out.push_str("=============================\n\n");

    if result.running {
        let addr = result.bind_addr.as_deref().unwrap_or("(unknown address)");
        out.push_str(&format!("State:      running on {addr}\n"));
    } else {
        out.push_str("State:      not running\n");
        if let Some(reason) = &result.disabled_reason {
            out.push_str(&format!("Reason:     {reason}\n"));
        }
    }

    if let Some(capabilities) = &result.capabilities {
        out.push_str(&render_capabilities(capabilities));
    }

    out.push_str("\nRequests\n--------\n");
    out.push_str(&format!(
        "  forwarded:                {}\n",
        result.forwarded
    ));
    out.push_str(&format!(
        "  refused (budget):         {}\n",
        result.denied_budget
    ));
    out.push_str(&format!(
        "  refused (unenforceable):  {}\n",
        result.denied_unenforceable
    ));
    out.push_str(&format!(
        "  refused (unauthorized):   {}\n",
        result.denied_unauthorized
    ));
    out.push_str(&format!(
        "  approval-gated:           {}\n",
        result.approval_gated
    ));
    out.push_str(&format!(
        "  upstream errors:          {}\n",
        result.upstream_errors
    ));

    out.push_str("\nSettlement\n----------\n");
    out.push_str(&format!(
        "  from provider-reported usage: {}\n",
        result.settled_with_known_usage
    ));
    out.push_str(&format!(
        "  conservative fallback:        {}\n",
        result.settled_without_usage
    ));
    if result.settled_without_usage > 0 {
        out.push_str(
            "  (a conservative settlement charges the full reserved amount — nothing is \n\
             \x20  refunded that cannot be proven unspent.)\n",
        );
    }
    if result.bound_violations > 0 {
        out.push_str(&format!(
            "\n  WARNING: {} request(s) reported more output tokens than their own max_tokens\n\
             \x20  declared. The reservation arithmetic's bound was violated — see the daemon log.\n",
            result.bound_violations
        ));
    }

    out
}

/// Renders the capability statement, including — explicitly — what is
/// *not* enforced.
///
/// The "Monetary cap: NOT ENFORCED" line is the point of this whole
/// function: a subscription-mode deployment must read as honestly
/// unenforced rather than as silently fine.
fn render_capabilities(capabilities: &EnforcementCapabilities) -> String {
    let mut out = String::new();
    out.push_str(&format!("Tier:       {:?}\n", capabilities.tier));
    out.push_str(&format!(
        "Credential: {:?}\n",
        capabilities.credential_custody
    ));
    out.push_str(&format!(
        "Usage:      {:?}\n",
        capabilities.usage_accounting
    ));
    match &capabilities.monetary_enforcement {
        MonetaryEnforcement::Enforced { pricing_version } => {
            out.push_str(&format!(
                "Monetary cap: ENFORCED, priced against {pricing_version}\n"
            ));
        }
        MonetaryEnforcement::NotAvailable { reason } => {
            out.push_str(&format!("Monetary cap: NOT ENFORCED ({reason:?})\n"));
        }
    }
    out.push_str(&format!(
        "Pre-spend refusal: {}\n",
        if capabilities.pre_spend_refusal {
            "yes — a request can be refused before any provider spend"
        } else {
            "no — hooks fire after the request is already in flight"
        }
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use libra_governor_protocol::EnforcementTier;

    fn result_for(tier: EnforcementTier, running: bool) -> GatewayStatusResult {
        GatewayStatusResult {
            running,
            disabled_reason: (!running).then(|| "no gateway is configured".to_string()),
            capabilities: Some(EnforcementCapabilities::for_tier(tier, "pricing-test-v1")),
            bind_addr: running.then(|| "127.0.0.1:8787".to_string()),
            forwarded: 3,
            denied_budget: 1,
            denied_unenforceable: 2,
            denied_unauthorized: 0,
            approval_gated: 1,
            settled_with_known_usage: 3,
            settled_without_usage: 0,
            upstream_errors: 0,
            bound_violations: 0,
        }
    }

    #[test]
    fn a_metered_gateway_reports_an_enforced_monetary_cap_and_its_pricing_version() {
        let rendered = render_status(&result_for(EnforcementTier::GatewayMetered, true));
        assert!(rendered.contains("Monetary cap: ENFORCED"));
        assert!(rendered.contains("pricing-test-v1"));
        assert!(rendered.contains("running on 127.0.0.1:8787"));
    }

    #[test]
    fn a_subscription_gateway_says_plainly_that_no_monetary_cap_is_enforced() {
        let rendered = render_status(&result_for(EnforcementTier::GatewayObservedQuota, true));
        assert!(
            rendered.contains("Monetary cap: NOT ENFORCED"),
            "a subscription deployment must never read as though it has a monetary cap: \
             {rendered}"
        );
        assert!(rendered.contains("OpaqueProviderQuota"));
        assert!(!rendered.contains("Monetary cap: ENFORCED"));
    }

    #[test]
    fn a_hooks_only_deployment_says_there_is_no_pre_spend_refusal() {
        let rendered = render_status(&result_for(EnforcementTier::HooksOnly, false));
        assert!(rendered.contains("not running"));
        assert!(rendered.contains("no gateway is configured"));
        assert!(rendered.contains("hooks fire after the request is already in flight"));
    }

    #[test]
    fn a_conservative_settlement_count_is_explained_rather_than_shown_bare() {
        let mut result = result_for(EnforcementTier::GatewayMetered, true);
        result.settled_without_usage = 4;
        let rendered = render_status(&result);
        assert!(rendered.contains("conservative fallback:        4"));
        assert!(rendered.contains("nothing is"));
    }

    #[test]
    fn a_bound_violation_is_surfaced_as_a_warning() {
        let mut result = result_for(EnforcementTier::GatewayMetered, true);
        result.bound_violations = 2;
        let rendered = render_status(&result);
        assert!(rendered.contains("WARNING"));
        assert!(rendered.contains("more output tokens than their own max_tokens"));
    }

    #[test]
    fn every_refusal_category_is_reported_separately() {
        let rendered = render_status(&result_for(EnforcementTier::GatewayMetered, true));
        assert!(rendered.contains("refused (budget):         1"));
        assert!(rendered.contains("refused (unenforceable):  2"));
        assert!(rendered.contains("refused (unauthorized):   0"));
    }
}
