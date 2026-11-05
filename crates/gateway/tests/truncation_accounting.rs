//! HORO-2286 characterization: what the task-budget gateway path books when
//! a response does not run to a clean `message_stop`.
//!
//! These tests PIN CURRENT BEHAVIOUR; they do not endorse it. The ticket
//! (and `docs/design/s11-gateway-governor-held-hard-cap.md` R6) suspects
//! that (1) a cut-off stream is settled at the partial usage observed so
//! far rather than at the reservation, and (2) a reservation that expires
//! while its request is still in flight frees its capacity without a
//! charge. This file records, at the `SpendAuthority` boundary, exactly
//! what the gateway tells the ledger. The ledger's own reaction to an
//! expiry is pinned in `crates/ledger/tests/gateway_expiry_accounting.rs`.

#[path = "fake_upstream.rs"]
mod fake_upstream;

use std::time::Duration;

use fake_upstream::*;
use http_body_util::BodyExt;
use libra_governor_domain::ResourceAmount;

const SESSION: &str = "x-claude-code-session-id";

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn headers<'a>(token: &'a str, scenario: &'a str) -> Vec<(&'a str, &'a str)> {
    vec![
        ("x-api-key", token),
        (SESSION, "sess-1"),
        (SCENARIO_HEADER, scenario),
    ]
}

/// The worst-case amount the gateway reserved for the single request.
fn reserved(gw: &TestGateway) -> ResourceAmount {
    gw.authority
        .calls()
        .into_iter()
        .find_map(|c| match c {
            AuthorityCall::Authorize { amount, .. } => Some(amount),
            _ => None,
        })
        .expect("a reservation was taken")
}

/// One streaming `/v1/messages` request to the test gateway: the response
/// and the pieces that keep its connection alive.
struct OpenStream {
    status: hyper::StatusCode,
    body: hyper::body::Incoming,
    sender: hyper::client::conn::http1::SendRequest<http_body_util::Full<bytes::Bytes>>,
    conn_task: tokio::task::JoinHandle<Result<(), hyper::Error>>,
}

async fn open_stream(addr: std::net::SocketAddr, token: String, scenario: String) -> OpenStream {
    let body = messages_body("claude-sonnet-4-5", Some(1_000), true);
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (mut sender, conn) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .unwrap();
    let conn_task = tokio::spawn(conn);
    let request = hyper::Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("host", addr.to_string())
        .header("x-api-key", token)
        .header(SESSION, "sess-1")
        .header(SCENARIO_HEADER, scenario)
        .body(http_body_util::Full::new(bytes::Bytes::from(body)))
        .unwrap();
    let response = sender.send_request(request).await.unwrap();
    OpenStream {
        status: response.status(),
        body: response.into_body(),
        sender,
        conn_task,
    }
}

/// Sends a request, reads response headers plus `frames` body frames, then
/// drops the connection without reading further. This is a client hang-up
/// (Claude Code's ESC), as distinct from `send`, which reads to the end.
fn request_then_hang_up(
    rt: &tokio::runtime::Runtime,
    gw: &TestGateway,
    scenario: &str,
    frames: usize,
) {
    let (addr, token, scenario) = (gw.addr, gw.token.clone(), scenario.to_string());
    rt.block_on(async move {
        let mut open = open_stream(addr, token, scenario).await;
        assert_eq!(open.status, hyper::StatusCode::OK);
        for _ in 0..frames {
            open.body.frame().await.expect("a frame").expect("frame ok");
        }
        drop(open.body);
        drop(open.sender);
        open.conn_task.abort();
    });
}

/// Reads the response to its end, tolerating the read error a reset
/// upstream produces.
fn read_until_the_stream_ends_or_fails(
    rt: &tokio::runtime::Runtime,
    gw: &TestGateway,
    scenario: &str,
) {
    let (addr, token, scenario) = (gw.addr, gw.token.clone(), scenario.to_string());
    rt.block_on(async move {
        let mut open = open_stream(addr, token, scenario).await;
        while let Some(Ok(_)) = open.body.frame().await {}
    });
}

// ---------------------------------------------------------------------
// (i) stream cut after message_start only
// ---------------------------------------------------------------------

#[test]
fn a_stream_reset_right_after_message_start_settles_at_input_plus_one_token_not_the_reservation() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());

    // The body error surfaces to the client as a failed read; only the
    // gateway's settlement is under test.
    read_until_the_stream_ends_or_fails(&rt, &gw, "cut_after_start");

    wait_until(|| !gw.authority.settlements().is_empty(), "settlement");
    // message_start reports input_tokens=120 and output_tokens=1 (a
    // placeholder). That is all the provider had said when the stream died.
    assert_eq!(
        gw.authority.settlements(),
        vec![Some(ResourceAmount::Tokens(121))]
    );
    assert_eq!(gw.authority.release_count(), 0);
    // Contrast with the ticket's expectation: the reservation is the
    // modelled worst case (input estimate + max_tokens), far above 121.
    let ResourceAmount::Tokens(reserved_tokens) = reserved(&gw) else {
        panic!("token budget");
    };
    assert_eq!(
        reserved_tokens, 1_036,
        "36 estimated input tokens + max_tokens 1000"
    );
    wait_until(|| gw.recorder.records().len() == 1, "provenance row");
    let record = gw.recorder.records().pop().unwrap();
    assert_eq!(record.terminal_state, "stream_aborted");
    assert!(
        record.usage_known,
        "the row claims exact usage although the stream was cut"
    );
    assert_eq!(record.output_tokens, Some(1));
}

#[test]
fn a_stream_that_ends_cleanly_after_message_start_with_no_stop_is_labelled_completed_cleanly() {
    // An upstream EOF with no message_stop is indistinguishable from a
    // finished body at the pump (`None => break`), so it is settled at the
    // partial usage and labelled `completed_cleanly`.
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());

    send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages",
        &headers(&gw.token, "truncated"),
        &messages_body("claude-sonnet-4-5", Some(1_000), true),
    );

    wait_until(|| gw.recorder.records().len() == 1, "provenance row");
    assert_eq!(
        gw.authority.settlements(),
        vec![Some(ResourceAmount::Tokens(137))]
    );
    let record = gw.recorder.records().pop().unwrap();
    assert_eq!(record.terminal_state, "completed_cleanly");
}

// ---------------------------------------------------------------------
// (ii) stream reset mid-output
// ---------------------------------------------------------------------

#[test]
fn a_stream_reset_mid_output_settles_at_the_partial_usage_far_below_the_reservation() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());

    read_until_the_stream_ends_or_fails(&rt, &gw, "cut_mid_output");

    wait_until(|| !gw.authority.settlements().is_empty(), "settlement");
    // 120 input + the last cumulative output figure seen (250).
    assert_eq!(
        gw.authority.settlements(),
        vec![Some(ResourceAmount::Tokens(370))]
    );
    let ResourceAmount::Tokens(reserved_tokens) = reserved(&gw) else {
        panic!("token budget");
    };
    assert_eq!(reserved_tokens, 1_036);
    wait_until(|| gw.recorder.records().len() == 1, "provenance row");
    let record = gw.recorder.records().pop().unwrap();
    assert_eq!(record.terminal_state, "stream_aborted");
    assert_eq!(record.output_tokens, Some(250));
}

// ---------------------------------------------------------------------
// (iii) client drop
// ---------------------------------------------------------------------

#[test]
fn a_client_hang_up_mid_stream_settles_at_the_usage_seen_when_the_hang_up_was_noticed() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());

    // Read only the first frame (message_start), then vanish. The upstream
    // would go on to report 17, then 40 output tokens and a clean stop.
    request_then_hang_up(&rt, &gw, "slow_stream", 1);

    wait_until(|| !gw.authority.settlements().is_empty(), "settlement");
    // The pump sees the delta(17) frame, taps it, fails to forward it, and
    // settles: 120 + 17. It never reads delta(40), so any work the provider
    // continues to do after the hang-up is not booked.
    assert_eq!(
        gw.authority.settlements(),
        vec![Some(ResourceAmount::Tokens(137))]
    );
    assert_eq!(gw.authority.release_count(), 0);
    wait_until(|| gw.recorder.records().len() == 1, "provenance row");
    let record = gw.recorder.records().pop().unwrap();
    assert_eq!(record.terminal_state, "client_disconnected");
    assert_eq!(record.output_tokens, Some(17));
}

#[test]
fn a_client_that_reads_the_whole_slow_stream_is_settled_at_the_final_usage() {
    // Control for the hang-up test: same upstream, no hang-up.
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());

    send(
        &rt,
        gw.addr,
        "POST",
        "/v1/messages",
        &headers(&gw.token, "slow_stream"),
        &messages_body("claude-sonnet-4-5", Some(1_000), true),
    );

    wait_until(|| !gw.authority.settlements().is_empty(), "settlement");
    assert_eq!(
        gw.authority.settlements(),
        vec![Some(ResourceAmount::Tokens(160))]
    );
}

// ---------------------------------------------------------------------
// (iv) upstream stall until the reservation would expire
// ---------------------------------------------------------------------

#[test]
fn a_stalled_upstream_leaves_the_reservation_open_with_no_settle_release_or_renewal() {
    let rt = runtime();
    let gw = start_test_gateway(TestGatewayOptions::default());

    // Read message_start, then the upstream goes silent without closing.
    // The harness cannot run for the 600 s TTL, so this pins the gateway
    // half: while a forwarded request is stalled, the gateway holds the
    // original reservation and does nothing to it. (There is no response
    // timeout and no TTL renewal on the task-budget path; see
    // GatewayConfig::upstream_connect_timeout docs.) Whether the ledger
    // charges the expired row is pinned in the ledger crate.
    request_then_stay_connected(&rt, &gw, "stall_after_start");

    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(gw.authority.authorize_count(), 1);
    assert!(gw.authority.settlements().is_empty());
    assert_eq!(gw.authority.release_count(), 0);
    assert_eq!(gw.upstream.received().len(), 1, "the request WAS forwarded");
}

/// Sends a request, reads headers and the first frame, and returns with the
/// connection still open (held by a detached task for the test's lifetime).
fn request_then_stay_connected(rt: &tokio::runtime::Runtime, gw: &TestGateway, scenario: &str) {
    let (addr, token, scenario) = (gw.addr, gw.token.clone(), scenario.to_string());
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    rt.spawn(async move {
        let mut open = open_stream(addr, token, scenario).await;
        open.body.frame().await.expect("a frame").expect("frame ok");
        ready_tx.send(()).unwrap();
        tokio::time::sleep(Duration::from_secs(60)).await;
    });
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("first frame relayed");
}
