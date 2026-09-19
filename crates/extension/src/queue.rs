//! [`DeliveryQueue`] — the trait `crate::dispatcher` polls, implemented
//! by the daemon against its own `LedgerStore` connection (HORO-1174).
//!
//! Mirrors `crates/gateway::authority::SpendAuthority`'s shape: this
//! crate defines the trait and owns the polling/backoff/HTTP-delivery
//! logic; the daemon supplies the only implementation
//! (`crates/daemon::extension_authority::LedgerDeliveryQueue`), backed by
//! its own `LedgerStore` connection to the same SQLite file (same
//! one-connection-per-thread discipline
//! `crates/daemon::gateway_authority::LedgerSpendAuthority` already
//! establishes for the gateway).

use time::OffsetDateTime;
use uuid::Uuid;

use crate::event::EventKind;

#[derive(Debug, Clone, thiserror::Error)]
#[error("delivery queue error: {0}")]
pub struct QueueError(pub String);

/// One row ready for a delivery attempt.
#[derive(Debug, Clone, PartialEq)]
pub struct QueuedDelivery {
    pub event_id: Uuid,
    pub event_kind: EventKind,
    /// The exact serialized `EventEnvelope` bytes to sign and send —
    /// identical across every attempt.
    pub payload: Vec<u8>,
    /// How many attempts have already been made (0 for a never-attempted
    /// delivery).
    pub attempts: u32,
}

pub trait DeliveryQueue: Send + Sync {
    /// Selects up to `limit` deliveries due for an attempt right now.
    /// Implementations must not hold a write transaction across this
    /// call and the eventual HTTP send — see `crate::dispatcher` docs.
    fn due(&self, now: OffsetDateTime, limit: usize) -> Result<Vec<QueuedDelivery>, QueueError>;

    /// Records a successful delivery.
    fn mark_delivered(&self, event_id: Uuid, now: OffsetDateTime) -> Result<(), QueueError>;

    /// Records a failed attempt. `next_attempt_at: None` means the
    /// backoff ladder is exhausted — the row is abandoned.
    fn mark_failed(
        &self,
        event_id: Uuid,
        next_attempt_at: Option<OffsetDateTime>,
        status: Option<u16>,
        error: &str,
    ) -> Result<(), QueueError>;
}
