//! [`run_dispatcher`] — the event-delivery thread body (HORO-1174).
//!
//! # Backoff ladder
//!
//! `1s, 5s, 30s, 2m, 10m`, then `abandoned` — [`BACKOFF_LADDER_SECS`].
//! Rows are retained after abandonment (no pruning — an accepted,
//! disclosed accumulation cost, same doctrine
//! `crates/gateway`'s `gateway_requests` table already accepts for its
//! own provenance rows).
//!
//! # Signing happens here, per attempt — never at enqueue time
//!
//! Every delivery attempt calls [`crate::sign::SignedHeaders::fresh`]
//! fresh, over the same stored payload bytes. See `crate::sign` docs for
//! why `webhook_deliveries` has no column to persist a signature into in
//! the first place.
//!
//! # Never holds a write transaction across the HTTP send
//!
//! [`DeliveryQueue::due`] is a short, read-only select (bounded by
//! `limit`); the HTTP send happens with no ledger transaction open at
//! all; [`DeliveryQueue::mark_delivered`]/[`DeliveryQueue::mark_failed`]
//! is a second, separate short write. The dispatcher's `LedgerStore` is a
//! second SQLite connection against the same file the daemon's serial
//! accept loop uses (see
//! `crates/daemon::extension_authority::open_extension_ledger`) — holding
//! a `BEGIN IMMEDIATE` transaction across a multi-second HTTP timeout
//! would stall every hook and statusline refresh behind it.

use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;

use crate::client::post_signed;
use crate::config::ValidatedEventsConfig;
use crate::queue::{DeliveryQueue, QueuedDelivery};
use crate::sign::SignedHeaders;

/// Retry delays, indexed by `attempts_after - 1` (the attempt that just
/// failed). `attempts_after` at or beyond `max_attempts` (or beyond this
/// ladder's own length, whichever is smaller) abandons the row.
const BACKOFF_LADDER_SECS: [u64; 5] = [1, 5, 30, 120, 600];

/// How often the dispatcher polls for due deliveries.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Up to how many due deliveries one poll iteration selects.
const BATCH_LIMIT: usize = 20;

fn next_attempt_delay(attempts_after: u32, max_attempts: u32) -> Option<Duration> {
    if attempts_after >= max_attempts {
        return None;
    }
    let idx = (attempts_after as usize)
        .saturating_sub(1)
        .min(BACKOFF_LADDER_SECS.len() - 1);
    Some(Duration::from_secs(BACKOFF_LADDER_SECS[idx]))
}

async fn deliver_one(
    events: &ValidatedEventsConfig,
    queue: &Arc<dyn DeliveryQueue>,
    item: QueuedDelivery,
) {
    let now = OffsetDateTime::now_utc();
    let headers = SignedHeaders::fresh(&events.secret, &item.payload);
    let attempts_after = item.attempts + 1;
    let extra_headers = [("x-libra-delivery-attempt", attempts_after.to_string())];

    let result = post_signed(
        &events.url,
        &headers,
        &extra_headers,
        item.payload.clone(),
        events.timeout,
    )
    .await;

    match result {
        Ok((status, _body)) if status.is_success() => {
            let _ = queue.mark_delivered(item.event_id, now);
        }
        Ok((status, _body)) => {
            let delay = next_attempt_delay(attempts_after, events.max_attempts);
            let next_attempt_at = delay.map(|d| now + d);
            let _ = queue.mark_failed(
                item.event_id,
                next_attempt_at,
                Some(status.as_u16()),
                &format!("upstream responded with status {}", status.as_u16()),
            );
        }
        Err(e) => {
            let delay = next_attempt_delay(attempts_after, events.max_attempts);
            let next_attempt_at = delay.map(|d| now + d);
            let _ = queue.mark_failed(item.event_id, next_attempt_at, None, &e.to_string());
        }
    }
}

/// Runs the dispatcher loop until `shutdown` is dropped or signaled.
/// Intended to be the body of a dedicated OS thread — see
/// `crates/daemon::server::start_event_dispatcher`.
///
/// Owns its own single-threaded Tokio runtime (same "blocking API,
/// internal runtime" shape as `crate::client::ProviderClient`, except
/// this one's own loop is the "blocking" entry point rather than a
/// per-call `block_on`).
pub fn run_dispatcher(
    events: ValidatedEventsConfig,
    queue: Arc<dyn DeliveryQueue>,
    shutdown: std::sync::mpsc::Receiver<()>,
) {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };

    runtime.block_on(async move {
        loop {
            match shutdown.try_recv() {
                Ok(()) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }

            let now = OffsetDateTime::now_utc();
            if let Ok(due) = queue.due(now, BATCH_LIMIT) {
                for item in due {
                    deliver_one(&events, &queue, item).await;
                }
            }

            tokio::time::sleep(POLL_INTERVAL).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_ladder_matches_the_documented_schedule() {
        assert_eq!(next_attempt_delay(1, 5), Some(Duration::from_secs(1)));
        assert_eq!(next_attempt_delay(2, 5), Some(Duration::from_secs(5)));
        assert_eq!(next_attempt_delay(3, 5), Some(Duration::from_secs(30)));
        assert_eq!(next_attempt_delay(4, 5), Some(Duration::from_secs(120)));
        assert_eq!(
            next_attempt_delay(5, 5),
            None,
            "5th failure abandons at max_attempts=5"
        );
    }

    #[test]
    fn a_smaller_max_attempts_abandons_earlier() {
        assert_eq!(next_attempt_delay(1, 2), Some(Duration::from_secs(1)));
        assert_eq!(next_attempt_delay(2, 2), None);
    }

    #[test]
    fn max_attempts_beyond_the_ladder_length_clamps_to_the_last_delay() {
        assert_eq!(next_attempt_delay(6, 10), Some(Duration::from_secs(600)));
        assert_eq!(next_attempt_delay(10, 10), None);
    }
}
