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

use libra_governor_domain::{Headroom, ReservationId, ResourceAmount, ResourceKind, TaskId};

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
