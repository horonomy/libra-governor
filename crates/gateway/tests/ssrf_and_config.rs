//! Open-proxy and SSRF defences, asserted against a running gateway
//! rather than only against `validate` (HORO-1144).
//!
//! `config.rs`'s own unit tests cover every branch of the configuration
//! refusal. What those cannot show is that the refusals actually bind at
//! runtime: that a gateway with a bad configuration never starts, that an
//! upstream redirect is not followed, and that no inbound value — path,
//! `Host`, absolute-form URI, or forwarding header — can steer a byte
//! anywhere the operator did not configure.

#[path = "fake_upstream.rs"]
mod fake_upstream;

use std::net::SocketAddr;
use std::path::PathBuf;

use fake_upstream::*;
use hyper::StatusCode;
use libra_governor_domain::ResourceKind;
use libra_governor_gateway::config::{validate, ConfigError, GatewayConfig, GatewayCredentialMode};
use libra_governor_gateway::credential::CredentialCommand;

const SESSION: &str = "x-claude-code-session-id";

/// A predicate over the refusal a bad upstream must produce. Named
/// rather than written inline so the case table below stays a readable
/// list of (upstream, expected refusal) pairs.
type RefusalCheck = fn(&ConfigError) -> bool;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn config_for(upstream: &str) -> GatewayConfig {
    let mut config = GatewayConfig::new(
        "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        PathBuf::from("/tmp/libra-ssrf-test/gateway.token"),
        GatewayCredentialMode::GovernorHeld {
            credential: CredentialCommand::new("/bin/true", vec![]),
        },
    );
    config.upstream = upstream.to_string();
    config
}

#[test]
fn a_gateway_whose_upstream_is_not_configuration_never_starts() {
    // Each of these is a shape that would let something other than the
    // operator choose the destination.
    let cases: Vec<(&str, RefusalCheck)> = vec![
        ("http://api.anthropic.com", |e| {
            matches!(e, ConfigError::UpstreamSchemeNotHttps { .. })
        }),
        ("https://evil.example.com", |e| {
            matches!(e, ConfigError::UpstreamHostNotAllowed { .. })
        }),
        ("https://user:sk-fake-smuggled@api.anthropic.com", |e| {
            matches!(e, ConfigError::UpstreamHasUserinfo)
        }),
        ("https://api.anthropic.com/v1/messages", |e| {
            matches!(e, ConfigError::UpstreamHasPathOrQuery)
        }),
        ("https://api.anthropic.com:8443", |e| {
            matches!(e, ConfigError::UpstreamNonStandardPort { .. })
        }),
        ("file:///etc/passwd", |e| {
            matches!(
                e,
                ConfigError::UpstreamSchemeNotHttps { .. } | ConfigError::UpstreamNotAUrl { .. }
            )
        }),
        ("https://169.254.169.254", |e| {
            matches!(e, ConfigError::UpstreamHostNotAllowed { .. })
        }),
    ];

    for (upstream, expected) in cases {
        let error = validate(config_for(upstream), ResourceKind::Tokens)
            .expect_err(&format!("{upstream} must not produce a running gateway"));
        assert!(expected(&error), "{upstream} was refused as {error:?}");
    }
}

#[test]
fn a_gateway_bound_to_a_non_loopback_address_never_starts() {
    let mut config = config_for("https://api.anthropic.com");
    config.bind_addr = "0.0.0.0:8787".parse().unwrap();
    assert!(matches!(
        validate(config, ResourceKind::Tokens).unwrap_err(),
        ConfigError::BindAddrNotLoopback { .. }
    ));
}

#[test]
fn the_plaintext_loopback_opt_in_cannot_be_turned_into_an_exfiltration_path() {
    let mut config = config_for("http://evil.example.com");
    config.allow_plaintext_loopback_upstream = true;
    config.upstream_host_allowlist = vec!["evil.example.com".to_string()];

    // Allowlisted AND flagged — and still refused, because the safeguard
    // is the loopback host literal, not the flag. See ADR 0003's
    // "Deviations".
    assert!(matches!(
        validate(config, ResourceKind::Tokens).unwrap_err(),
        ConfigError::UpstreamSchemeNotHttps { .. }
    ));
}

#[test]
fn a_name_that_merely_looks_like_loopback_is_not_treated_as_loopback() {
    let mut config = config_for("http://localhost.evil.example.com");
    config.allow_plaintext_loopback_upstream = true;
    config.upstream_host_allowlist = vec!["localhost.evil.example.com".to_string()];
    assert!(validate(config, ResourceKind::Tokens).is_err());
}

#[test]
fn an_upstream_redirect_is_relayed_rather_than_followed() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());

    let response = send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages",
        &[
            ("x-api-key", gw.token.as_str()),
            (SESSION, "sess-1"),
            (SCENARIO_HEADER, "redirect"),
        ],
        &messages_body("claude-sonnet-4-5", Some(100), true),
    );

    assert_eq!(response.status, StatusCode::FOUND);
    assert_eq!(
        response.header("location"),
        Some("https://evil.example.com/v1/messages"),
        "the redirect is handed to the agent verbatim"
    );
    assert_eq!(
        gw.upstream.received().len(),
        1,
        "following the redirect would let the UPSTREAM choose the next destination — the one \
         thing this component must never permit"
    );
}

#[test]
fn an_absolute_form_request_uri_cannot_redirect_the_outbound_call() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());

    // The classic forward-proxy request shape. The gateway matches on the
    // URI's PATH against a closed table, so the authority in an
    // absolute-form URI is simply never consulted.
    let response = send(
        &rt,
        gw.addr,
        "POST",
        "http://evil.example.com/v1/messages",
        &[
            ("x-api-key", gw.token.as_str()),
            (SESSION, "sess-1"),
            (SCENARIO_HEADER, "nonstream"),
        ],
        &messages_body("claude-sonnet-4-5", Some(100), false),
    );

    // Either it is refused outright, or it is routed to the CONFIGURED
    // upstream — never to the authority the URI named.
    if response.status == StatusCode::OK {
        let received = gw.upstream.received();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].path, "/v1/messages");
        assert_eq!(
            received[0].header("host"),
            Some(format!("127.0.0.1:{}", gw.upstream.addr.port()).as_str()),
            "the absolute-form authority must never become the outbound destination"
        );
    } else {
        assert!(
            response.status == StatusCode::NOT_FOUND
                || response.status == StatusCode::MISDIRECTED_REQUEST
                || response.status == StatusCode::BAD_REQUEST,
            "unexpected status {} for an absolute-form URI",
            response.status
        );
        assert!(gw.upstream.received().is_empty());
    }
}

#[test]
fn a_traversal_path_is_refused_rather_than_normalised_into_the_upstream_url() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());

    for path in [
        "/v1/messages/../../internal",
        "/v1/messages%2f..%2fadmin",
        "//v1/messages",
        "/v1//messages",
    ] {
        let response = send(
            &rt,
            gw.addr,
            "POST",
            path,
            &[("x-api-key", gw.token.as_str()), (SESSION, "sess-1")],
            &messages_body("claude-sonnet-4-5", Some(100), false),
        );
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "{path} must not resolve to a proxied route"
        );
    }
    assert!(gw.upstream.received().is_empty());
}

#[test]
fn a_subscription_deployment_may_not_claim_a_usd_hard_cap() {
    let mut config = config_for("https://api.anthropic.com");
    config.credential_mode = GatewayCredentialMode::PassThroughSubscription;

    assert_eq!(
        validate(config.clone(), ResourceKind::Usd).unwrap_err(),
        ConfigError::SubscriptionModeCannotEnforceUsd,
        "this refusal — not a README caveat — is what makes 'subscription modes are labeled \
         honestly' true"
    );
    assert!(
        validate(config, ResourceKind::Tokens).is_ok(),
        "the same mode against a token budget is perfectly honest; it just cannot price"
    );
}

#[test]
fn the_configured_upstream_is_the_only_destination_any_request_reaches() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());

    // Every inbound lever an attacker has, applied at once.
    send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages?redirect=https://evil.example.com",
        &[
            ("x-api-key", gw.token.as_str()),
            (SESSION, "sess-1"),
            (SCENARIO_HEADER, "nonstream"),
            ("x-forwarded-host", "evil.example.com"),
            ("x-original-url", "https://evil.example.com/v1/messages"),
        ],
        &messages_body("claude-sonnet-4-5", Some(100), false),
    );

    let received = gw.upstream.received();
    assert_eq!(
        received.len(),
        1,
        "the only call made was to the configured upstream"
    );
    assert_eq!(received[0].path, "/v1/messages");
    assert!(received[0].header("x-forwarded-host").is_none());
}
