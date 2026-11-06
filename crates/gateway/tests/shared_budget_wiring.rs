//! HORO-2274 (S11e-1): how the proxy drives the shared monthly budget, against
//! a stub `SharedBudgetAuthority` and the fake upstream. The ledger-backed
//! authority is exercised end to end in `crates/daemon/tests`.
//!
//! The property throughout: a request the shared budget refuses never reaches
//! the provider and leaves no reservation behind, in either budget.

#[path = "fake_upstream.rs"]
mod fake_upstream;

use std::sync::Arc;

use fake_upstream::*;
use hyper::StatusCode;
use libra_governor_gateway::authority::{SharedDecision, SharedDenial, SharedMode};
use libra_governor_gateway::cost::{EnforcementGap, SettleFlags};
use libra_governor_gateway::proxy::renewal_period;

const SESSION: &str = "x-claude-code-session-id";

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn gateway(mode: SharedMode, script: SharedScript) -> TestGateway {
    start_test_gateway(TestGatewayOptions {
        shared: Some(Arc::new(StubSharedAuthority::new(mode, script))),
        ..Default::default()
    })
}

fn post(gw: &TestGateway, scenario: &str, body: &[u8]) -> TestResponse {
    send(
        &runtime(),
        gw.addr,
        "POST",
        "/v1/messages",
        &[
            ("x-api-key", &gw.token),
            (SESSION, "sess-1"),
            (SCENARIO_HEADER, scenario),
        ],
        body,
    )
}

fn plain_body() -> Vec<u8> {
    messages_body("claude-sonnet-4-5", Some(1_000), true)
}

fn shared(gw: &TestGateway) -> &StubSharedAuthority {
    gw.shared
        .as_deref()
        .expect("the test attached a shared authority")
}

/// A refusal reached neither the provider nor either budget's rows.
fn assert_refused_cleanly(gw: &TestGateway, response: &TestResponse, decision: &str) {
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert_eq!(response.decision(), Some(decision));
    assert!(gw.upstream.received().is_empty(), "the provider was called");
    assert!(gw.authority.settlements().is_empty());
    assert!(shared(gw).settles().is_empty());
    assert!(shared(gw).releases().is_empty());
}

#[test]
fn without_a_shared_authority_the_gateway_is_a_task_budget_gateway() {
    let gw = start_test_gateway(TestGatewayOptions::default());
    let response = post(&gw, "sse", &plain_body());
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(gw.authority.authorize_count(), 1);
    assert!(gw.shared.is_none());
}

#[test]
fn a_shared_budget_that_is_not_consulted_changes_nothing() {
    let gw = gateway(SharedMode::NotConsulted, SharedScript::Grant);
    let response = post(&gw, "sse", &plain_body());
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(gw.authority.authorize_count(), 1);
    assert!(shared(&gw).reserves().is_empty());
    wait_until(|| !gw.authority.settlements().is_empty(), "task settlement");
    assert!(shared(&gw).settles().is_empty());
}

#[test]
fn an_enforced_request_reserves_both_budgets_and_settles_the_hold_it_was_granted() {
    let gw = gateway(SharedMode::Enforce, SharedScript::Grant);
    let body = plain_body();
    let response = post(&gw, "sse", &body);
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.decision(), Some("allowed"));

    let SharedCall::Reserve {
        session_id,
        model,
        body_bytes,
        max_tokens,
        image_count,
        fast_requested,
        request_id,
    } = shared(&gw).reserves()[0].clone()
    else {
        unreachable!()
    };
    assert_eq!(session_id, "sess-1");
    assert_eq!(model, "claude-sonnet-4-5");
    assert_eq!(body_bytes, body.len(), "the full body length, not a subset");
    assert_eq!(max_tokens, Some(1_000));
    assert_eq!(image_count, 0);
    assert!(!fast_requested);
    assert_eq!(
        Some(request_id.as_str()),
        response.header("x-libra-request-id")
    );
    assert_eq!(gw.authority.authorize_count(), 1);

    wait_until(
        || !shared(&gw).settles().is_empty(),
        "the shared settlement",
    );
    let SharedCall::Settle {
        hold,
        observed,
        completed_cleanly,
        saw_message_start,
    } = shared(&gw).settles()[0].clone()
    else {
        unreachable!()
    };
    // The hold is handed back exactly as granted: model, max_tokens and the
    // input bound are what the reservation was priced with.
    assert_eq!(hold.model, "claude-sonnet-4-5");
    assert_eq!(hold.max_tokens, 1_000);
    assert_eq!(hold.input_bound_tokens, 1_234);
    assert_eq!(hold.reserved_usd_cents, 4_242);
    assert!(completed_cleanly && saw_message_start);
    assert_eq!(observed.map(|u| u.output_tokens), Some(30));
    assert!(shared(&gw).releases().is_empty());
}

#[test]
fn a_non_streaming_body_that_arrives_whole_settles_as_complete() {
    let gw = gateway(SharedMode::Enforce, SharedScript::Grant);
    post(
        &gw,
        "nonstream",
        &messages_body("claude-sonnet-4-5", Some(1_000), false),
    );
    wait_until(
        || !shared(&gw).settles().is_empty(),
        "the shared settlement",
    );
    let SharedCall::Settle {
        completed_cleanly, ..
    } = shared(&gw).settles()[0].clone()
    else {
        unreachable!()
    };
    assert!(completed_cleanly);
}

#[test]
fn an_exhausted_cap_refuses_before_the_provider_and_gives_the_task_reservation_back() {
    let gw = gateway(
        SharedMode::Enforce,
        SharedScript::Decide(SharedDecision::Denied(SharedDenial::CapExhausted {
            remaining_usd_cents: 7,
        })),
    );
    let response = post(&gw, "sse", &plain_body());
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert_eq!(response.decision(), Some("shared_budget_exceeded"));
    assert!(gw.upstream.received().is_empty());
    assert_eq!(
        gw.authority.authorize_count(),
        1,
        "the task row was taken first"
    );
    wait_until(
        || gw.authority.release_count() == 1,
        "the task reservation's release",
    );
    assert!(gw.authority.settlements().is_empty());
}

#[test]
fn shared_denials_map_to_their_decision_and_release_the_task_reservation() {
    let cases = [
        (
            SharedScript::Decide(SharedDecision::Denied(SharedDenial::Unattributable)),
            "shared_budget_unattributable",
        ),
        (
            SharedScript::Decide(SharedDecision::Denied(SharedDenial::Unavailable)),
            "shared_budget_unavailable",
        ),
        (
            SharedScript::Decide(SharedDecision::Denied(SharedDenial::Unenforceable(
                EnforcementGap::UnboundedRequest,
            ))),
            "unenforceable_request",
        ),
        (SharedScript::Error, "shared_budget_unavailable"),
    ];
    for (script, decision) in cases {
        let gw = gateway(SharedMode::Enforce, script);
        let response = post(&gw, "sse", &plain_body());
        assert_eq!(response.decision(), Some(decision));
        assert_eq!(response.status, StatusCode::FORBIDDEN);
        assert!(gw.upstream.received().is_empty());
        wait_until(|| gw.authority.release_count() == 1, "the task release");
    }
}

#[test]
fn an_unavailable_shared_budget_refuses_before_any_reservation() {
    let gw = gateway(SharedMode::Unavailable, SharedScript::Grant);
    let response = post(&gw, "sse", &plain_body());
    assert_refused_cleanly(&gw, &response, "shared_budget_unavailable");
    assert_eq!(gw.authority.authorize_count(), 0);
    assert!(shared(&gw).reserves().is_empty());
}

#[test]
fn a_request_the_shared_budget_cannot_bound_makes_zero_authorize_calls() {
    let gw = gateway(SharedMode::Enforce, SharedScript::Grant);
    let body = serde_json::json!({
        "model": "claude-sonnet-4-5", "max_tokens": 1000, "stream": true,
        "tools": [{"type": "web_search_20250305", "name": "web_search"}],
        "messages": [{"role": "user", "content": "hi"}]
    })
    .to_string()
    .into_bytes();
    let response = post(&gw, "sse", &body);
    assert_refused_cleanly(&gw, &response, "unenforceable_request");
    assert_eq!(
        gw.authority.authorize_count(),
        0,
        "no task reservation either"
    );
    assert!(shared(&gw).reserves().is_empty());
}

#[test]
fn a_request_with_no_session_header_reserves_nothing_in_either_budget() {
    let gw = gateway(SharedMode::Enforce, SharedScript::Grant);
    let response = send(
        &runtime(),
        gw.addr,
        "POST",
        "/v1/messages",
        &[("x-api-key", &gw.token), (SCENARIO_HEADER, "sse")],
        &plain_body(),
    );
    assert_refused_cleanly(&gw, &response, "task_unbound");
    assert_eq!(gw.authority.authorize_count(), 0);
    assert!(shared(&gw).reserves().is_empty());
}

#[test]
fn an_upstream_failure_releases_both_budgets() {
    for scenario in ["server_error", "overloaded_529", "overloaded"] {
        let gw = gateway(SharedMode::Enforce, SharedScript::Grant);
        post(&gw, scenario, &plain_body());
        wait_until(|| gw.authority.release_count() == 1, "the task release");
        wait_until(|| shared(&gw).releases().len() == 1, "the shared release");
        assert!(gw.authority.settlements().is_empty(), "{scenario}");
        assert!(shared(&gw).settles().is_empty(), "{scenario}");
    }
}

#[test]
fn a_truncated_stream_hands_the_authority_the_raw_facts_and_counts_its_flags() {
    let gw = gateway(SharedMode::Enforce, SharedScript::Grant);
    *shared(&gw).settle_flags.lock().unwrap() = SettleFlags {
        truncated_settle: true,
        input_bound_violated: true,
    };
    post(&gw, "cut_mid_output", &plain_body());
    wait_until(
        || !shared(&gw).settles().is_empty(),
        "the shared settlement",
    );
    let SharedCall::Settle {
        observed,
        completed_cleanly,
        saw_message_start,
        ..
    } = shared(&gw).settles()[0].clone()
    else {
        unreachable!()
    };
    assert!(!completed_cleanly && saw_message_start);
    assert_eq!(observed.map(|u| u.output_tokens), Some(250));
    wait_until(
        || {
            let s = gw.stats.snapshot();
            s.truncated_settles == 1 && s.input_bound_violations == 1
        },
        "the flag counters",
    );
}

#[test]
fn a_stream_renews_no_more_often_than_every_thirty_seconds() {
    assert_eq!(renewal_period(600).as_secs(), 200);
    assert_eq!(renewal_period(60).as_secs(), 30);
    assert_eq!(renewal_period(0).as_secs(), 30);
}
