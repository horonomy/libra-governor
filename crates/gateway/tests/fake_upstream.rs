//! A local stand-in for the Anthropic Messages API, plus the stub
//! `SpendAuthority`/`RequestRecorder` the gateway's integration tests
//! drive it with (HORO-1144).
//!
//! # No real provider traffic, ever
//!
//! Nothing in this crate's tests reaches the real API, and no real
//! credential exists anywhere in them. Every credential-shaped value is a
//! literal `sk-fake-...`, per `SECURITY.md`. The scenarios below — clean
//! SSE with `ping` events, a non-streaming body, `401`, `500`, a `3xx`,
//! and a stream cut off mid-flight — are the response shapes the gateway
//! must handle, reproduced locally so they can be asserted on
//! deterministically rather than hoped for.
//!
//! # Why this file is also a test target
//!
//! Every file directly under `tests/` compiles as its own test binary, so
//! this one carries self-tests proving the fake behaves as the other
//! suites assume. They include it with `#[path = "fake_upstream.rs"] mod
//! fake_upstream;`.

#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Request, Response, StatusCode};
use libra_governor_domain::{Headroom, ReservationId, ResourceAmount, ResourceKind, TaskId};
use libra_governor_gateway::authority::{
    AuthorityError, BudgetContext, SpendAuthority, SpendDecision, SpendDenial, SpendRequest,
};
use libra_governor_gateway::proxy::{GatewayRequestRecord, RequestRecorder};

/// The header a test sets to choose which canned response it wants. It
/// rides through the gateway untouched, which incidentally also proves
/// that ordinary headers are forwarded.
pub const SCENARIO_HEADER: &str = "x-fake-scenario";

/// The credential the fake upstream accepts. Fake by construction — see
/// the module docs and `SECURITY.md`.
pub const FAKE_UPSTREAM_KEY: &str = "sk-fake-upstream-key-for-tests";

/// What the fake upstream actually received, so a test can assert on
/// exactly which headers and path arrived.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub query: Option<String>,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl RecordedRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(|v| v.as_str())
    }
}

/// A running fake upstream.
pub struct FakeUpstream {
    pub addr: SocketAddr,
    received: Arc<Mutex<Vec<RecordedRequest>>>,
    /// Incremented on every `401` served, so a test can prove the
    /// credential-refresh retry happened exactly once.
    unauthorized_served: Arc<Mutex<u32>>,
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

impl FakeUpstream {
    pub fn received(&self) -> Vec<RecordedRequest> {
        self.received.lock().unwrap().clone()
    }

    pub fn unauthorized_served(&self) -> u32 {
        *self.unauthorized_served.lock().unwrap()
    }
}

/// The canned SSE stream, with `ping` events and a comment line, and a
/// `message_delta` sequence whose `output_tokens` is cumulative.
pub const CANNED_SSE: &str = concat!(
    "event: message_start\n",
    r#"data: {"type":"message_start","message":{"id":"msg_fake","usage":{"input_tokens":120,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":1}}}"#,
    "\n\n",
    "event: ping\n",
    "data: {\"type\": \"ping\"}\n\n",
    ": an SSE comment the gateway must relay untouched\n\n",
    "event: content_block_delta\n",
    r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}"#,
    "\n\n",
    "event: message_delta\n",
    r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":9}}"#,
    "\n\n",
    "event: message_delta\n",
    r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":30}}"#,
    "\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// The canned non-streaming body, carrying usage at the top level.
pub const CANNED_JSON: &str = r#"{"id":"msg_fake","type":"message","role":"assistant","content":[{"type":"text","text":"hello"}],"usage":{"input_tokens":120,"output_tokens":30}}"#;

/// An SSE stream that reports usage and then stops abruptly, with no
/// `message_stop`.
pub const CANNED_SSE_TRUNCATED: &str = concat!(
    "event: message_start\n",
    r#"data: {"type":"message_start","message":{"id":"msg_fake","usage":{"input_tokens":120,"output_tokens":1}}}"#,
    "\n\n",
    "event: message_delta\n",
    r#"data: {"type":"message_delta","usage":{"output_tokens":17}}"#,
    "\n\n",
);

/// An SSE stream whose reported output exceeds any sane `max_tokens`, for
/// the bound-violation path.
pub const CANNED_SSE_OVER_BOUND: &str = concat!(
    "event: message_start\n",
    r#"data: {"type":"message_start","message":{"id":"msg_fake","usage":{"input_tokens":10,"output_tokens":1}}}"#,
    "\n\n",
    "event: message_delta\n",
    r#"data: {"type":"message_delta","usage":{"output_tokens":999999}}"#,
    "\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

fn body(bytes: impl Into<Bytes>) -> BoxBody<Bytes, std::io::Error> {
    Full::new(bytes.into())
        .map_err(|never| match never {})
        .boxed()
}

async fn serve_one(
    req: Request<Incoming>,
    received: Arc<Mutex<Vec<RecordedRequest>>>,
    unauthorized_served: Arc<Mutex<u32>>,
) -> Result<Response<BoxBody<Bytes, std::io::Error>>, std::io::Error> {
    let scenario = req
        .headers()
        .get(SCENARIO_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("sse")
        .to_string();
    let headers: HashMap<String, String> = req
        .headers()
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_ascii_lowercase(),
                value.to_str().unwrap_or("<non-utf8>").to_string(),
            )
        })
        .collect();
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(|q| q.to_string());
    let collected = req.into_body().collect().await.map(|c| c.to_bytes());
    let request_body = collected.map(|b| b.to_vec()).unwrap_or_default();

    received.lock().unwrap().push(RecordedRequest {
        method,
        path,
        query,
        headers,
        body: request_body,
    });

    let response = match scenario.as_str() {
        "nonstream" => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(body(CANNED_JSON))
            .unwrap(),
        "unauthorized" => {
            *unauthorized_served.lock().unwrap() += 1;
            Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .header("content-type", "application/json")
                .body(body(
                    r#"{"type":"error","error":{"type":"authentication_error","message":"bad key"}}"#,
                ))
                .unwrap()
        }
        // Answers 401 the first time and a normal stream afterwards, so a
        // test can prove the credential-refresh retry recovered.
        "unauthorized_once" => {
            let mut count = unauthorized_served.lock().unwrap();
            *count += 1;
            if *count == 1 {
                Response::builder()
                    .status(StatusCode::UNAUTHORIZED)
                    .body(body(r#"{"type":"error"}"#))
                    .unwrap()
            } else {
                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .body(body(CANNED_SSE))
                    .unwrap()
            }
        }
        "server_error" => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header("content-type", "application/json")
            .body(body(
                r#"{"type":"error","error":{"type":"api_error","message":"upstream exploded"}}"#,
            ))
            .unwrap(),
        "overloaded" => Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .header("content-type", "application/json")
            .body(body(
                r#"{"type":"error","error":{"type":"rate_limit_error"}}"#,
            ))
            .unwrap(),
        "redirect" => Response::builder()
            .status(StatusCode::FOUND)
            .header("location", "https://evil.example.com/v1/messages")
            .body(body(""))
            .unwrap(),
        "truncated" => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(body(CANNED_SSE_TRUNCATED))
            .unwrap(),
        "over_bound" => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(body(CANNED_SSE_OVER_BOUND))
            .unwrap(),
        "no_usage" => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(body("event: ping\ndata: {\"type\":\"ping\"}\n\n"))
            .unwrap(),
        "count_tokens" => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(body(r#"{"input_tokens":120}"#))
            .unwrap(),
        _ => Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(body(CANNED_SSE))
            .unwrap(),
    };
    Ok(response)
}

/// Starts the fake upstream on a loopback port, inside `runtime`.
pub fn start_fake_upstream(runtime: &tokio::runtime::Runtime) -> FakeUpstream {
    let received = Arc::new(Mutex::new(Vec::new()));
    let unauthorized_served = Arc::new(Mutex::new(0u32));
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();

    let received_for_task = Arc::clone(&received);
    let unauthorized_for_task = Arc::clone(&unauthorized_served);
    runtime.spawn(async move {
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    let received = Arc::clone(&received_for_task);
                    let unauthorized = Arc::clone(&unauthorized_for_task);
                    tokio::spawn(async move {
                        let service = hyper::service::service_fn(move |req| {
                            serve_one(req, Arc::clone(&received), Arc::clone(&unauthorized))
                        });
                        let _ = hyper_util::server::conn::auto::Builder::new(
                            hyper_util::rt::TokioExecutor::new(),
                        )
                        .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                        .await;
                    });
                }
            }
        }
    });

    FakeUpstream {
        addr,
        received,
        unauthorized_served,
        _shutdown: shutdown_tx,
    }
}

// ---------------------------------------------------------------------
// Stub authority and recorder
// ---------------------------------------------------------------------

/// What a [`StubAuthority`] should answer with.
#[derive(Debug, Clone)]
pub enum StubBehaviour {
    Grant { approval_required: bool },
    DenyPolicy,
    DenyInsufficient,
    DenyNoBudget,
    NoSession,
    Error,
}

/// One recorded call against the stub authority, so a test can assert
/// that a reservation was — or crucially was NOT — taken.
#[derive(Debug, Clone, PartialEq)]
pub enum AuthorityCall {
    Authorize {
        amount: ResourceAmount,
        idempotency_key: String,
    },
    Settle {
        reservation_id: ReservationId,
        actual: Option<ResourceAmount>,
    },
    Release {
        reservation_id: ReservationId,
    },
}

/// An in-memory `SpendAuthority`.
///
/// The gateway crate cannot depend on the ledger (that would let it
/// decide, which ADR 0003 §2 forbids), so its own tests drive the trait
/// directly. The ledger-backed implementation is exercised end to end by
/// `crates/daemon/tests/gateway_enforcement.rs`.
pub struct StubAuthority {
    pub behaviour: Mutex<StubBehaviour>,
    pub resource_kind: ResourceKind,
    pub calls: Arc<Mutex<Vec<AuthorityCall>>>,
    pub task_id: TaskId,
}

impl StubAuthority {
    pub fn new(behaviour: StubBehaviour, resource_kind: ResourceKind) -> Self {
        Self {
            behaviour: Mutex::new(behaviour),
            resource_kind,
            calls: Arc::new(Mutex::new(Vec::new())),
            task_id: TaskId::new(),
        }
    }

    pub fn calls(&self) -> Vec<AuthorityCall> {
        self.calls.lock().unwrap().clone()
    }

    pub fn authorize_count(&self) -> usize {
        self.calls()
            .iter()
            .filter(|c| matches!(c, AuthorityCall::Authorize { .. }))
            .count()
    }

    pub fn settlements(&self) -> Vec<Option<ResourceAmount>> {
        self.calls()
            .into_iter()
            .filter_map(|c| match c {
                AuthorityCall::Settle { actual, .. } => Some(actual),
                _ => None,
            })
            .collect()
    }

    pub fn release_count(&self) -> usize {
        self.calls()
            .iter()
            .filter(|c| matches!(c, AuthorityCall::Release { .. }))
            .count()
    }
}

impl SpendAuthority for StubAuthority {
    fn budget_context(&self, _session_id: &str) -> Result<Option<BudgetContext>, AuthorityError> {
        match &*self.behaviour.lock().unwrap() {
            StubBehaviour::NoSession => Ok(None),
            StubBehaviour::Error => Err(AuthorityError("stub ledger failure".to_string())),
            _ => Ok(Some(BudgetContext {
                task_id: self.task_id,
                resource_kind: self.resource_kind,
            })),
        }
    }

    fn authorize(&self, req: SpendRequest<'_>) -> Result<SpendDecision, AuthorityError> {
        self.calls.lock().unwrap().push(AuthorityCall::Authorize {
            amount: req.amount,
            idempotency_key: req.idempotency_key.to_string(),
        });
        let behaviour = self.behaviour.lock().unwrap().clone();
        Ok(match behaviour {
            StubBehaviour::Grant { approval_required } => SpendDecision::Granted {
                reservation_id: ReservationId::new(),
                reserved: req.amount,
                approval_required,
            },
            StubBehaviour::DenyPolicy => SpendDecision::Denied(SpendDenial::PolicyDenied {
                detail: "ResourceExceedsHardCeiling".to_string(),
            }),
            StubBehaviour::DenyInsufficient => SpendDecision::Denied(SpendDenial::Insufficient {
                available: Headroom {
                    kind: self.resource_kind,
                    value: 0.0,
                },
                requested: req.amount,
                protected_reserve: ResourceAmount::from_kind_f64(self.resource_kind, 5_000.0),
            }),
            StubBehaviour::DenyNoBudget | StubBehaviour::NoSession => {
                SpendDecision::Denied(SpendDenial::NoBudget)
            }
            StubBehaviour::Error => return Err(AuthorityError("stub failure".to_string())),
        })
    }

    fn settle(
        &self,
        reservation_id: ReservationId,
        actual: Option<ResourceAmount>,
    ) -> Result<(), AuthorityError> {
        self.calls.lock().unwrap().push(AuthorityCall::Settle {
            reservation_id,
            actual,
        });
        Ok(())
    }

    fn release(&self, reservation_id: ReservationId) -> Result<(), AuthorityError> {
        self.calls
            .lock()
            .unwrap()
            .push(AuthorityCall::Release { reservation_id });
        Ok(())
    }
}

/// Collects the provenance rows the gateway produces.
#[derive(Default)]
pub struct StubRecorder {
    pub records: Arc<Mutex<Vec<GatewayRequestRecord>>>,
}

impl StubRecorder {
    pub fn records(&self) -> Vec<GatewayRequestRecord> {
        self.records.lock().unwrap().clone()
    }

    /// The decision tag of the most recent row that carries one.
    pub fn last_decision(&self) -> Option<&'static str> {
        self.records().last().map(|r| r.decision)
    }
}

impl RequestRecorder for StubRecorder {
    fn record(&self, record: GatewayRequestRecord) {
        self.records.lock().unwrap().push(record);
    }
}

// ---------------------------------------------------------------------
// Test harness: a real gateway in front of the fake upstream
// ---------------------------------------------------------------------

/// A running gateway plus everything a test needs to assert on it.
pub struct TestGateway {
    pub addr: SocketAddr,
    pub token: String,
    pub authority: Arc<StubAuthority>,
    pub recorder: Arc<StubRecorder>,
    pub upstream: FakeUpstream,
    _dir: tempfile::TempDir,
    _shutdown: std::sync::mpsc::Sender<()>,
    _runtime: Arc<tokio::runtime::Runtime>,
}

/// Knobs a test can turn before the gateway starts.
pub struct TestGatewayOptions {
    pub behaviour: StubBehaviour,
    pub resource_kind: ResourceKind,
    pub credential_mode: libra_governor_gateway::config::GatewayCredentialMode,
    pub max_request_bytes: usize,
}

impl Default for TestGatewayOptions {
    fn default() -> Self {
        Self {
            behaviour: StubBehaviour::Grant {
                approval_required: false,
            },
            resource_kind: ResourceKind::Tokens,
            credential_mode: governor_held_fake_credential(),
            max_request_bytes: libra_governor_gateway::config::DEFAULT_MAX_REQUEST_BYTES,
        }
    }
}

/// A credential command that prints the fake upstream key. Nothing here
/// is a real secret — see `SECURITY.md` and this module's docs.
pub fn governor_held_fake_credential() -> libra_governor_gateway::config::GatewayCredentialMode {
    libra_governor_gateway::config::GatewayCredentialMode::GovernorHeld {
        credential: libra_governor_gateway::credential::CredentialCommand::new(
            "/bin/sh",
            vec!["-c".to_string(), format!("printf '{FAKE_UPSTREAM_KEY}'")],
        ),
    }
}

/// Starts a fake upstream and a real gateway pointed at it.
pub fn start_test_gateway(options: TestGatewayOptions) -> TestGateway {
    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap(),
    );
    let upstream = start_fake_upstream(&runtime);

    let dir = tempfile::tempdir().unwrap();
    let token_path = dir.path().join("gateway.token");
    // Created here rather than read back after startup: the gateway
    // generates it inside `build_state` on its own thread, so reading it
    // after merely observing the port would be a race. `load_or_create`
    // is idempotent, so the gateway loads this same value.
    let token =
        libra_governor_gateway::credential::LocalCapabilityToken::load_or_create(&token_path)
            .unwrap()
            .expose_for_agent()
            .to_string();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    // The plaintext-loopback opt-in, which exists precisely so this
    // harness can exist. See ADR 0003's "Deviations": the loopback host
    // literal check, not this flag, is the safeguard.
    let mut raw = libra_governor_gateway::config::GatewayConfig::new(
        addr,
        token_path.clone(),
        options.credential_mode,
    );
    raw.upstream = format!("http://127.0.0.1:{}", upstream.addr.port());
    raw.upstream_host_allowlist = vec!["127.0.0.1".to_string()];
    raw.allow_plaintext_loopback_upstream = true;
    raw.allow_nonstandard_port = true;
    raw.max_request_bytes = options.max_request_bytes;

    let validated = libra_governor_gateway::config::validate(raw, options.resource_kind)
        .expect("the harness configuration must validate");

    let authority = Arc::new(StubAuthority::new(options.behaviour, options.resource_kind));
    let recorder = Arc::new(StubRecorder::default());
    let runtime_config = libra_governor_gateway::server::GatewayRuntimeConfig::new(
        validated,
        Arc::clone(&authority) as Arc<dyn SpendAuthority>,
        Arc::clone(&recorder) as Arc<dyn RequestRecorder>,
        Arc::new(libra_governor_gateway::stats::GatewayStats::new()),
    );

    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ =
            libra_governor_gateway::server::run_gateway_on(runtime_config, listener, shutdown_rx);
    });

    // Wait for the listener to answer rather than sleeping a fixed
    // interval, so the suite is not timing-dependent on a loaded machine.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::net::TcpStream::connect(addr).is_err() {
        assert!(
            std::time::Instant::now() < deadline,
            "the test gateway never started listening"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    TestGateway {
        addr,
        token,
        authority,
        recorder,
        upstream,
        _dir: dir,
        _shutdown: shutdown_tx,
        _runtime: runtime,
    }
}

/// A response read back from the gateway, collected in full.
#[derive(Debug)]
pub struct TestResponse {
    pub status: StatusCode,
    pub headers: HashMap<String, String>,
    pub body: String,
}

impl TestResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(|v| v.as_str())
    }

    pub fn decision(&self) -> Option<&str> {
        self.header("x-libra-decision")
    }
}

/// Sends one request to `addr` over plain HTTP/1.1 and collects the whole
/// response.
pub fn send(
    runtime: &tokio::runtime::Runtime,
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> TestResponse {
    let method = method.to_string();
    let path = path.to_string();
    let headers: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    let body = Bytes::copy_from_slice(body);

    runtime.block_on(async move {
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (mut sender, conn) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
                .await
                .unwrap();
        tokio::spawn(conn);

        let mut builder = Request::builder()
            .method(method.as_str())
            .uri(path.as_str());
        let mut saw_host = false;
        for (name, value) in &headers {
            if name.eq_ignore_ascii_case("host") {
                saw_host = true;
            }
            builder = builder.header(name.as_str(), value.as_str());
        }
        if !saw_host {
            builder = builder.header("host", addr.to_string());
        }
        let request = builder.body(Full::new(body)).unwrap();

        let response = sender.send_request(request).await.unwrap();
        let status = response.status();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_ascii_lowercase(),
                    value.to_str().unwrap_or("<non-utf8>").to_string(),
                )
            })
            .collect();
        let collected = response.into_body().collect().await.unwrap().to_bytes();
        TestResponse {
            status,
            headers,
            body: String::from_utf8_lossy(&collected).to_string(),
        }
    })
}

/// A Messages request body with the given model and `max_tokens`.
pub fn messages_body(model: &str, max_tokens: Option<u64>, stream: bool) -> Vec<u8> {
    let mut value = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "hello"}],
        "stream": stream,
    });
    if let Some(max_tokens) = max_tokens {
        value["max_tokens"] = serde_json::json!(max_tokens);
    }
    serde_json::to_vec(&value).unwrap()
}

/// Blocks until `predicate` holds or the deadline passes. Settlement
/// happens on a spawned task after the response body ends, so a test
/// asserting on it must wait for that task rather than assume it already
/// ran.
pub fn wait_until(predicate: impl Fn() -> bool, what: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !predicate() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_canned_sse_stream_reports_cumulative_output_tokens() {
        // The fixture the lifecycle suite asserts against: 120 input,
        // final cumulative output 30 (not 1 + 9 + 30 = 40).
        assert!(CANNED_SSE.contains(r#""input_tokens":120"#));
        assert!(CANNED_SSE.contains(r#""output_tokens":30"#));
        assert!(CANNED_SSE.contains("event: ping"));
        assert!(
            CANNED_SSE.contains(": an SSE comment"),
            "the comment line exists precisely so a test can prove it is relayed untouched"
        );
        assert!(CANNED_SSE.contains("message_stop"));
    }

    #[test]
    fn the_truncated_stream_never_reaches_message_stop() {
        assert!(CANNED_SSE_TRUNCATED.contains("message_delta"));
        assert!(!CANNED_SSE_TRUNCATED.contains("message_stop"));
    }

    #[test]
    fn every_credential_shaped_fixture_value_is_obviously_fake() {
        assert!(
            FAKE_UPSTREAM_KEY.starts_with("sk-fake-"),
            "SECURITY.md requires fixture credentials be unmistakably non-functional"
        );
    }

    #[test]
    fn the_fake_upstream_serves_the_scenario_it_is_asked_for() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let upstream = start_fake_upstream(&runtime);

        let status = runtime.block_on(async {
            let stream = tokio::net::TcpStream::connect(upstream.addr).await.unwrap();
            let (mut sender, conn) =
                hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
                    .await
                    .unwrap();
            tokio::spawn(conn);
            let request = Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("host", upstream.addr.to_string())
                .header(SCENARIO_HEADER, "server_error")
                .body(Full::new(Bytes::from_static(b"{}")))
                .unwrap();
            sender.send_request(request).await.unwrap().status()
        });

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        let received = upstream.received();
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].path, "/v1/messages");
    }
}
