//! [`SpendAuthority`] — the interface through which the gateway asks
//! permission to spend, and the only way it can affect the ledger
//! (HORO-1144).
//!
//! # The gateway enforces; it does not decide
//!
//! `ARCHITECTURE.md` states that rule, and this trait is what makes it
//! structural rather than aspirational. `libra-governor-gateway` depends
//! on `libra-governor-domain` for value types and on nothing else of
//! Libra's: it cannot open the ledger, cannot construct a `Policy`, and
//! cannot evaluate an admission. Everything it is allowed to do to the
//! task budget is one of the four methods below, and every one of them
//! is answered by `crates/daemon`'s `LedgerSpendAuthority`.
//!
//! A future change that tried to move an admission decision into the
//! gateway would have to add a dependency to do it — a visible,
//! reviewable act, rather than a few lines quietly appearing in a request
//! handler.
//!
//! # Synchronous on purpose
//!
//! Every method is blocking. The implementation is `rusqlite`, which is
//! blocking, and pretending otherwise with an `async` signature would
//! mean either a fake `async` wrapper or a runtime-blocking call inside a
//! future. The proxy calls these through `tokio::task::spawn_blocking`
//! instead, which is what that facility is for.

use libra_governor_domain::{
    Headroom, ReservationId, ResourceAmount, ResourceKind, SharedPoolReservationId, TaskId,
};

use crate::cost::{EnforcementGap, SettleFlags};
use crate::usage::ObservedUsage;

/// What the ledger already knows about the task a request is bound to —
/// fetched before any cost arithmetic, because the budget's own
/// [`ResourceKind`] determines which arithmetic is even valid.
///
/// `LedgerStore::reserve` returns a hard error, not a rejection, when a
/// reservation's kind differs from the task budget's. So the kind is an
/// *input* to [`crate::cost::worst_case_reservation`], never an output of
/// whether pricing happened to be available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetContext {
    pub task_id: TaskId,
    pub resource_kind: ResourceKind,
}

/// One request for permission to spend, already costed by the gateway.
#[derive(Debug, Clone)]
pub struct SpendRequest<'a> {
    pub task_id: TaskId,
    /// The agent session the request declared. Recorded for
    /// attributability; it is not an authorization token — only the
    /// ledger, inside its own transaction against the task's recorded
    /// budget, can create a reservation.
    pub session_id: &'a str,
    /// The worst-case amount, in the task budget's own kind.
    pub amount: ResourceAmount,
    /// Unique per inbound HTTP request (`gw:<uuid>`). A client retry is a
    /// *new* upstream call and is costed again; what this key prevents is
    /// double-*settling* one reservation, which HORO-1141's idempotency
    /// already handles.
    pub idempotency_key: &'a str,
    pub ttl_secs: u64,
}

/// Why permission to spend was refused.
///
/// Every variant maps onto an `x-libra-decision` header value, so a
/// refusal is explainable to the user without revealing anything about
/// the request's contents or any credential.
#[derive(Debug, Clone, PartialEq)]
pub enum SpendDenial {
    /// The admitting `Policy` refused the projected spend outright.
    /// `detail` is a rendered `DenyReason` list — amounts and limits, no
    /// content.
    PolicyDenied { detail: String },
    /// The policy would have allowed it but the ledger had no headroom
    /// left for [`libra_governor_domain::ReservationClass::OptionalWork`]
    /// — which is where the protected Completion Reserve does its work.
    Insufficient {
        available: Headroom,
        requested: ResourceAmount,
        protected_reserve: ResourceAmount,
    },
    /// No `task_budgets` row exists for this task: admission never ran,
    /// so there is no envelope to spend against and nothing to enforce.
    NoBudget,
}

/// The answer to a [`SpendRequest`].
#[derive(Debug, Clone, PartialEq)]
pub enum SpendDecision {
    Granted {
        reservation_id: ReservationId,
        reserved: ResourceAmount,
        /// The policy returned `ApprovalRequired` rather than a clean
        /// `Admit`. The request still proceeds — the proxy has no channel
        /// through which to interrupt a human mid-request — but the fact
        /// is surfaced through [`crate::stats::GatewayStats`] so the
        /// statusline can show it. See ADR 0003's "approval is visible,
        /// not actionable".
        approval_required: bool,
    },
    Denied(SpendDenial),
}

/// Anything that went wrong talking to the authority itself, as distinct
/// from the authority deliberately refusing.
///
/// Kept a plain string rather than a structured error: the gateway's only
/// possible response to either is the same (fail closed, `403`), and a
/// richer type would tempt an implementation into putting ledger internals
/// on the wire.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("spend authority unavailable: {0}")]
pub struct AuthorityError(pub String);

/// The gateway's whole permitted surface against the Governor's ledger
/// and policy.
pub trait SpendAuthority: Send + Sync + 'static {
    /// Resolves the task a session is bound to, and the resource kind its
    /// budget is denominated in.
    ///
    /// `Ok(None)` means the session has no task — a request that arrived
    /// without ever having been admitted. That is a refusal
    /// (`task_unbound`), not an error: forwarding it would mean spending
    /// against a budget that does not exist, which is precisely the state
    /// this component exists to prevent.
    fn budget_context(&self, session_id: &str) -> Result<Option<BudgetContext>, AuthorityError>;

    /// Evaluates the policy and, if it admits, atomically reserves
    /// `req.amount`.
    ///
    /// Both halves must happen: an implementation that evaluated the
    /// policy and then forwarded without reserving would leave concurrent
    /// requests able to each pass a check neither of them then committed
    /// to.
    fn authorize(&self, req: SpendRequest<'_>) -> Result<SpendDecision, AuthorityError>;

    /// Closes a reservation with its actual cost.
    ///
    /// `None` means no usage figure was reported, and settles at the full
    /// reserved amount — the conservative HORO-1141 fallback recorded as
    /// `usage_known = false`. It must never be flattened to zero:
    /// refunding a request that certainly cost something is the one
    /// settlement error that silently defeats a hard budget.
    fn settle(
        &self,
        reservation_id: ReservationId,
        actual: Option<ResourceAmount>,
    ) -> Result<(), AuthorityError>;

    /// Closes a reservation unspent and fully refunded — the path taken
    /// when the upstream call never happened (a connection failure) or
    /// produced no tokens at all.
    fn release(&self, reservation_id: ReservationId) -> Result<(), AuthorityError>;
}

// ---- The shared monthly budget (S11e) --------------------------------

/// Whether the shared monthly budget applies to the next request. Asked
/// once per request, before anything is reserved, so a request the shared
/// path would refuse never touches the task budget either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SharedMode {
    /// The shared budget is not in force for the gateway: exactly the
    /// task-budget behaviour that existed before it.
    NotConsulted,
    /// Every metered request also reserves against the shared pool.
    Enforce,
    /// The shared budget is in force but cannot be honoured (for example a
    /// pass-through gateway under `enforcement = gateway`): refuse.
    Unavailable,
}

/// One request for a hold on the shared pool. The authority prices the
/// worst case itself from these facts; the gateway never supplies an
/// amount, and never holds a pricing table for this path.
#[derive(Debug, Clone)]
pub struct SharedReserveRequest<'a> {
    pub session_id: &'a str,
    pub model: &'a str,
    /// The request body's full length. Looser than excluding inline base64
    /// payloads, which only over-reserves.
    pub body_bytes: usize,
    pub max_tokens: Option<u64>,
    pub image_count: usize,
    /// The per-request id; the authority joins it with its own boot id.
    pub request_id: &'a str,
    pub ttl_secs: u64,
}

/// A granted hold on the shared pool, and the facts it was priced with.
/// [`SharedBudgetAuthority::settle`] takes it back unchanged, so the
/// settlement is computed from the same model, `max_tokens` and input
/// bound as the reservation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedHold {
    pub id: SharedPoolReservationId,
    pub reserved_usd_cents: u64,
    pub input_bound_tokens: u64,
    pub model: String,
    pub max_tokens: u64,
}

/// Why the shared pool refused a request.
#[derive(Debug, Clone, PartialEq)]
pub enum SharedDenial {
    /// The request's worst case does not fit in the remaining cap.
    CapExhausted { remaining_usd_cents: i64 },
    /// The shared budget could not be consulted or is not usable.
    Unavailable,
    /// The session id is not one the shared budget can book against.
    Unattributable,
    /// No finite, priceable worst case exists for the request.
    Unenforceable(EnforcementGap),
}

#[derive(Debug, Clone, PartialEq)]
pub enum SharedDecision {
    /// The shared budget stopped applying between [`SharedBudgetAuthority::mode`]
    /// and the reserve; proceed on the task budget alone.
    NotConsulted,
    Granted(SharedHold),
    Denied(SharedDenial),
}

/// What a settlement booked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SharedSettled {
    pub booked_usd_cents: u64,
    /// `true` only when the booked amount is the provider's own figure.
    pub usage_known: bool,
    pub flags: SettleFlags,
}

/// The raw facts of a finished request. The authority applies the S11b
/// rule with the shared-budget table; the gateway prices nothing here.
#[derive(Debug, Clone)]
pub struct SharedSettlement<'a> {
    pub hold: &'a SharedHold,
    pub observed: Option<ObservedUsage>,
    pub completed_cleanly: bool,
    pub saw_message_start: bool,
}

/// The gateway's surface against the shared monthly budget. Like
/// [`SpendAuthority`], blocking, and answered by the daemon.
///
/// # Known limitation: a hold the gateway never settles
///
/// Renewal starts only once the response headers arrive and the pump exists.
/// A client that cancels before that, a pump task that panics, or a hold whose
/// lease lapses without renewal is never settled or released by the gateway.
/// It stays active until the daemon books it on expiry, at the full
/// reservation (an over-count, the safe direction), or at the next daemon
/// start. Cancellation is not redesigned here.
pub trait SharedBudgetAuthority: Send + Sync + 'static {
    fn mode(&self) -> SharedMode;

    fn reserve(&self, req: SharedReserveRequest<'_>) -> Result<SharedDecision, AuthorityError>;

    /// Books the request. An error leaves the hold active; the daemon books
    /// it at its reservation when it expires.
    fn settle(&self, settlement: SharedSettlement<'_>) -> Result<SharedSettled, AuthorityError>;

    /// Releases a hold for a request the provider did not bill.
    fn release(&self, id: SharedPoolReservationId) -> Result<(), AuthorityError>;

    /// Extends a hold for a stream that outlasts its first lease. Failure
    /// is only logged by the caller: safety does not depend on it.
    fn renew(&self, id: SharedPoolReservationId, ttl_secs: u64) -> Result<(), AuthorityError>;
}
