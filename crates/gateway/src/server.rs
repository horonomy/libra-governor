//! Runtime bootstrap: bind the loopback listener, build the upstream
//! client, serve connections, shut down cleanly (HORO-1144).
//!
//! # A thread inside the daemon, not a second process
//!
//! [`run_gateway`] owns a Tokio multi-threaded runtime and blocks the
//! calling thread. `crates/daemon` starts it on a `std::thread` so its
//! own single-threaded, serial Unix-socket accept loop is untouched —
//! see ADR 0003 §1 for why those two workloads must not share a loop, and
//! `crates/daemon/src/server.rs`'s module docs for the accept loop's own
//! reasoning.
//!
//! # TLS is the destination binding
//!
//! The upstream connector is built `https_only` unless a plaintext
//! loopback upstream was explicitly permitted. Certificate verification
//! against the *configured* hostname is what actually binds the
//! destination: a DNS answer that redirects `api.anthropic.com` to a host
//! the attacker controls fails certificate validation and never carries a
//! byte. See ADR 0003 §7 on why a resolved-IP guard is deliberately not
//! added on top.

use std::sync::Arc;

use hyper::header::HeaderName;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ServerBuilder;

use crate::authority::SpendAuthority;
use crate::config::{GatewayCredentialMode, ValidatedGatewayConfig};
use crate::credential::{CredentialError, CredentialStore, LocalCapabilityToken};
use crate::proxy::{self, GatewayState, RequestRecorder, DEFAULT_SESSION_HEADER};
use crate::stats::GatewayStats;

/// Why a gateway could not start.
///
/// Every variant leaves the gateway off and the daemon running: a
/// misconfigured or credential-less gateway must not take away preflight,
/// estimation, and the ledger. See ADR 0003 §6.
#[derive(Debug, thiserror::Error)]
pub enum GatewayStartError {
    #[error("could not bind the gateway listener or read its token file: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not resolve the upstream credential: {0}")]
    Credential(#[from] CredentialError),
    #[error("could not build the gateway's async runtime: {0}")]
    Runtime(String),
    #[error("session header `{0}` is not a valid HTTP header name")]
    InvalidSessionHeader(String),
}

/// Everything the daemon hands the gateway at startup.
pub struct GatewayRuntimeConfig {
    pub config: ValidatedGatewayConfig,
    pub authority: Arc<dyn SpendAuthority>,
    pub recorder: Arc<dyn RequestRecorder>,
    pub stats: Arc<GatewayStats>,
    /// Header naming the agent session a request belongs to. Configurable
    /// because the header a harness emits is a property of that harness,
    /// not of this protocol — see the known limitation documented in
    /// `integrations/claude-code/README.md`.
    pub session_header: String,
}

impl GatewayRuntimeConfig {
    pub fn new(
        config: ValidatedGatewayConfig,
        authority: Arc<dyn SpendAuthority>,
        recorder: Arc<dyn RequestRecorder>,
        stats: Arc<GatewayStats>,
    ) -> Self {
        Self {
            config,
            authority,
            recorder,
            stats,
            session_header: DEFAULT_SESSION_HEADER.to_string(),
        }
    }
}

/// Builds the shared per-gateway state, resolving the credential and the
/// local capability token eagerly.
///
/// Eagerly on purpose: a gateway that started and then discovered on its
/// first real request that it has no credential would have already told
/// the user it was protecting them.
pub fn build_state(runtime: &GatewayRuntimeConfig) -> Result<GatewayState, GatewayStartError> {
    let credentials = match &runtime.config.credential_mode {
        GatewayCredentialMode::GovernorHeld { credential } => Some(Arc::new(
            tokio::sync::Mutex::new(CredentialStore::resolve(credential.clone())?),
        )),
        GatewayCredentialMode::PassThroughSubscription => None,
    };
    let token = LocalCapabilityToken::load_or_create(&runtime.config.token_path)?;
    let session_header = HeaderName::try_from(runtime.session_header.as_str())
        .map_err(|_| GatewayStartError::InvalidSessionHeader(runtime.session_header.clone()))?;

    let mut http_connector = hyper_util::client::legacy::connect::HttpConnector::new();
    http_connector.enforce_http(false);
    http_connector.set_connect_timeout(Some(runtime.config.upstream_connect_timeout));

    // `https_only` in every real deployment. The plaintext branch is
    // reachable only when validation already established that the
    // upstream host is a loopback literal — see ADR 0003's "Deviations".
    let schemes = hyper_rustls::HttpsConnectorBuilder::new().with_webpki_roots();
    let tls = if runtime.config.upstream_scheme == "http" {
        schemes
            .https_or_http()
            .enable_http1()
            .wrap_connector(http_connector)
    } else {
        schemes
            .https_only()
            .enable_http1()
            .wrap_connector(http_connector)
    };

    let client = hyper_util::client::legacy::Client::builder(TokioExecutor::new())
        // Never follow a redirect: a 3xx is relayed to the agent as-is.
        // Following one would let the upstream choose a destination,
        // which is the one thing this component must never permit.
        .build(tls);

    Ok(GatewayState {
        config: Arc::new(runtime.config.clone()),
        authority: Arc::clone(&runtime.authority),
        recorder: Arc::clone(&runtime.recorder),
        stats: Arc::clone(&runtime.stats),
        token,
        credentials,
        client,
        session_header,
        semaphore: Arc::new(tokio::sync::Semaphore::new(
            runtime.config.max_concurrent_requests,
        )),
    })
}

/// Runs the gateway until `shutdown` is signalled. Blocks the calling
/// thread; intended to be called from a dedicated `std::thread`.
pub fn run_gateway(
    runtime_config: GatewayRuntimeConfig,
    shutdown: std::sync::mpsc::Receiver<()>,
) -> Result<(), GatewayStartError> {
    let listener = std::net::TcpListener::bind(runtime_config.config.bind_addr)?;
    run_gateway_on(runtime_config, listener, shutdown)
}

/// Runs the gateway on an already-bound listener.
///
/// Separate from [`run_gateway`] because binding and serving fail for
/// different reasons and a caller often needs the bound address before
/// the server starts — a test binds port 0, learns the port, configures
/// the gateway to match, and only then hands the listener over. Splitting
/// them also means a bind failure is reported without ever constructing a
/// runtime.
pub fn run_gateway_on(
    runtime_config: GatewayRuntimeConfig,
    listener: std::net::TcpListener,
    shutdown: std::sync::mpsc::Receiver<()>,
) -> Result<(), GatewayStartError> {
    let state = Arc::new(build_state(&runtime_config)?);
    listener.set_nonblocking(true)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| GatewayStartError::Runtime(e.to_string()))?;

    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::from_std(listener)?;
        let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
        // The shutdown signal is a blocking std channel (the daemon side
        // is not async), so it is waited on from its own thread and
        // relayed into the runtime.
        std::thread::spawn(move || {
            let _ = shutdown.recv();
            let _ = tx.send(());
        });

        loop {
            tokio::select! {
                _ = &mut rx => break,
                accepted = listener.accept() => {
                    let (stream, _peer) = match accepted {
                        Ok(pair) => pair,
                        // One failed accept must never take the listener
                        // down; the daemon's own loop has the same rule.
                        Err(_) => continue,
                    };
                    let state = Arc::clone(&state);
                    tokio::spawn(async move {
                        let service = hyper::service::service_fn(move |req| {
                            proxy::handle(Arc::clone(&state), req)
                        });
                        let _ = ServerBuilder::new(TokioExecutor::new())
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    });
                }
            }
        }
        Ok::<(), std::io::Error>(())
    })?;

    Ok(())
}
