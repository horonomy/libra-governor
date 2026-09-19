//! [`LedgerDeliveryQueue`] — the daemon side of
//! `libra-governor-extension`'s `DeliveryQueue` trait (HORO-1174).
//!
//! Mirrors `crates/daemon::gateway_authority::LedgerSpendAuthority`'s
//! shape exactly: `libra-governor-extension` defines the trait and owns
//! the polling/backoff/HTTP logic; this is the one implementation, backed
//! by its own `LedgerStore` connection to the same SQLite file (a second
//! connection, opened here, behind an `Arc<Mutex<..>>` — the same
//! one-connection-per-thread discipline the gateway's own authority
//! already establishes, for the same reason: `rusqlite::Connection` is
//! not `Sync`).

use std::sync::{Arc, Mutex};

use libra_governor_extension::{DeliveryQueue, EventKind, QueueError, QueuedDelivery};
use libra_governor_ledger::{DeliveryEnqueue, LedgerStore};
use time::OffsetDateTime;
use uuid::Uuid;

fn event_kind_to_str(kind: EventKind) -> &'static str {
    match kind {
        EventKind::Admission => "admission",
        EventKind::Replan => "replan",
        EventKind::Approval => "approval",
        EventKind::Outcome => "outcome",
    }
}

fn event_kind_from_str(s: &str) -> Option<EventKind> {
    match s {
        "admission" => Some(EventKind::Admission),
        "replan" => Some(EventKind::Replan),
        "approval" => Some(EventKind::Approval),
        "outcome" => Some(EventKind::Outcome),
        _ => None,
    }
}

pub struct LedgerDeliveryQueue {
    ledger: Arc<Mutex<LedgerStore>>,
}

impl LedgerDeliveryQueue {
    pub fn new(ledger: Arc<Mutex<LedgerStore>>) -> Self {
        Self { ledger }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, LedgerStore>, QueueError> {
        self.ledger
            .lock()
            .map_err(|_| QueueError("extension ledger mutex was poisoned".to_string()))
    }
}

impl DeliveryQueue for LedgerDeliveryQueue {
    fn due(&self, now: OffsetDateTime, limit: usize) -> Result<Vec<QueuedDelivery>, QueueError> {
        let ledger = self.lock()?;
        let rows = ledger
            .due_deliveries(now, limit)
            .map_err(|e| QueueError(e.to_string()))?;
        rows.into_iter()
            .filter_map(|row| {
                let event_id = Uuid::parse_str(&row.event_id).ok()?;
                let event_kind = event_kind_from_str(&row.event_kind)?;
                Some(QueuedDelivery {
                    event_id,
                    event_kind,
                    payload: row.payload_json.into_bytes(),
                    attempts: row.attempts,
                })
            })
            .map(Ok)
            .collect()
    }

    fn mark_delivered(&self, event_id: Uuid, now: OffsetDateTime) -> Result<(), QueueError> {
        let mut ledger = self.lock()?;
        ledger
            .record_delivery_success(&event_id.to_string(), now)
            .map_err(|e| QueueError(e.to_string()))
    }

    fn mark_failed(
        &self,
        event_id: Uuid,
        next_attempt_at: Option<OffsetDateTime>,
        status: Option<u16>,
        error: &str,
    ) -> Result<(), QueueError> {
        let mut ledger = self.lock()?;
        let abandon = next_attempt_at.is_none();
        ledger
            .record_delivery_failure(
                &event_id.to_string(),
                next_attempt_at,
                status,
                error,
                abandon,
            )
            .map_err(|e| QueueError(e.to_string()))
    }
}

/// Opens the extension surface's own `LedgerStore` against `ledger_path`.
/// A separate connection from both the daemon's own and the gateway's —
/// see this module's docs.
pub fn open_extension_ledger(
    ledger_path: &std::path::Path,
) -> Result<Arc<Mutex<LedgerStore>>, libra_governor_ledger::LedgerError> {
    Ok(Arc::new(Mutex::new(LedgerStore::open(ledger_path)?)))
}

/// Enqueues one event for delivery. `event_id` is minted by the caller
/// (via `EventEnvelope::new`) and carried through in `payload`.
/// Idempotent on `(event_kind, dedupe_key)` — see
/// `LedgerStore::enqueue_delivery` docs.
pub fn enqueue_event(
    ledger: &mut LedgerStore,
    event_id: Uuid,
    kind: EventKind,
    dedupe_key: &str,
    task_id: Option<libra_governor_domain::TaskId>,
    payload: &[u8],
    now: OffsetDateTime,
) -> Result<bool, libra_governor_ledger::LedgerError> {
    let event_id_str = event_id.to_string();
    let payload_str = String::from_utf8_lossy(payload);
    ledger.enqueue_delivery(DeliveryEnqueue {
        event_id: &event_id_str,
        event_kind: event_kind_to_str(kind),
        dedupe_key,
        task_id,
        payload_json: &payload_str,
        now,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use libra_governor_domain::TaskIdentity;

    fn store() -> Arc<Mutex<LedgerStore>> {
        let mut inner = LedgerStore::open_in_memory().unwrap();
        let identity = TaskIdentity::new(None);
        inner
            .insert_task(&identity, OffsetDateTime::UNIX_EPOCH)
            .unwrap();
        Arc::new(Mutex::new(inner))
    }

    #[test]
    fn enqueue_and_due_round_trip_through_the_queue_trait() {
        let ledger = store();
        let queue = LedgerDeliveryQueue::new(Arc::clone(&ledger));
        let event_id = Uuid::new_v4();

        {
            let mut guard = ledger.lock().unwrap();
            let inserted = enqueue_event(
                &mut guard,
                event_id,
                EventKind::Outcome,
                "task-1:plan-1",
                None,
                br#"{"schema_version":"libra.extension.v1"}"#,
                OffsetDateTime::UNIX_EPOCH,
            )
            .unwrap();
            assert!(inserted);
        }

        let due = queue.due(OffsetDateTime::UNIX_EPOCH, 10).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].event_id, event_id);
        assert_eq!(due[0].event_kind, EventKind::Outcome);
        assert_eq!(due[0].attempts, 0);
    }

    #[test]
    fn mark_delivered_removes_it_from_due() {
        let ledger = store();
        let queue = LedgerDeliveryQueue::new(Arc::clone(&ledger));
        let event_id = Uuid::new_v4();
        {
            let mut guard = ledger.lock().unwrap();
            enqueue_event(
                &mut guard,
                event_id,
                EventKind::Admission,
                "plan-1",
                None,
                b"{}",
                OffsetDateTime::UNIX_EPOCH,
            )
            .unwrap();
        }
        queue
            .mark_delivered(event_id, OffsetDateTime::UNIX_EPOCH)
            .unwrap();
        assert!(queue
            .due(OffsetDateTime::UNIX_EPOCH, 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn mark_failed_with_a_next_attempt_reschedules_rather_than_abandons() {
        let ledger = store();
        let queue = LedgerDeliveryQueue::new(Arc::clone(&ledger));
        let event_id = Uuid::new_v4();
        {
            let mut guard = ledger.lock().unwrap();
            enqueue_event(
                &mut guard,
                event_id,
                EventKind::Replan,
                "plan-1",
                None,
                b"{}",
                OffsetDateTime::UNIX_EPOCH,
            )
            .unwrap();
        }
        let retry_at = OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(5);
        queue
            .mark_failed(event_id, Some(retry_at), Some(500), "server error")
            .unwrap();
        assert!(queue
            .due(OffsetDateTime::UNIX_EPOCH, 10)
            .unwrap()
            .is_empty());
        assert_eq!(queue.due(retry_at, 10).unwrap().len(), 1);
    }
}
