//! The reserve → forward → settle lifecycle, end to end against a fake
//! upstream (HORO-1144).
//!
//! The property under test throughout is that **no provider call happens
//! without a successful reservation, and no reservation is left
//! outstanding after the response ends** — however it ends. The awkward
//! endings get the most attention: a stream cut off mid-flight, a client
//! that hangs up, an upstream that refuses, a response that reports no
//! usage at all. Those are where a budget boundary silently stops being
//! one.

#[path = "fake_upstream.rs"]
mod fake_upstream;

use fake_upstream::*;
use hyper::StatusCode;
use libra_governor_domain::{ResourceAmount, ResourceKind};

const SESSION: &str = "x-claude-code-session-id";

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn authorized_headers<'a>(token: &'a str, scenario: &'a str) -> Vec<(&'a str, &'a str)> {
    vec![
        ("x-api-key", token),
        (SESSION, "sess-1"),
        (SCENARIO_HEADER, scenario),
    ]
}

#[test]
fn a_streaming_request_is_reserved_relayed_verbatim_and_settled_at_the_exact_observed_usage() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());

    let response = send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages",
        &authorized_headers(&gw.token, "sse"),
        &messages_body("claude-sonnet-4-5", Some(1_000), true),
    );

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.decision(), Some("allowed"));
    assert_eq!(
        response.body, CANNED_SSE,
        "the stream must be relayed byte for byte — ping events and comment lines included"
    );
    assert!(response.body.contains("event: ping"));
    assert!(response.body.contains(": an SSE comment"));

    assert_eq!(gw.authority.authorize_count(), 1);
    wait_until(
        || !gw.authority.settlements().is_empty(),
        "the settlement that follows the end of the stream",
    );
    // 120 input + 0 cache + 0 cache-read + 30 output (the LAST cumulative
    // message_delta value, never the sum 1 + 9 + 30).
    assert_eq!(
        gw.authority.settlements(),
        vec![Some(ResourceAmount::Tokens(150))]
    );
    assert_eq!(gw.authority.release_count(), 0);
}

#[test]
fn a_non_streaming_request_settles_from_the_bodys_top_level_usage() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());

    let response = send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages",
        &authorized_headers(&gw.token, "nonstream"),
        &messages_body("claude-sonnet-4-5", Some(1_000), false),
    );

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, CANNED_JSON);
    wait_until(
        || !gw.authority.settlements().is_empty(),
        "the settlement for a buffered response",
    );
    assert_eq!(
        gw.authority.settlements(),
        vec![Some(ResourceAmount::Tokens(150))]
    );
}

#[test]
fn the_reservation_is_the_conservative_worst_case_not_the_eventual_actual() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());
    let body = messages_body("claude-sonnet-4-5", Some(1_000), true);
    let expected_reserved = (body.len() as f64 / 3.0).ceil() as u64 + 1_000;

    send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages",
        &authorized_headers(&gw.token, "sse"),
        &body,
    );

    let AuthorityCall::Authorize { amount, .. } = gw.authority.calls()[0].clone() else {
        panic!("the first authority call must be the reservation");
    };
    assert_eq!(amount, ResourceAmount::Tokens(expected_reserved));
    wait_until(|| !gw.authority.settlements().is_empty(), "settlement");
    assert!(
        gw.authority.settlements()[0].unwrap().as_f64() < amount.as_f64(),
        "settlement must correct the worst case downward, not merely confirm it"
    );
}

#[test]
fn a_request_with_no_positive_max_tokens_is_refused_before_any_reservation() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());

    for max_tokens in [None, Some(0)] {
        let response = send(
            &rt,
            gw.addr,
            "POST",
            "/v1/messages",
            &authorized_headers(&gw.token, "sse"),
            &messages_body("claude-sonnet-4-5", max_tokens, true),
        );
        assert_eq!(response.status, StatusCode::FORBIDDEN);
        assert_eq!(response.decision(), Some("unenforceable_request"));
    }
    assert_eq!(
        gw.authority.authorize_count(),
        0,
        "an unbounded request must never be reserved at zero"
    );
    assert!(gw.upstream.received().is_empty());
}

#[test]
fn a_request_with_no_session_binding_is_refused_as_task_unbound() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());

    let response = send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages",
        &[("x-api-key", gw.token.as_str()), (SCENARIO_HEADER, "sse")],
        &messages_body("claude-sonnet-4-5", Some(100), true),
    );

    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert_eq!(response.decision(), Some("task_unbound"));
    assert!(gw.upstream.received().is_empty());
}

#[test]
fn a_session_with_no_admitted_task_is_refused() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions {
        behaviour: StubBehaviour::NoSession,
        ..Default::default()
    });

    let response = send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages",
        &authorized_headers(&gw.token, "sse"),
        &messages_body("claude-sonnet-4-5", Some(100), true),
    );

    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert_eq!(response.decision(), Some("task_unbound"));
    assert_eq!(gw.authority.authorize_count(), 0);
    assert!(gw.upstream.received().is_empty());
}

#[test]
fn a_policy_denial_refuses_the_request_before_any_provider_spend() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions {
        behaviour: StubBehaviour::DenyPolicy,
        ..Default::default()
    });

    let response = send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages",
        &authorized_headers(&gw.token, "sse"),
        &messages_body("claude-sonnet-4-5", Some(100), true),
    );

    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert_eq!(response.decision(), Some("budget_exceeded"));
    assert!(
        gw.upstream.received().is_empty(),
        "the hard boundary is worthless unless the refusal precedes the call"
    );
    assert_eq!(
        response.status,
        StatusCode::FORBIDDEN,
        "never 429 — Claude Code retries a 429 with backoff, which would storm a boundary \
         that will refuse every time"
    );
    let body: serde_json::Value = serde_json::from_str(&response.body).unwrap();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "permission_error");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .starts_with("libra-governor: "));
}

#[test]
fn exhausted_headroom_refuses_even_though_the_completion_reserve_still_holds_capacity() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions {
        behaviour: StubBehaviour::DenyInsufficient,
        ..Default::default()
    });

    let response = send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages",
        &authorized_headers(&gw.token, "sse"),
        &messages_body("claude-sonnet-4-5", Some(100), true),
    );

    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert_eq!(response.decision(), Some("budget_exceeded"));
    assert!(gw.upstream.received().is_empty());

    let record = gw.recorder.records().pop().expect("a provenance row");
    let detail = record.decision_detail.expect("a denial detail");
    assert!(
        detail.contains("protected_reserve"),
        "the refusal must record that the protected reserve was what remained: {detail}"
    );
}

#[test]
fn a_ledger_failure_fails_closed_rather_than_letting_the_request_through() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions {
        behaviour: StubBehaviour::Error,
        ..Default::default()
    });

    let response = send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages",
        &authorized_headers(&gw.token, "sse"),
        &messages_body("claude-sonnet-4-5", Some(100), true),
    );

    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert_eq!(response.decision(), Some("no_budget"));
    assert!(
        gw.upstream.received().is_empty(),
        "an unavailable ledger is not a reason to relax the boundary"
    );
}

#[test]
fn an_approval_gated_request_still_proceeds_and_is_recorded_as_such() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions {
        behaviour: StubBehaviour::Grant {
            approval_required: true,
        },
        ..Default::default()
    });

    let response = send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages",
        &authorized_headers(&gw.token, "nonstream"),
        &messages_body("claude-sonnet-4-5", Some(100), false),
    );

    assert_eq!(
        response.status,
        StatusCode::OK,
        "the proxy has no channel to interrupt a human mid-request; approval is surfaced, \
         not enforced here"
    );
    let record = gw.recorder.records().into_iter().next().unwrap();
    assert_eq!(record.decision_detail.as_deref(), Some("approval_required"));
}

#[test]
fn an_unpriced_model_against_a_usd_budget_is_refused_rather_than_priced_by_guess() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions {
        resource_kind: ResourceKind::Usd,
        ..Default::default()
    });

    let response = send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages",
        &authorized_headers(&gw.token, "sse"),
        &messages_body("some-unlisted-model-v9", Some(100), true),
    );

    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert_eq!(response.decision(), Some("unpriced_model"));
    assert_eq!(gw.authority.authorize_count(), 0);
    assert!(gw.upstream.received().is_empty());
}

#[test]
fn a_priced_model_against_a_usd_budget_reserves_and_settles_in_cents() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions {
        resource_kind: ResourceKind::Usd,
        ..Default::default()
    });

    send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages",
        &authorized_headers(&gw.token, "sse"),
        &messages_body("claude-sonnet-4-5", Some(1_000), true),
    );

    let AuthorityCall::Authorize { amount, .. } = gw.authority.calls()[0].clone() else {
        panic!("the first authority call must be the reservation");
    };
    assert!(matches!(amount, ResourceAmount::UsdCents(_)));
    wait_until(|| !gw.authority.settlements().is_empty(), "settlement");
    let settled = gw.authority.settlements()[0].unwrap();
    assert!(matches!(settled, ResourceAmount::UsdCents(_)));
    assert!(
        settled.as_f64() <= amount.as_f64(),
        "settlement ({settled:?}) must never exceed the reservation ({amount:?}) that bounded it"
    );
}

#[test]
fn a_quota_percent_budget_is_refused_because_tokens_cannot_be_converted_into_it() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions {
        resource_kind: ResourceKind::QuotaPercent,
        ..Default::default()
    });

    let response = send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages",
        &authorized_headers(&gw.token, "sse"),
        &messages_body("claude-sonnet-4-5", Some(100), true),
    );

    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert_eq!(response.decision(), Some("unenforceable_request"));
    assert!(gw.upstream.received().is_empty());
}

#[test]
fn an_upstream_error_is_relayed_verbatim_and_the_reservation_is_released_in_full() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());

    let response = send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages",
        &authorized_headers(&gw.token, "server_error"),
        &messages_body("claude-sonnet-4-5", Some(100), true),
    );

    assert_eq!(response.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        response.body.contains("upstream exploded"),
        "the provider's own error is more useful than anything we could substitute"
    );
    wait_until(
        || gw.authority.release_count() == 1,
        "the full refund for a call that generated no tokens",
    );
    assert!(
        gw.authority.settlements().is_empty(),
        "a failed call must be released, not settled at its reserved amount"
    );
}

#[test]
fn an_upstream_rate_limit_is_relayed_as_itself_not_rewritten() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());

    let response = send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages",
        &authorized_headers(&gw.token, "overloaded"),
        &messages_body("claude-sonnet-4-5", Some(100), true),
    );

    assert_eq!(
        response.status,
        StatusCode::TOO_MANY_REQUESTS,
        "the gateway never invents a 429, but it must relay a real one so the agent's own \
         backoff still works"
    );
    wait_until(|| gw.authority.release_count() == 1, "the refund");
}

#[test]
fn an_upstream_401_triggers_exactly_one_credential_refresh_retry() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());

    let response = send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages",
        &authorized_headers(&gw.token, "unauthorized"),
        &messages_body("claude-sonnet-4-5", Some(100), true),
    );

    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    // The cooldown suppresses the refresh (the credential was resolved
    // moments ago at startup), so exactly one upstream call is made and
    // the provider's own 401 is relayed rather than retried into a loop.
    assert_eq!(
        gw.upstream.received().len(),
        1,
        "a persistently rejecting credential must not spawn a retry storm"
    );
    wait_until(|| gw.authority.release_count() == 1, "the refund");
}

#[test]
fn a_stream_cut_off_mid_flight_settles_at_the_last_observed_usage() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());

    let response = send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages",
        &authorized_headers(&gw.token, "truncated"),
        &messages_body("claude-sonnet-4-5", Some(1_000), true),
    );

    assert_eq!(response.status, StatusCode::OK);
    wait_until(|| !gw.authority.settlements().is_empty(), "settlement");
    // 120 input + 17 output — the figures the provider had reported by
    // the time the stream stopped. The provider did that work and billed
    // for it; refunding would under-count real spend.
    assert_eq!(
        gw.authority.settlements(),
        vec![Some(ResourceAmount::Tokens(137))]
    );
    let record = gw.recorder.records().pop().unwrap();
    assert!(record.usage_known);
}

#[test]
fn a_response_reporting_no_usage_settles_conservatively_at_the_reserved_amount() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());

    send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages",
        &authorized_headers(&gw.token, "no_usage"),
        &messages_body("claude-sonnet-4-5", Some(1_000), true),
    );

    wait_until(
        || !gw.authority.calls().is_empty() && gw.recorder.records().len() >= 2,
        "the settlement and its provenance row",
    );
    assert_eq!(
        gw.authority.settlements(),
        vec![None],
        "settling a zeroed usage would refund a request that certainly cost something"
    );
    let record = gw.recorder.records().pop().unwrap();
    assert!(!record.usage_known);
}

#[test]
fn a_bound_violation_is_recorded_rather_than_clamped_away() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());

    send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages",
        &authorized_headers(&gw.token, "over_bound"),
        &messages_body("claude-sonnet-4-5", Some(10), true),
    );

    wait_until(|| !gw.authority.settlements().is_empty(), "settlement");
    let record = gw.recorder.records().pop().unwrap();
    assert!(
        record.bound_violated,
        "the provider reported 999999 output tokens against max_tokens=10; a violated \
         assumption must be visible, not absorbed"
    );
    assert_eq!(record.output_tokens, Some(999_999));
}

#[test]
fn a_client_retry_is_a_fresh_reservation_not_a_double_settlement() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());
    let body = messages_body("claude-sonnet-4-5", Some(1_000), false);

    for _ in 0..2 {
        send(
            &rt,
            gw.addr,
            "POST",
            "/v1/messages",
            &authorized_headers(&gw.token, "nonstream"),
            &body,
        );
    }

    wait_until(|| gw.authority.settlements().len() == 2, "both settlements");
    assert_eq!(
        gw.authority.authorize_count(),
        2,
        "a client retry is a second real upstream call and is charged again"
    );

    let keys: Vec<String> = gw
        .authority
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            AuthorityCall::Authorize {
                idempotency_key, ..
            } => Some(idempotency_key),
            _ => None,
        })
        .collect();
    assert_ne!(
        keys[0], keys[1],
        "each inbound request gets its own idempotency key; sharing one would make the second \
         call silently free"
    );
    assert!(keys.iter().all(|k| k.starts_with("gw:")));

    // The hazard that matters is double-SETTLING one reservation.
    let settled_ids: Vec<_> = gw
        .authority
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            AuthorityCall::Settle { reservation_id, .. } => Some(reservation_id),
            _ => None,
        })
        .collect();
    assert_eq!(settled_ids.len(), 2);
    assert_ne!(settled_ids[0], settled_ids[1]);
}

#[test]
fn every_terminal_transition_writes_exactly_one_provenance_row_carrying_its_pricing_version() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());

    // A refusal.
    send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages",
        &authorized_headers(&gw.token, "sse"),
        &messages_body("claude-sonnet-4-5", None, true),
    );
    let records = gw.recorder.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].decision, "unenforceable_request");
    assert_eq!(
        records[0].pricing_version,
        libra_governor_gateway::pricing::PRICING_VERSION
    );
    assert_eq!(records[0].terminal_state, "rejected_before_upstream");

    // A completed request writes the response-path row plus the
    // settlement row from the pump task.
    send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages",
        &authorized_headers(&gw.token, "nonstream"),
        &messages_body("claude-sonnet-4-5", Some(100), false),
    );
    wait_until(
        || gw.recorder.records().len() >= 3,
        "the settled request's provenance row",
    );
    let settled = gw
        .recorder
        .records()
        .into_iter()
        .find(|r| r.settled_amount.is_some())
        .expect("a settled row");
    assert_eq!(settled.model.as_deref(), Some("claude-sonnet-4-5"));
    assert_eq!(settled.max_tokens, Some(100));
    assert_eq!(settled.tier, "gateway_metered");
    assert_eq!(settled.upstream_status, Some(200));
}

#[test]
fn a_provenance_row_never_carries_a_body_or_a_credential() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());

    send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages",
        &authorized_headers(&gw.token, "nonstream"),
        &messages_body("claude-sonnet-4-5", Some(100), false),
    );
    wait_until(|| !gw.authority.settlements().is_empty(), "settlement");

    for record in gw.recorder.records() {
        let rendered = format!("{record:?}");
        assert!(
            !rendered.contains("hello"),
            "the request body's content reached a provenance row: {rendered}"
        );
        assert!(
            !rendered.contains(&gw.token),
            "the local capability token reached a provenance row"
        );
        assert!(
            !rendered.contains(FAKE_UPSTREAM_KEY),
            "the upstream credential reached a provenance row"
        );
    }
}
