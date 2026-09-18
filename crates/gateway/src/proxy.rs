//! The per-request state machine: reserve, forward, settle (HORO-1144).
//!
//! # States
//!
//! ```text
//! Received ──┬─ Host mismatch ─────────────────────────────▶ Rejected 421
//!            ├─ ambiguous dual auth ───────────────────────▶ Rejected 400
//!            ├─ local capability token mismatch ───────────▶ Rejected 401
//!            ├─ path not in the closed route table ────────▶ Rejected 404
//!            ├─ HEAD/GET /api/hello ──────────────────────▶ answered locally 200
//!            ├─ body over the cap ────────────────────────▶ Rejected 413
//!            ├─ POST count_tokens ────────────────────────▶ Forwarding (unmetered)
//!            └─ POST /v1/messages ────────────────────────▶ Estimating
//!
//! Estimating ┬─ no positive max_tokens ────────────────────▶ Rejected 403 unenforceable
//!            ├─ no session/task binding ───────────────────▶ Rejected 403 task_unbound
//!            ├─ unpriced model, USD budget ────────────────▶ Rejected 403 unpriced_model
//!            ├─ quota-percent budget ──────────────────────▶ Rejected 403 unenforceable
//!            └─ costed ───────────────────────────────────▶ Reserving
//!
//! Reserving ─┬─ Policy denies ─────────────────────────────▶ Rejected 403 budget_exceeded
//!            ├─ no headroom (Completion Reserve intact) ───▶ Rejected 403 budget_exceeded
//!            ├─ no task budget ───────────────────────────▶ Rejected 403 no_budget
//!            └─ granted (approval_required surfaced) ─────▶ Forwarding
//!
//! Forwarding ┬─ connect/TLS failure ───────────────────────▶ release, 502
//!            ├─ upstream 401 ─────────────────────────────▶ refresh credential, retry once
//!            ├─ upstream non-2xx ─────────────────────────▶ relay verbatim, release
//!            └─ upstream 2xx ─────────────────────────────▶ Streaming
//!
//! Streaming ─┬─ clean end ────────────────────────────────▶ settle(observed)
//!            ├─ client disconnected ──────────────────────▶ settle(last observed)
//!            ├─ upstream aborted ─────────────────────────▶ settle(last observed)
//!            └─ nothing parseable ───────────────────────▶ settle(None) → usage_known=false
//! ```
//!
//! Any state → process death: the reservation stays `Active` and
//! HORO-1141's `expire_stale_reservations` reclaims it at the TTL.
//!
//! # Why there is no `Drop` guard
//!
//! Settling is a blocking ledger call dispatched through
//! `spawn_blocking`, and `Drop` cannot `await`. So settlement is explicit
//! on every exit path instead, and the response body is produced by a
//! channel fed from a **spawned pump task** rather than by wrapping the
//! upstream body. That is what makes client disconnection an ordinary
//! branch: the pump's `send` fails, the loop breaks, and the same
//! settlement code at the bottom of the same function runs. Nothing
//! depends on a destructor firing.
//!
//! # Never buffer a streaming response
//!
//! Bytes are relayed frame by frame, verbatim — `ping` events, comments,
//! and all. Claude Code's own watchdog counts raw bytes during extended
//! thinking pauses, so holding a stream to parse it would look like a
//! hang. [`crate::usage::UsageAccumulator`] is a tap on the way past.
//!
//! # Request and response bodies are never logged
//!
//! At any level. There is deliberately no debug body-dump switch — see
//! ADR 0003. The only record a request leaves is the scalar
//! [`GatewayRequestRecord`] below.

use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::{Frame, Incoming};
use hyper::header::{HeaderName, HeaderValue};
use hyper::{Request, Response, StatusCode};
use libra_governor_domain::{ReservationId, ResourceAmount, ResourceKind, TaskId};

use crate::authority::{SpendAuthority, SpendDecision, SpendDenial, SpendRequest};
use crate::config::{GatewayCredentialMode, ValidatedGatewayConfig};
use crate::cost::{bound_violated, settled_cost, worst_case_reservation, EnforcementGap};
use crate::credential::{CredentialStore, LocalCapabilityToken};
use crate::pricing::PRICING_VERSION;
use crate::route::{match_route, Route};
use crate::stats::GatewayStats;
use crate::usage::UsageAccumulator;

/// The response body type every path here produces.
pub type GatewayBody = BoxBody<Bytes, std::io::Error>;

/// The header carrying the agent session a request belongs to. Without
/// it the gateway cannot tell which task's budget to charge, and a
/// request that cannot be attributed is refused rather than charged to
/// whichever task happens to be around.
pub const DEFAULT_SESSION_HEADER: &str = "x-claude-code-session-id";

/// Headers never forwarded upstream: hop-by-hop framing, the inbound
/// `Host` (the outbound one is the configured upstream), the inbound
/// credentials (replaced or passed through deliberately), and the
/// forwarding-chain headers a downstream could use to spoof provenance.
const STRIPPED_REQUEST_HEADERS: &[&str] = &[
    "host",
    "authorization",
    "x-api-key",
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-forwarded-port",
    "x-real-ip",
];

/// Hop-by-hop headers stripped from the upstream response before it is
/// relayed back.
const STRIPPED_RESPONSE_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "transfer-encoding",
    "upgrade",
    "content-length",
];

/// The machine-readable reason carried in `x-libra-decision`.
///
/// A closed set, and every value is a category — never a message
/// assembled from request content. See [`Self::as_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allowed,
    BudgetExceeded,
    TaskUnbound,
    UnpricedModel,
    NoBudget,
    UnenforceableRequest,
    AmbiguousCredential,
    Unauthorized,
    HostMismatch,
    RouteNotFound,
    PayloadTooLarge,
    Overloaded,
    UpstreamUnavailable,
}

impl Decision {
    pub fn as_str(&self) -> &'static str {
        match self {
            Decision::Allowed => "allowed",
            Decision::BudgetExceeded => "budget_exceeded",
            Decision::TaskUnbound => "task_unbound",
            Decision::UnpricedModel => "unpriced_model",
            Decision::NoBudget => "no_budget",
            Decision::UnenforceableRequest => "unenforceable_request",
            Decision::AmbiguousCredential => "ambiguous_credential",
            Decision::Unauthorized => "unauthorized",
            Decision::HostMismatch => "host_mismatch",
            Decision::RouteNotFound => "route_not_found",
            Decision::PayloadTooLarge => "payload_too_large",
            Decision::Overloaded => "overloaded",
            Decision::UpstreamUnavailable => "upstream_unavailable",
        }
    }
}

/// Which exit path a request took. Recorded so an auditor can tell a
/// refusal from a completed call from a stream that was cut short.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalState {
    RejectedBeforeUpstream,
    AnsweredLocally,
    ForwardedUnmetered,
    UpstreamUnavailable,
    UpstreamRejected,
    CompletedCleanly,
    ClientDisconnected,
    StreamAborted,
}

impl TerminalState {
    pub fn as_str(&self) -> &'static str {
        match self {
            TerminalState::RejectedBeforeUpstream => "rejected_before_upstream",
            TerminalState::AnsweredLocally => "answered_locally",
            TerminalState::ForwardedUnmetered => "forwarded_unmetered",
            TerminalState::UpstreamUnavailable => "upstream_unavailable",
            TerminalState::UpstreamRejected => "upstream_rejected",
            TerminalState::CompletedCleanly => "completed_cleanly",
            TerminalState::ClientDisconnected => "client_disconnected",
            TerminalState::StreamAborted => "stream_aborted",
        }
    }
}

/// One provenance row. Scalars, enum tags, and identifiers only — no
/// body, no header, no prompt, no tool output. See ADR 0003 §11 and
/// `crates/ledger/migrations/0007_gateway_requests.sql`.
#[derive(Debug, Clone, PartialEq)]
pub struct GatewayRequestRecord {
    pub id: String,
    pub task_id: Option<TaskId>,
    pub session_id: Option<String>,
    pub route: Option<&'static str>,
    pub model: Option<String>,
    pub tier: &'static str,
    pub decision: &'static str,
    /// A short, structured detail string. Amounts and limits only.
    pub decision_detail: Option<String>,
    pub reservation_id: Option<ReservationId>,
    pub reserved_amount: Option<ResourceAmount>,
    pub settled_amount: Option<ResourceAmount>,
    pub resource_kind: Option<ResourceKind>,
    pub usage_known: bool,
    pub input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub max_tokens: Option<u64>,
    pub bound_violated: bool,
    pub pricing_version: &'static str,
    pub upstream_status: Option<u16>,
    pub terminal_state: &'static str,
}

impl GatewayRequestRecord {
    fn new(id: String, tier: &'static str) -> Self {
        Self {
            id,
            task_id: None,
            session_id: None,
            route: None,
            model: None,
            tier,
            decision: Decision::Allowed.as_str(),
            decision_detail: None,
            reservation_id: None,
            reserved_amount: None,
            settled_amount: None,
            resource_kind: None,
            usage_known: false,
            input_tokens: None,
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            output_tokens: None,
            max_tokens: None,
            bound_violated: false,
            pricing_version: PRICING_VERSION,
            upstream_status: None,
            terminal_state: TerminalState::RejectedBeforeUpstream.as_str(),
        }
    }
}

/// Where a finished request's provenance row goes.
///
/// A trait for the same reason [`SpendAuthority`] is one: the gateway
/// crate cannot open the ledger, so the daemon supplies the
/// implementation. A recorder that fails must never fail the request —
/// see [`Self::record`].
pub trait RequestRecorder: Send + Sync + 'static {
    /// Records one terminal transition. Implementations swallow their own
    /// errors: losing an audit row is bad, but failing a request the user
    /// has already been served (or refusing one the budget allows)
    /// because a log write failed is worse.
    fn record(&self, record: GatewayRequestRecord);
}

/// Everything one running gateway shares across its request tasks.
pub struct GatewayState {
    pub config: Arc<ValidatedGatewayConfig>,
    pub authority: Arc<dyn SpendAuthority>,
    pub recorder: Arc<dyn RequestRecorder>,
    pub stats: Arc<GatewayStats>,
    pub token: LocalCapabilityToken,
    /// `Some` in [`GatewayCredentialMode::GovernorHeld`], `None` in
    /// pass-through mode where the agent's own credential is forwarded.
    pub credentials: Option<Arc<tokio::sync::Mutex<CredentialStore>>>,
    pub client: UpstreamClient,
    pub session_header: HeaderName,
    /// Bounds requests in flight. A permit is held for the whole
    /// request, streaming included, so a hundred concurrent long-lived
    /// streams cannot exhaust file descriptors.
    pub semaphore: Arc<tokio::sync::Semaphore>,
}

/// The hyper client used for upstream calls.
pub type UpstreamClient = hyper_util::client::legacy::Client<
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
    Full<Bytes>,
>;

/// What the inbound request declared, as far as metering needs to know.
#[derive(Debug, Default, PartialEq)]
struct MessagesEnvelope {
    model: Option<String>,
    max_tokens: Option<u64>,
    stream: bool,
}

/// Reads `model`, `max_tokens`, and `stream` out of a Messages request
/// body.
///
/// Deliberately reads only those three fields and retains nothing else:
/// the body contains the user's prompt, and this function is the only
/// place in the gateway that looks inside one.
fn parse_messages_envelope(body: &[u8]) -> MessagesEnvelope {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return MessagesEnvelope::default();
    };
    MessagesEnvelope {
        model: value
            .get("model")
            .and_then(|m| m.as_str())
            .map(|s| s.to_string()),
        max_tokens: value.get("max_tokens").and_then(|m| m.as_u64()),
        stream: value
            .get("stream")
            .and_then(|s| s.as_bool())
            .unwrap_or(false),
    }
}

/// The credential the caller presented, and whether the two possible
/// headers agreed.
enum PresentedCredential {
    None,
    One {
        raw: String,
        header: &'static str,
    },
    /// Both `Authorization` and `x-api-key` were present with different
    /// values. Refused rather than guessed: picking one would make the
    /// gateway's choice of credential depend on header ordering.
    Ambiguous,
}

fn presented_credential(headers: &hyper::HeaderMap) -> PresentedCredential {
    let bearer = headers
        .get(hyper::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.strip_prefix("Bearer ").unwrap_or(v).trim().to_string());
    let api_key = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string());

    match (bearer, api_key) {
        (Some(a), Some(b)) if a != b => PresentedCredential::Ambiguous,
        (Some(a), _) => PresentedCredential::One {
            raw: a,
            header: "authorization",
        },
        (None, Some(b)) => PresentedCredential::One {
            raw: b,
            header: "x-api-key",
        },
        (None, None) => PresentedCredential::None,
    }
}

/// Builds the Anthropic-shaped refusal body.
///
/// Anthropic-shaped so Claude Code renders it as a real error rather than
/// as an unrecognised blob, and `403` rather than `429` because Claude
/// Code treats `429` as a rate limit and retries with backoff — which
/// would turn one budget refusal into a retry storm against a boundary
/// that will refuse every time.
fn refusal(
    status: StatusCode,
    decision: Decision,
    request_id: &str,
    reason: &str,
) -> Response<GatewayBody> {
    let body = serde_json::json!({
        "type": "error",
        "error": {
            "type": "permission_error",
            "message": format!("libra-governor: {reason}"),
        }
    })
    .to_string();

    let mut response = Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .header("x-libra-decision", decision.as_str())
        .body(full_body(Bytes::from(body)))
        .expect("a constant status/header/body triple is always a valid response");
    if let Ok(value) = HeaderValue::from_str(request_id) {
        response.headers_mut().insert("x-libra-request-id", value);
    }
    response
}

fn full_body(bytes: Bytes) -> GatewayBody {
    Full::new(bytes).map_err(|never| match never {}).boxed()
}

/// Handles exactly one inbound HTTP request through the whole state
/// machine, and writes exactly one provenance row before returning.
pub async fn handle(
    state: Arc<GatewayState>,
    req: Request<Incoming>,
) -> Result<Response<GatewayBody>, std::io::Error> {
    let request_id = uuid::Uuid::new_v4().to_string();
    let tier = tier_tag(state.config.tier);
    let mut record = GatewayRequestRecord::new(request_id.clone(), tier);

    let response = run(&state, req, &request_id, &mut record).await;
    state.recorder.record(record);
    Ok(response)
}

fn tier_tag(tier: libra_governor_domain::EnforcementTier) -> &'static str {
    match tier {
        libra_governor_domain::EnforcementTier::GatewayMetered => "gateway_metered",
        libra_governor_domain::EnforcementTier::GatewayObservedQuota => "gateway_observed_quota",
        libra_governor_domain::EnforcementTier::HooksOnly => "hooks_only",
    }
}

/// Marks `record` as a pre-upstream refusal and produces the response.
fn reject(
    state: &GatewayState,
    record: &mut GatewayRequestRecord,
    status: StatusCode,
    decision: Decision,
    request_id: &str,
    reason: &str,
) -> Response<GatewayBody> {
    record.decision = decision.as_str();
    record.terminal_state = TerminalState::RejectedBeforeUpstream.as_str();
    match decision {
        Decision::BudgetExceeded | Decision::NoBudget => state.stats.record_denied_budget(),
        Decision::TaskUnbound | Decision::UnpricedModel | Decision::UnenforceableRequest => {
            state.stats.record_denied_unenforceable()
        }
        _ => state.stats.record_denied_unauthorized(),
    }
    refusal(status, decision, request_id, reason)
}

async fn run(
    state: &Arc<GatewayState>,
    req: Request<Incoming>,
    request_id: &str,
    record: &mut GatewayRequestRecord,
) -> Response<GatewayBody> {
    // ---- Host header, checked BEFORE routing -------------------------
    //
    // Order matters: a request naming a destination it did not dial is
    // proxy abuse, and it must be refused without ever consulting the
    // route table or reading a body.
    let host_ok = req
        .headers()
        .get(hyper::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|h| state.config.accepts_host(h))
        .unwrap_or(false);
    if !host_ok {
        return reject(
            state,
            record,
            StatusCode::MISDIRECTED_REQUEST,
            Decision::HostMismatch,
            request_id,
            "this request names a host the local gateway does not serve",
        );
    }

    // ---- Local authorization -----------------------------------------
    let presented = presented_credential(req.headers());
    if matches!(presented, PresentedCredential::Ambiguous) {
        return reject(
            state,
            record,
            StatusCode::BAD_REQUEST,
            Decision::AmbiguousCredential,
            request_id,
            "Authorization and x-api-key disagree; refusing rather than guessing which to use",
        );
    }
    // In governor-held mode the caller must present the local capability
    // token. In pass-through mode the caller's own subscription
    // credential IS the authorization — it is forwarded unchanged and the
    // provider adjudicates it — so there is no local token to check.
    if matches!(
        state.config.credential_mode,
        GatewayCredentialMode::GovernorHeld { .. }
    ) {
        let authorized = match &presented {
            PresentedCredential::One { raw, .. } => state.token.matches(raw),
            _ => false,
        };
        if !authorized {
            return reject(
                state,
                record,
                StatusCode::UNAUTHORIZED,
                Decision::Unauthorized,
                request_id,
                "the local gateway capability token was missing or did not match",
            );
        }
    }

    // ---- Closed route table ------------------------------------------
    let path = req.uri().path().to_string();
    let query = req.uri().query().map(|q| q.to_string());
    let Some(route) = match_route(req.method(), &path) else {
        return reject(
            state,
            record,
            StatusCode::NOT_FOUND,
            Decision::RouteNotFound,
            request_id,
            "this endpoint is not proxied by the local gateway",
        );
    };
    record.route = Some(route.upstream_path());

    if route.is_answered_locally() {
        record.decision = Decision::Allowed.as_str();
        record.terminal_state = TerminalState::AnsweredLocally.as_str();
        return Response::builder()
            .status(StatusCode::OK)
            .header("x-libra-request-id", request_id)
            .body(full_body(Bytes::new()))
            .expect("a constant status/header/body triple is always a valid response");
    }

    // ---- Bounded body read -------------------------------------------
    let (parts, incoming) = req.into_parts();
    let body = match read_body_capped(incoming, state.config.max_request_bytes).await {
        Ok(bytes) => bytes,
        Err(BodyReadError::TooLarge) => {
            return reject(
                state,
                record,
                StatusCode::PAYLOAD_TOO_LARGE,
                Decision::PayloadTooLarge,
                request_id,
                "the request body exceeds the gateway's configured maximum",
            );
        }
        Err(BodyReadError::Io) => {
            return reject(
                state,
                record,
                StatusCode::BAD_REQUEST,
                Decision::UnenforceableRequest,
                request_id,
                "the request body could not be read in full",
            );
        }
    };

    // Concurrency bound. Held for the whole request, streaming included.
    let _permit = match state.semaphore_acquire().await {
        Some(permit) => permit,
        None => {
            return reject(
                state,
                record,
                StatusCode::SERVICE_UNAVAILABLE,
                Decision::Overloaded,
                request_id,
                "too many requests are already in flight through the local gateway",
            );
        }
    };

    if !route.is_metered() {
        // `count_tokens` costs nothing and returns no completion.
        // Reserving against it would deny real work for no reason.
        record.terminal_state = TerminalState::ForwardedUnmetered.as_str();
        return forward_unmetered(
            state,
            record,
            route,
            &parts,
            query.as_deref(),
            body,
            request_id,
        )
        .await;
    }

    metered_request(
        state, record, route, &parts, query, body, request_id, &presented,
    )
    .await
}

enum BodyReadError {
    TooLarge,
    Io,
}

/// Reads an inbound body, refusing at `cap` rather than growing without
/// bound. A local process must not be able to make the daemon allocate
/// arbitrary memory merely by claiming to send a large message.
async fn read_body_capped(mut incoming: Incoming, cap: usize) -> Result<Bytes, BodyReadError> {
    let mut buf: Vec<u8> = Vec::new();
    while let Some(frame) = incoming.frame().await {
        let frame = frame.map_err(|_| BodyReadError::Io)?;
        if let Some(data) = frame.data_ref() {
            if buf.len() + data.len() > cap {
                return Err(BodyReadError::TooLarge);
            }
            buf.extend_from_slice(data);
        }
    }
    Ok(Bytes::from(buf))
}

impl GatewayState {
    async fn semaphore_acquire(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        self.semaphore.clone().try_acquire_owned().ok()
    }
}

/// Builds the outbound request: the configured upstream origin, the
/// route's own constant path, and the original query string. No inbound
/// path segment is ever concatenated in — see [`crate::route`].
fn build_upstream_request(
    state: &GatewayState,
    route: Route,
    parts: &hyper::http::request::Parts,
    query: Option<&str>,
    body: Bytes,
    credential_header: Option<(HeaderName, HeaderValue)>,
) -> Result<Request<Full<Bytes>>, ()> {
    let mut uri = format!(
        "{}{}",
        state.config.upstream_origin(),
        route.upstream_path()
    );
    if let Some(query) = query {
        uri.push('?');
        uri.push_str(query);
    }

    let mut builder = Request::builder().method(parts.method.clone()).uri(uri);
    {
        let headers = builder.headers_mut().ok_or(())?;
        for (name, value) in parts.headers.iter() {
            if STRIPPED_REQUEST_HEADERS
                .iter()
                .any(|stripped| name.as_str().eq_ignore_ascii_case(stripped))
            {
                continue;
            }
            headers.append(name.clone(), value.clone());
        }
        if let Some((name, value)) = credential_header {
            headers.insert(name, value);
        }
    }
    builder.body(Full::new(body)).map_err(|_| ())
}

/// Copies an upstream response's status and headers onto a relayed
/// response, dropping hop-by-hop headers and adding the request id.
fn relay_head(
    status: StatusCode,
    upstream_headers: &hyper::HeaderMap,
    request_id: &str,
) -> hyper::http::response::Builder {
    let mut builder = Response::builder().status(status);
    if let Some(headers) = builder.headers_mut() {
        for (name, value) in upstream_headers.iter() {
            if STRIPPED_RESPONSE_HEADERS
                .iter()
                .any(|stripped| name.as_str().eq_ignore_ascii_case(stripped))
            {
                continue;
            }
            headers.append(name.clone(), value.clone());
        }
        if let Ok(value) = HeaderValue::from_str(request_id) {
            headers.insert("x-libra-request-id", value);
        }
    }
    builder
}

/// The credential header to put on an outbound request.
///
/// Governor-held mode substitutes the resolved upstream credential —
/// which is why the agent never needs to hold it. Pass-through mode
/// re-attaches the caller's own credential under the header it arrived
/// on, unchanged; the Governor takes no custody of it and the provider
/// adjudicates it.
async fn outbound_credential(
    state: &GatewayState,
    presented: &PresentedCredential,
) -> Option<(HeaderName, HeaderValue)> {
    match &state.config.credential_mode {
        GatewayCredentialMode::GovernorHeld { .. } => {
            let store = state.credentials.as_ref()?;
            let guard = store.lock().await;
            let value = guard.current().header_value().ok()?;
            Some((HeaderName::from_static("x-api-key"), value))
        }
        GatewayCredentialMode::PassThroughSubscription => match presented {
            PresentedCredential::One { raw, header } => {
                let mut value = HeaderValue::from_str(raw).ok()?;
                value.set_sensitive(true);
                let name = if *header == "authorization" {
                    hyper::header::AUTHORIZATION
                } else {
                    HeaderName::from_static("x-api-key")
                };
                Some((name, value))
            }
            _ => None,
        },
    }
}

/// Forwards a free endpoint (`count_tokens`) without reserving anything.
#[allow(clippy::too_many_arguments)]
async fn forward_unmetered(
    state: &Arc<GatewayState>,
    record: &mut GatewayRequestRecord,
    route: Route,
    parts: &hyper::http::request::Parts,
    query: Option<&str>,
    body: Bytes,
    request_id: &str,
) -> Response<GatewayBody> {
    let presented = presented_credential(&parts.headers);
    let credential = outbound_credential(state, &presented).await;
    let Ok(upstream_req) = build_upstream_request(state, route, parts, query, body, credential)
    else {
        return reject(
            state,
            record,
            StatusCode::BAD_GATEWAY,
            Decision::UpstreamUnavailable,
            request_id,
            "the upstream request could not be constructed",
        );
    };

    match state.client.request(upstream_req).await {
        Ok(upstream) => {
            let status = upstream.status();
            record.upstream_status = Some(status.as_u16());
            let (head, incoming) = upstream.into_parts();
            relay_head(status, &head.headers, request_id)
                .body(incoming.map_err(io_error).boxed())
                .unwrap_or_else(|_| {
                    refusal(
                        StatusCode::BAD_GATEWAY,
                        Decision::UpstreamUnavailable,
                        request_id,
                        "the upstream response could not be relayed",
                    )
                })
        }
        Err(_) => {
            state.stats.record_upstream_error();
            record.terminal_state = TerminalState::UpstreamUnavailable.as_str();
            refusal(
                StatusCode::BAD_GATEWAY,
                Decision::UpstreamUnavailable,
                request_id,
                "the upstream provider could not be reached",
            )
        }
    }
}

fn io_error<E: std::fmt::Debug>(_e: E) -> std::io::Error {
    // The upstream error is deliberately not rendered into the message:
    // a transport error can carry a URL, and a URL can carry a query
    // string. The category is all a client needs.
    std::io::Error::other("upstream body error")
}

/// The full reserve → forward → settle path for `POST /v1/messages`.
#[allow(clippy::too_many_arguments)]
async fn metered_request(
    state: &Arc<GatewayState>,
    record: &mut GatewayRequestRecord,
    route: Route,
    parts: &hyper::http::request::Parts,
    query: Option<String>,
    body: Bytes,
    request_id: &str,
    presented: &PresentedCredential,
) -> Response<GatewayBody> {
    // ---- Estimating --------------------------------------------------
    let envelope = parse_messages_envelope(&body);
    record.model = envelope.model.clone();
    record.max_tokens = envelope.max_tokens;

    let Some(model) = envelope.model.clone() else {
        return reject(
            state,
            record,
            StatusCode::FORBIDDEN,
            Decision::UnenforceableRequest,
            request_id,
            "the request declares no model, so its cost cannot be bounded",
        );
    };

    let session_id = parts
        .headers
        .get(&state.session_header)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    record.session_id = session_id.clone();
    let Some(session_id) = session_id.filter(|s| !s.is_empty()) else {
        return reject(
            state,
            record,
            StatusCode::FORBIDDEN,
            Decision::TaskUnbound,
            request_id,
            "the request carries no session binding, so no task budget can be charged",
        );
    };

    let context = {
        let authority = Arc::clone(&state.authority);
        let session = session_id.clone();
        match tokio::task::spawn_blocking(move || authority.budget_context(&session)).await {
            Ok(Ok(Some(context))) => context,
            Ok(Ok(None)) => {
                return reject(
                    state,
                    record,
                    StatusCode::FORBIDDEN,
                    Decision::TaskUnbound,
                    request_id,
                    "this session has no admitted task, so there is no budget to spend against",
                );
            }
            // A ledger failure fails CLOSED: the whole point of this
            // component is that spend cannot happen without a successful
            // reservation, and "the ledger is unavailable" is not a
            // reason to relax that.
            Ok(Err(_)) | Err(_) => {
                return reject(
                    state,
                    record,
                    StatusCode::FORBIDDEN,
                    Decision::NoBudget,
                    request_id,
                    "the local ledger could not be consulted, so no spend can be authorized",
                );
            }
        }
    };
    record.task_id = Some(context.task_id);
    record.resource_kind = Some(context.resource_kind);

    let reserved = match worst_case_reservation(
        context.resource_kind,
        body.len(),
        envelope.max_tokens,
        &model,
        &state.config.pricing,
    ) {
        Ok(amount) => amount,
        Err(gap) => {
            let (decision, reason) = match &gap {
                EnforcementGap::UnpricedModel { .. } => (
                    Decision::UnpricedModel,
                    "this model has no pinned price, so a currency budget cannot be enforced \
                     against it",
                ),
                EnforcementGap::UnboundedRequest => (
                    Decision::UnenforceableRequest,
                    "the request declares no positive max_tokens, so its cost has no upper bound",
                ),
                EnforcementGap::UnsupportedResourceKind { .. } => (
                    Decision::UnenforceableRequest,
                    "this task's budget is denominated in a unit that cannot be derived from \
                     token counts",
                ),
            };
            record.decision_detail = Some(gap.to_string());
            return reject(
                state,
                record,
                StatusCode::FORBIDDEN,
                decision,
                request_id,
                reason,
            );
        }
    };
    record.reserved_amount = Some(reserved);

    // ---- Reserving ---------------------------------------------------
    let idempotency_key = format!("gw:{request_id}");
    let decision = {
        let authority = Arc::clone(&state.authority);
        let task_id = context.task_id;
        let session = session_id.clone();
        let key = idempotency_key.clone();
        let ttl = state.config.reservation_ttl_secs;
        match tokio::task::spawn_blocking(move || {
            authority.authorize(SpendRequest {
                task_id,
                session_id: &session,
                amount: reserved,
                idempotency_key: &key,
                ttl_secs: ttl,
            })
        })
        .await
        {
            Ok(Ok(decision)) => decision,
            Ok(Err(_)) | Err(_) => {
                return reject(
                    state,
                    record,
                    StatusCode::FORBIDDEN,
                    Decision::NoBudget,
                    request_id,
                    "the local ledger could not authorize this spend",
                );
            }
        }
    };

    let (reservation_id, approval_required) = match decision {
        SpendDecision::Granted {
            reservation_id,
            approval_required,
            ..
        } => (reservation_id, approval_required),
        SpendDecision::Denied(denial) => {
            let (decision, reason) = match &denial {
                SpendDenial::PolicyDenied { .. } => (
                    Decision::BudgetExceeded,
                    "this request would exceed the budget admitted for this task",
                ),
                SpendDenial::Insufficient { .. } => (
                    Decision::BudgetExceeded,
                    "this task has no remaining budget outside its protected completion reserve",
                ),
                SpendDenial::NoBudget => (
                    Decision::NoBudget,
                    "this task has no admitted budget, so no spend can be authorized",
                ),
            };
            record.decision_detail = Some(denial_detail(&denial));
            return reject(
                state,
                record,
                StatusCode::FORBIDDEN,
                decision,
                request_id,
                reason,
            );
        }
    };
    record.reservation_id = Some(reservation_id);
    if approval_required {
        state.stats.record_approval_gated();
        record.decision_detail = Some("approval_required".to_string());
    }

    // ---- Forwarding --------------------------------------------------
    let upstream =
        send_with_credential_retry(state, route, parts, query.as_deref(), body, presented).await;

    let upstream = match upstream {
        Some(response) => response,
        None => {
            // The call never happened: refund in full rather than
            // charging for provider work that was never done.
            settle_or_release(state, reservation_id, None, true).await;
            state.stats.record_upstream_error();
            record.decision = Decision::UpstreamUnavailable.as_str();
            record.terminal_state = TerminalState::UpstreamUnavailable.as_str();
            return refusal(
                StatusCode::BAD_GATEWAY,
                Decision::UpstreamUnavailable,
                request_id,
                "the upstream provider could not be reached",
            );
        }
    };

    let status = upstream.status();
    record.upstream_status = Some(status.as_u16());
    let (head, incoming) = upstream.into_parts();

    if !status.is_success() {
        // A refused or failed upstream call generates no tokens, so the
        // reservation is released in full. The body is relayed unmodified
        // — the provider's own error is more useful to the agent than
        // anything this gateway could substitute.
        settle_or_release(state, reservation_id, None, true).await;
        state.stats.record_upstream_error();
        record.terminal_state = TerminalState::UpstreamRejected.as_str();
        return relay_head(status, &head.headers, request_id)
            .body(incoming.map_err(io_error).boxed())
            .unwrap_or_else(|_| {
                refusal(
                    StatusCode::BAD_GATEWAY,
                    Decision::UpstreamUnavailable,
                    request_id,
                    "the upstream response could not be relayed",
                )
            });
    }

    state.stats.record_forwarded();
    record.terminal_state = TerminalState::CompletedCleanly.as_str();

    let is_sse = head
        .headers
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.starts_with("text/event-stream"))
        .unwrap_or(envelope.stream);

    let (sender, channel) = http_body_util::channel::Channel::<Bytes, std::io::Error>::new(8);
    let settle_ctx = SettleContext {
        state: Arc::clone(state),
        reservation_id,
        resource_kind: context.resource_kind,
        model,
        max_tokens: envelope.max_tokens.unwrap_or(0),
        record: record.clone(),
        is_sse,
    };
    tokio::spawn(pump_and_settle(incoming, sender, settle_ctx));

    relay_head(status, &head.headers, request_id)
        .header("x-libra-decision", Decision::Allowed.as_str())
        .body(channel.boxed())
        .unwrap_or_else(|_| {
            refusal(
                StatusCode::BAD_GATEWAY,
                Decision::UpstreamUnavailable,
                request_id,
                "the upstream response could not be relayed",
            )
        })
}

/// Renders a denial into a short, structured detail string. Amounts and
/// limits only — never anything derived from the request body.
fn denial_detail(denial: &SpendDenial) -> String {
    match denial {
        SpendDenial::PolicyDenied { detail } => format!("policy_denied: {detail}"),
        SpendDenial::Insufficient {
            available,
            requested,
            protected_reserve,
        } => format!(
            "insufficient: available={:.0} requested={:.0} protected_reserve={:.0}",
            available.value,
            requested.as_f64(),
            protected_reserve.as_f64()
        ),
        SpendDenial::NoBudget => "no_budget".to_string(),
    }
}

/// Sends the upstream request, re-resolving the credential and retrying
/// exactly once on a `401`.
///
/// A `401` from the provider almost always means the key was rotated out
/// from under a long-lived daemon. One retry after a fresh credential
/// resolution recovers without a restart; the cooldown inside
/// [`CredentialStore`] is what stops a genuinely invalid credential from
/// prompting the keychain on every request. `None` means the call could
/// not be made at all.
async fn send_with_credential_retry(
    state: &Arc<GatewayState>,
    route: Route,
    parts: &hyper::http::request::Parts,
    query: Option<&str>,
    body: Bytes,
    presented: &PresentedCredential,
) -> Option<Response<Incoming>> {
    let credential = outbound_credential(state, presented).await;
    let request =
        build_upstream_request(state, route, parts, query, body.clone(), credential).ok()?;
    let response = state.client.request(request).await.ok()?;

    if response.status() != StatusCode::UNAUTHORIZED {
        return Some(response);
    }
    let Some(store) = state.credentials.as_ref() else {
        // Pass-through mode: the caller's own credential was rejected.
        // Refreshing is not ours to do — relay the provider's answer.
        return Some(response);
    };
    {
        let mut guard = store.lock().await;
        match guard.refresh_if_cooled_down() {
            Ok(true) => {}
            // Cooldown suppressed the refresh, or the credential command
            // failed. Either way there is nothing new to retry with.
            Ok(false) | Err(_) => return Some(response),
        }
    }

    let credential = outbound_credential(state, presented).await;
    let retry = build_upstream_request(state, route, parts, query, body, credential).ok()?;
    match state.client.request(retry).await {
        Ok(retried) => Some(retried),
        // The retry failed to even connect; the original 401 is still the
        // most honest thing to relay.
        Err(_) => Some(response),
    }
}

/// Everything the pump task needs to settle and record when the response
/// ends, however it ends.
struct SettleContext {
    state: Arc<GatewayState>,
    reservation_id: ReservationId,
    resource_kind: ResourceKind,
    model: String,
    max_tokens: u64,
    record: GatewayRequestRecord,
    is_sse: bool,
}

/// Relays the upstream body frame by frame, taps usage on the way past,
/// and settles exactly once when the relay ends — for any reason.
///
/// This is where "no `Drop` guard" is paid for: every way the relay can
/// end (clean EOF, client gone, upstream abort) breaks the same loop and
/// falls into the same settlement below.
async fn pump_and_settle(
    mut upstream: Incoming,
    mut sender: http_body_util::channel::Sender<Bytes, std::io::Error>,
    mut ctx: SettleContext,
) {
    let mut accumulator = UsageAccumulator::new();
    let mut whole_body: Vec<u8> = Vec::new();
    let mut terminal = TerminalState::CompletedCleanly;

    loop {
        match upstream.frame().await {
            Some(Ok(frame)) => {
                if let Some(data) = frame.data_ref() {
                    if ctx.is_sse {
                        accumulator.feed(data);
                    } else if whole_body.len() + data.len() <= NON_STREAM_TAP_CAP {
                        whole_body.extend_from_slice(data);
                    }
                }
                let forwarded = match frame.into_data() {
                    Ok(data) => Frame::data(data),
                    Err(other) => other,
                };
                if sender.send(forwarded).await.is_err() {
                    // The client hung up. The provider still did (and
                    // billed for) the work produced so far, so this
                    // settles at the last observed usage rather than
                    // refunding.
                    terminal = TerminalState::ClientDisconnected;
                    break;
                }
            }
            Some(Err(_)) => {
                terminal = TerminalState::StreamAborted;
                break;
            }
            None => break,
        }
    }
    drop(sender);

    if !ctx.is_sse {
        accumulator.feed_whole_body(&whole_body);
    }

    let observed = accumulator.observed();
    let actual = observed.and_then(|usage| {
        settled_cost(
            ctx.resource_kind,
            &usage,
            &ctx.model,
            &ctx.state.config.pricing,
        )
        .ok()
    });

    if let Some(usage) = observed {
        ctx.record.input_tokens = Some(usage.input_tokens);
        ctx.record.cache_creation_input_tokens = Some(usage.cache_creation_input_tokens);
        ctx.record.cache_read_input_tokens = Some(usage.cache_read_input_tokens);
        ctx.record.output_tokens = Some(usage.output_tokens);
        if ctx.max_tokens > 0 && bound_violated(ctx.max_tokens, usage.output_tokens) {
            ctx.record.bound_violated = true;
            ctx.state.stats.record_bound_violation();
        }
    }
    ctx.record.usage_known = actual.is_some();
    ctx.record.settled_amount = actual.or(ctx.record.reserved_amount);
    ctx.record.terminal_state = terminal.as_str();
    if actual.is_some() {
        ctx.state.stats.record_settled_with_usage();
    } else {
        ctx.state.stats.record_settled_without_usage();
    }

    settle_or_release(&ctx.state, ctx.reservation_id, actual, false).await;
    ctx.state.recorder.record(ctx.record);
}

/// How much of a non-streaming response body is copied for parsing.
///
/// Bytes are relayed to the client regardless; this caps only the tap's
/// own copy. A response larger than this settles conservatively at the
/// reserved amount rather than growing the daemon's memory to parse a
/// usage object.
const NON_STREAM_TAP_CAP: usize = 8 * 1024 * 1024;

/// Closes a reservation, either settling it at `actual` or releasing it
/// in full.
///
/// A failure here is swallowed deliberately: the reservation stays
/// `Active` and HORO-1141's `expire_stale_reservations` reclaims it at
/// the TTL. Propagating would mean failing a response the user has
/// already received.
async fn settle_or_release(
    state: &Arc<GatewayState>,
    reservation_id: ReservationId,
    actual: Option<ResourceAmount>,
    release: bool,
) {
    let authority = Arc::clone(&state.authority);
    let _ = tokio::task::spawn_blocking(move || {
        if release {
            authority.release(reservation_id)
        } else {
            authority.settle(reservation_id, actual)
        }
    })
    .await;
}
