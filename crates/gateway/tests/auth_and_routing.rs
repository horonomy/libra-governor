//! Local authorization, credential custody, and the closed route table,
//! against a real gateway in front of a fake upstream (HORO-1144).
//!
//! These are the checks that run *before* any budget is consulted, in the
//! order the state machine runs them. The ordering is itself asserted:
//! a `Host` mismatch must be refused before routing, because a request
//! naming a destination it did not dial is proxy abuse whatever path it
//! carries.

#[path = "fake_upstream.rs"]
mod fake_upstream;

use fake_upstream::*;
use hyper::StatusCode;
use libra_governor_domain::ResourceKind;

const SESSION: &str = "x-claude-code-session-id";

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn a_request_with_no_credential_is_refused_and_never_reaches_the_upstream() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());

    let response = send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages",
        &[(SESSION, "sess-1")],
        &messages_body("claude-sonnet-4-5", Some(100), true),
    );

    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    assert_eq!(response.decision(), Some("unauthorized"));
    assert!(gw.upstream.received().is_empty());
    assert_eq!(gw.authority.authorize_count(), 0);
}

#[test]
fn a_wrong_capability_token_is_refused() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());

    let response = send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages",
        &[
            ("x-api-key", "sk-fake-not-the-local-token"),
            (SESSION, "sess-1"),
        ],
        &messages_body("claude-sonnet-4-5", Some(100), true),
    );

    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    assert!(gw.upstream.received().is_empty());
}

#[test]
fn the_capability_token_is_accepted_under_either_header_spelling() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());

    for (name, value) in [
        ("x-api-key".to_string(), gw.token.clone()),
        ("authorization".to_string(), format!("Bearer {}", gw.token)),
    ] {
        let response = send(
            &rt,
            gw.addr,
            "POST",
            "/v1/messages",
            &[
                (name.as_str(), value.as_str()),
                (SESSION, "sess-1"),
                (SCENARIO_HEADER, "nonstream"),
            ],
            &messages_body("claude-sonnet-4-5", Some(100), false),
        );
        assert_eq!(response.status, StatusCode::OK, "header {name} was refused");
    }
}

#[test]
fn disagreeing_dual_auth_headers_are_refused_without_any_upstream_call() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());

    let response = send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages",
        &[
            ("authorization", "Bearer sk-fake-one-value"),
            ("x-api-key", "sk-fake-a-different-value"),
            (SESSION, "sess-1"),
        ],
        &messages_body("claude-sonnet-4-5", Some(100), true),
    );

    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(response.decision(), Some("ambiguous_credential"));
    assert!(
        gw.upstream.received().is_empty(),
        "an ambiguous credential state must be refused before any provider spend"
    );
}

#[test]
fn matching_dual_auth_headers_are_not_ambiguous() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());
    let bearer = format!("Bearer {}", gw.token);

    let response = send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages",
        &[
            ("authorization", bearer.as_str()),
            ("x-api-key", gw.token.as_str()),
            (SESSION, "sess-1"),
            (SCENARIO_HEADER, "nonstream"),
        ],
        &messages_body("claude-sonnet-4-5", Some(100), false),
    );

    assert_eq!(response.status, StatusCode::OK);
}

#[test]
fn a_host_header_naming_another_destination_is_refused_before_routing() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());

    // The path is also unroutable. If routing ran first this would be a
    // 404; the 421 is what proves the Host check is genuinely enforced
    // first rather than merely documented as being first.
    let response = send(
        &rt,
        gw.addr,
        "POST",
        "/definitely/not/a/route",
        &[
            ("host", "api.anthropic.com"),
            ("x-api-key", gw.token.as_str()),
        ],
        b"{}",
    );

    assert_eq!(response.status, StatusCode::MISDIRECTED_REQUEST);
    assert_eq!(response.decision(), Some("host_mismatch"));
    assert!(gw.upstream.received().is_empty());
}

#[test]
fn a_path_outside_the_closed_route_table_is_refused() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());

    for path in [
        "/v1/complete",
        "/v1/models",
        "/v1/messages/../../admin",
        "/v1/messages/",
        "/",
    ] {
        let response = send(
            &rt,
            gw.addr,
            "POST",
            path,
            &[("x-api-key", gw.token.as_str()), (SESSION, "sess-1")],
            b"{}",
        );
        assert_eq!(
            response.status,
            StatusCode::NOT_FOUND,
            "{path} must not be proxied"
        );
        assert_eq!(response.decision(), Some("route_not_found"));
    }
    assert!(
        gw.upstream.received().is_empty(),
        "an unroutable path must never produce an upstream call"
    );
}

#[test]
fn the_connectivity_probe_is_answered_locally_and_costs_nothing() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());

    let response = send(
        &rt,
        gw.addr,
        "GET",
        "/api/hello",
        &[("x-api-key", gw.token.as_str())],
        b"",
    );

    assert_eq!(response.status, StatusCode::OK);
    assert!(
        gw.upstream.received().is_empty(),
        "a connectivity probe must work even when the provider is unreachable"
    );
    assert_eq!(gw.authority.authorize_count(), 0);
}

#[test]
fn the_upstream_receives_the_governor_held_credential_and_never_the_local_token() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());

    send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages",
        &[
            ("x-api-key", gw.token.as_str()),
            (SESSION, "sess-1"),
            (SCENARIO_HEADER, "nonstream"),
        ],
        &messages_body("claude-sonnet-4-5", Some(100), false),
    );

    let received = gw.upstream.received();
    assert_eq!(received.len(), 1);
    assert_eq!(
        received[0].header("x-api-key"),
        Some(FAKE_UPSTREAM_KEY),
        "the gateway must substitute the credential the agent never holds"
    );
    for (name, value) in &received[0].headers {
        assert!(
            !value.contains(&gw.token),
            "the local capability token leaked upstream in header {name}"
        );
    }
    assert!(
        received[0].header("authorization").is_none(),
        "the inbound Authorization header must be stripped, not forwarded alongside the \
         substituted credential"
    );
}

#[test]
fn forwarding_chain_and_inbound_host_headers_are_dropped() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());

    send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages",
        &[
            ("x-api-key", gw.token.as_str()),
            (SESSION, "sess-1"),
            (SCENARIO_HEADER, "nonstream"),
            ("x-forwarded-for", "203.0.113.9"),
            ("x-forwarded-host", "evil.example.com"),
            ("x-forwarded-proto", "http"),
            ("forwarded", "for=203.0.113.9;host=evil.example.com"),
            ("x-real-ip", "203.0.113.9"),
        ],
        &messages_body("claude-sonnet-4-5", Some(100), false),
    );

    let received = gw.upstream.received();
    assert_eq!(received.len(), 1);
    for header in [
        "x-forwarded-for",
        "x-forwarded-host",
        "x-forwarded-proto",
        "forwarded",
        "x-real-ip",
    ] {
        assert!(
            received[0].header(header).is_none(),
            "{header} must not be forwarded — a downstream could use it to spoof provenance"
        );
    }
    assert_eq!(
        received[0].header("host"),
        Some(format!("127.0.0.1:{}", gw.upstream.addr.port()).as_str()),
        "the outbound Host must name the configured upstream, never the inbound one"
    );
}

#[test]
fn ordinary_headers_and_the_query_string_are_forwarded_unchanged() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());

    send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages?beta=true&other=1",
        &[
            ("x-api-key", gw.token.as_str()),
            (SESSION, "sess-1"),
            (SCENARIO_HEADER, "nonstream"),
            ("anthropic-version", "2023-06-01"),
            ("anthropic-beta", "some-beta-flag"),
        ],
        &messages_body("claude-sonnet-4-5", Some(100), false),
    );

    let received = gw.upstream.received();
    assert_eq!(received[0].header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(received[0].header("anthropic-beta"), Some("some-beta-flag"));
    assert_eq!(received[0].query.as_deref(), Some("beta=true&other=1"));
    assert_eq!(
        received[0].path, "/v1/messages",
        "the outbound path is the route's own constant, never the inbound string"
    );
}

#[test]
fn count_tokens_is_forwarded_without_reserving_anything() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());

    let response = send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages/count_tokens",
        &[
            ("x-api-key", gw.token.as_str()),
            (SESSION, "sess-1"),
            (SCENARIO_HEADER, "count_tokens"),
        ],
        &messages_body("claude-sonnet-4-5", None, false),
    );

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(gw.upstream.received().len(), 1);
    assert_eq!(
        gw.authority.authorize_count(),
        0,
        "count_tokens costs nothing — reserving against it would deny real work for no reason"
    );
}

#[test]
fn a_request_body_over_the_cap_is_refused_before_the_upstream_is_dialled() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions {
        max_request_bytes: 1_024,
        ..Default::default()
    });

    let mut body = messages_body("claude-sonnet-4-5", Some(100), false);
    body.extend(std::iter::repeat_n(b' ', 4_096));

    let response = send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages",
        &[("x-api-key", gw.token.as_str()), (SESSION, "sess-1")],
        &body,
    );

    assert_eq!(response.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(response.decision(), Some("payload_too_large"));
    assert!(gw.upstream.received().is_empty());
}

#[test]
fn pass_through_subscription_mode_forwards_the_callers_own_credential_unchanged() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions {
        credential_mode:
            libra_governor_gateway::config::GatewayCredentialMode::PassThroughSubscription,
        resource_kind: ResourceKind::Tokens,
        ..Default::default()
    });

    send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages",
        &[
            ("authorization", "Bearer sk-fake-callers-own-subscription"),
            (SESSION, "sess-1"),
            (SCENARIO_HEADER, "nonstream"),
        ],
        &messages_body("claude-sonnet-4-5", Some(100), false),
    );

    let received = gw.upstream.received();
    assert_eq!(received.len(), 1);
    assert_eq!(
        received[0].header("authorization"),
        Some("Bearer sk-fake-callers-own-subscription"),
        "in pass-through mode the Governor takes no custody — the caller's credential is \
         relayed exactly as it arrived"
    );
}

#[test]
fn every_response_carries_a_request_id_for_correlation() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());

    let refused = send(&rt, gw.addr, "POST", "/nope", &[], b"{}");
    assert!(refused.header("x-libra-request-id").is_some());

    let allowed = send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages",
        &[
            ("x-api-key", gw.token.as_str()),
            (SESSION, "sess-1"),
            (SCENARIO_HEADER, "nonstream"),
        ],
        &messages_body("claude-sonnet-4-5", Some(100), false),
    );
    assert!(allowed.header("x-libra-request-id").is_some());
}
