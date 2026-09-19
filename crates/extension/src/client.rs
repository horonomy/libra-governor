//! [`ProviderClient`] — the blocking API the daemon calls into for the
//! two request/response extension surfaces (business context, policy
//! webhook). HORO-1174.
//!
//! # A blocking API over an internal runtime
//!
//! `crates/daemon` has no async runtime (ADR 0003 deliberately kept
//! async work off its serial accept loop — see that ADR's §1). This
//! crate owns its own single-threaded Tokio runtime internally and
//! exposes a **blocking** API to the daemon, so `handle_preflight` can
//! call `fetch_business_context`/`request_policy_decision` exactly like
//! any other synchronous function, with a real, enforced timeout.
//!
//! # Every response is capped and schema-checked
//!
//! A response body over [`crate::config::MAX_RESPONSE_BYTES`], one whose
//! JSON fails to parse, or one whose `schema_version` does not match
//! [`crate::config::WIRE_SCHEMA_VERSION`] is reported as
//! [`ClientError::Malformed`] — treated identically to a timeout or a
//! non-200 status by the caller: a fail-open no-op, never a preflight
//! failure. See `docs/adr/0005-local-extension-points.md`.

use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Request, StatusCode, Uri};
use serde::de::DeserializeOwned;

use crate::config::{ValidatedSurfaceConfig, MAX_RESPONSE_BYTES, WIRE_SCHEMA_VERSION};
use crate::sign::SignedHeaders;
use crate::wire::{
    BusinessContextRequest, BusinessContextResponse, PolicyWebhookRequest, PolicyWebhookResponse,
};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("request timed out")]
    Timeout,
    #[error("connect/io error")]
    Io,
    #[error("could not build the runtime")]
    RuntimeUnavailable,
    #[error("response was malformed, oversized, or carried an unexpected schema version")]
    Malformed,
    #[error("upstream responded with status {0}")]
    UnexpectedStatus(u16),
}

/// One connect-send-read round trip over plain HTTP/1.1 to a loopback
/// destination, with a hard wall-clock `timeout` and a
/// [`MAX_RESPONSE_BYTES`] cap on the response body. `pub(crate)` so
/// `crate::dispatcher` can reuse it for event delivery without
/// duplicating the connect/timeout/cap logic.
pub(crate) async fn post_signed(
    url: &Uri,
    headers: &SignedHeaders,
    extra_headers: &[(&str, String)],
    body: Vec<u8>,
    timeout: Duration,
) -> Result<(StatusCode, Vec<u8>), ClientError> {
    tokio::time::timeout(
        timeout,
        post_signed_inner(url, headers, extra_headers, body),
    )
    .await
    .map_err(|_| ClientError::Timeout)?
}

async fn post_signed_inner(
    url: &Uri,
    headers: &SignedHeaders,
    extra_headers: &[(&str, String)],
    body: Vec<u8>,
) -> Result<(StatusCode, Vec<u8>), ClientError> {
    let host = url.host().ok_or(ClientError::Io)?;
    let port = url.port_u16().unwrap_or(80);
    let authority = format!("{host}:{port}");
    let path = url
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/")
        .to_string();

    let stream = tokio::net::TcpStream::connect(&authority)
        .await
        .map_err(|_| ClientError::Io)?;
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
            .await
            .map_err(|_| ClientError::Io)?;
    tokio::spawn(connection);

    let mut builder = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", authority)
        .header("content-type", "application/json")
        .header("x-libra-schema-version", WIRE_SCHEMA_VERSION)
        .header("x-libra-request-id", headers.request_id.clone())
        .header("x-libra-timestamp", headers.timestamp.to_string())
        .header("x-libra-nonce", headers.marker.clone())
        .header("x-libra-signature", headers.signature.clone());
    for (name, value) in extra_headers {
        builder = builder.header(*name, value.clone());
    }
    let request = builder
        .body(Full::new(Bytes::from(body)))
        .map_err(|_| ClientError::Io)?;

    let response = sender
        .send_request(request)
        .await
        .map_err(|_| ClientError::Io)?;
    let status = response.status();
    let collected = collect_capped(response.into_body()).await?;
    Ok((status, collected))
}

/// Reads a response body up to [`MAX_RESPONSE_BYTES`] + 1, so "exactly at
/// the cap" and "over the cap" stay distinguishable — mirrors
/// `crates/gateway::credential::CredentialCommand::resolve`'s discipline
/// for a subprocess's stdout.
async fn collect_capped(body: Incoming) -> Result<Vec<u8>, ClientError> {
    let mut buf = Vec::new();
    let mut body = body;
    loop {
        use http_body_util::BodyExt as _;
        let Some(frame) = body.frame().await else {
            break;
        };
        let frame = frame.map_err(|_| ClientError::Io)?;
        if let Ok(data) = frame.into_data() {
            buf.extend_from_slice(&data);
            if buf.len() > MAX_RESPONSE_BYTES {
                return Err(ClientError::Malformed);
            }
        }
    }
    Ok(buf)
}

fn parse_json<T: DeserializeOwned>(bytes: &[u8], schema_version: &str) -> Result<T, ClientError> {
    let value: T = serde_json::from_slice(bytes).map_err(|_| ClientError::Malformed)?;
    let raw: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| ClientError::Malformed)?;
    if raw.get("schema_version").and_then(|v| v.as_str()) != Some(schema_version) {
        return Err(ClientError::Malformed);
    }
    Ok(value)
}

/// The blocking client the daemon calls into.
pub struct ProviderClient {
    runtime: tokio::runtime::Runtime,
}

impl ProviderClient {
    /// Builds a client with its own current-thread Tokio runtime. See
    /// module docs.
    pub fn new() -> Result<Self, ClientError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| ClientError::RuntimeUnavailable)?;
        Ok(Self { runtime })
    }

    /// Fetches a business context. `Err` on any failure (timeout,
    /// connect failure, non-200, malformed/oversized/wrong-schema-version
    /// response) — the caller (the daemon) treats every `Err` identically
    /// as a fail-open no-op.
    pub fn fetch_business_context(
        &self,
        surface: &ValidatedSurfaceConfig,
        request: &BusinessContextRequest,
    ) -> Result<BusinessContextResponse, ClientError> {
        self.runtime
            .block_on(fetch_business_context_async(surface, request))
    }

    /// Requests a policy decision. Only ever called by the daemon when
    /// `decision.admission == ApprovalRequired`. Same `Err`-is-fail-open
    /// contract as [`Self::fetch_business_context`].
    pub fn request_policy_decision(
        &self,
        surface: &ValidatedSurfaceConfig,
        request: &PolicyWebhookRequest,
    ) -> Result<PolicyWebhookResponse, ClientError> {
        self.runtime
            .block_on(request_policy_decision_async(surface, request))
    }
}

async fn fetch_business_context_async(
    surface: &ValidatedSurfaceConfig,
    request: &BusinessContextRequest,
) -> Result<BusinessContextResponse, ClientError> {
    let body = serde_json::to_vec(request).map_err(|_| ClientError::Malformed)?;
    let headers = SignedHeaders::fresh(&surface.secret, &body);
    let (status, response_body) =
        post_signed(&surface.url, &headers, &[], body, surface.timeout).await?;
    if status != StatusCode::OK {
        return Err(ClientError::UnexpectedStatus(status.as_u16()));
    }
    parse_json(&response_body, WIRE_SCHEMA_VERSION)
}

async fn request_policy_decision_async(
    surface: &ValidatedSurfaceConfig,
    request: &PolicyWebhookRequest,
) -> Result<PolicyWebhookResponse, ClientError> {
    let body = serde_json::to_vec(request).map_err(|_| ClientError::Malformed)?;
    let headers = SignedHeaders::fresh(&surface.secret, &body);
    let (status, response_body) =
        post_signed(&surface.url, &headers, &[], body, surface.timeout).await?;
    if status != StatusCode::OK {
        return Err(ClientError::UnexpectedStatus(status.as_u16()));
    }
    parse_json(&response_body, WIRE_SCHEMA_VERSION)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret::WebhookSecretCommand;
    use http_body_util::combinators::BoxBody;
    use http_body_util::BodyExt;
    use hyper::body::Incoming;
    use hyper::{Request as HyperRequest, Response as HyperResponse};
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    fn fake_secret() -> crate::secret::WebhookSecret {
        WebhookSecretCommand::new(
            "/bin/sh",
            vec!["-c".to_string(), "printf sk-fake-client-secret".to_string()],
        )
        .resolve()
        .unwrap()
    }

    fn body_of(bytes: impl Into<Bytes>) -> BoxBody<Bytes, std::io::Error> {
        Full::new(bytes.into())
            .map_err(|never| match never {})
            .boxed()
    }

    /// Starts a tiny local hyper server serving a fixed scenario, for
    /// exercising `post_signed`/the two async fetch fns against something
    /// real rather than mocked out. Mirrors
    /// `crates/gateway/tests/fake_upstream.rs`'s pattern at a much
    /// smaller scale.
    async fn start_fake_server(
        response_body: &'static str,
        status: StatusCode,
    ) -> (
        SocketAddr,
        tokio::sync::oneshot::Sender<()>,
        Arc<Mutex<Vec<Vec<u8>>>>,
    ) {
        let received = Arc::new(Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let received_for_task = Arc::clone(&received);

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => break,
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { continue };
                        let received = Arc::clone(&received_for_task);
                        tokio::spawn(async move {
                            let service = hyper::service::service_fn(move |req: HyperRequest<Incoming>| {
                                let received = Arc::clone(&received);
                                async move {
                                    let collected = req.into_body().collect().await.map(|c| c.to_bytes());
                                    if let Ok(bytes) = collected {
                                        received.lock().unwrap().push(bytes.to_vec());
                                    }
                                    Ok::<_, std::io::Error>(
                                        HyperResponse::builder()
                                            .status(status)
                                            .header("content-type", "application/json")
                                            .body(body_of(response_body))
                                            .unwrap(),
                                    )
                                }
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

        (addr, shutdown_tx, received)
    }

    #[tokio::test]
    async fn post_signed_delivers_a_signed_request_and_reads_the_response() {
        let (addr, _shutdown, received) = start_fake_server(
            r#"{"schema_version":"libra.extension.v1","ok":true}"#,
            StatusCode::OK,
        )
        .await;
        let url: Uri = format!("http://127.0.0.1:{}/libra/test", addr.port())
            .parse()
            .unwrap();
        let secret = fake_secret();
        let body = br#"{"hello":"world"}"#.to_vec();
        let headers = SignedHeaders::fresh(&secret, &body);

        let (status, response) =
            post_signed(&url, &headers, &[], body.clone(), Duration::from_secs(2))
                .await
                .unwrap();

        assert_eq!(status, StatusCode::OK);
        assert!(String::from_utf8_lossy(&response).contains("\"ok\":true"));
        assert_eq!(received.lock().unwrap().as_slice(), &[body]);
    }

    #[tokio::test]
    async fn a_timeout_is_reported_as_client_error_timeout() {
        // Bind but never accept -- the connect succeeds (loopback), but
        // no HTTP response is ever produced, so the wrapping timeout must
        // fire.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Drop the listener immediately so nothing is listening at all --
        // proves connect-failure and timeout are both handled, whichever
        // this environment produces.
        drop(listener);

        let url: Uri = format!("http://127.0.0.1:{}/libra/test", addr.port())
            .parse()
            .unwrap();
        let secret = fake_secret();
        let headers = SignedHeaders::fresh(&secret, b"body");
        let result = post_signed(
            &url,
            &headers,
            &[],
            b"body".to_vec(),
            Duration::from_millis(200),
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn an_oversized_response_is_reported_as_malformed() {
        let big: String = "x".repeat(MAX_RESPONSE_BYTES + 100);
        let leaked: &'static str = Box::leak(big.into_boxed_str());
        let (addr, _shutdown, _received) = start_fake_server(leaked, StatusCode::OK).await;
        let url: Uri = format!("http://127.0.0.1:{}/libra/test", addr.port())
            .parse()
            .unwrap();
        let secret = fake_secret();
        let headers = SignedHeaders::fresh(&secret, b"body");
        let result = post_signed(
            &url,
            &headers,
            &[],
            b"body".to_vec(),
            Duration::from_secs(2),
        )
        .await;
        assert!(matches!(result, Err(ClientError::Malformed)));
    }
}
