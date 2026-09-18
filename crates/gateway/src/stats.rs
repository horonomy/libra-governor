//! [`GatewayStats`] — the counters the statusline and `gateway status`
//! render (HORO-1144).
//!
//! # Why counters and not a log
//!
//! `ARCHITECTURE.md` names the statusline as the *visible explanation
//! channel*: when the Governor refuses or gates work, the user should
//! learn it there rather than from a file they never open. These counters
//! are that channel's data source.
//!
//! They are plain atomics with `Relaxed` ordering, shared by every
//! request task. Nothing here guards a correctness decision — the ledger
//! does that, transactionally — so exact cross-counter consistency is not
//! required and paying for it would put a synchronisation point on every
//! request for a number that is rendered once a second.
//!
//! # Privacy
//!
//! Every field is a `u64` counter or a small enum tag. There is no model
//! name, no session id, no path, and no body — the same rule the ledger's
//! own schema follows, applied to the in-memory surface so a future
//! "just add the last request for debugging" cannot be a one-field change.

use std::sync::atomic::{AtomicU64, Ordering};

/// Live counters for one running gateway.
#[derive(Debug, Default)]
pub struct GatewayStats {
    forwarded: AtomicU64,
    denied_budget: AtomicU64,
    denied_unenforceable: AtomicU64,
    denied_unauthorized: AtomicU64,
    approval_gated: AtomicU64,
    settled_with_known_usage: AtomicU64,
    settled_without_usage: AtomicU64,
    upstream_errors: AtomicU64,
    bound_violations: AtomicU64,
}

/// An immutable snapshot, so a renderer reads one coherent set of numbers
/// rather than racing each counter individually.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GatewayStatsSnapshot {
    pub forwarded: u64,
    pub denied_budget: u64,
    pub denied_unenforceable: u64,
    pub denied_unauthorized: u64,
    pub approval_gated: u64,
    pub settled_with_known_usage: u64,
    pub settled_without_usage: u64,
    pub upstream_errors: u64,
    pub bound_violations: u64,
}

impl GatewayStats {
    pub fn new() -> Self {
        Self::default()
    }

    /// A request that passed admission and was forwarded upstream.
    pub fn record_forwarded(&self) {
        self.forwarded.fetch_add(1, Ordering::Relaxed);
    }

    /// Refused because the budget could not absorb it — the policy denied
    /// it, or the ledger had no optional-work headroom left.
    pub fn record_denied_budget(&self) {
        self.denied_budget.fetch_add(1, Ordering::Relaxed);
    }

    /// Refused because it could not be metered exactly enough to enforce
    /// anything against it: no `max_tokens`, no task binding, an unpriced
    /// model, an unsupported resource kind.
    pub fn record_denied_unenforceable(&self) {
        self.denied_unenforceable.fetch_add(1, Ordering::Relaxed);
    }

    /// Refused on the local capability token, a `Host` mismatch, an
    /// ambiguous dual-auth state, or an unroutable path.
    pub fn record_denied_unauthorized(&self) {
        self.denied_unauthorized.fetch_add(1, Ordering::Relaxed);
    }

    /// Forwarded, but the policy asked for approval rather than admitting
    /// cleanly. Counted separately so the statusline can say so — the
    /// proxy itself cannot interrupt a human mid-request.
    pub fn record_approval_gated(&self) {
        self.approval_gated.fetch_add(1, Ordering::Relaxed);
    }

    /// Settled against the provider's own reported usage.
    pub fn record_settled_with_usage(&self) {
        self.settled_with_known_usage
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Settled at the conservative reserved amount because no usage was
    /// reported. Tracked separately because a rising count here means the
    /// ledger's spend figures are becoming approximations again.
    pub fn record_settled_without_usage(&self) {
        self.settled_without_usage.fetch_add(1, Ordering::Relaxed);
    }

    /// The upstream call failed to connect, or answered non-2xx.
    pub fn record_upstream_error(&self) {
        self.upstream_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// The provider reported more output tokens than `max_tokens`
    /// allowed — an assumption of the reservation arithmetic being
    /// violated. Surfaced rather than clamped.
    pub fn record_bound_violation(&self) {
        self.bound_violations.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> GatewayStatsSnapshot {
        GatewayStatsSnapshot {
            forwarded: self.forwarded.load(Ordering::Relaxed),
            denied_budget: self.denied_budget.load(Ordering::Relaxed),
            denied_unenforceable: self.denied_unenforceable.load(Ordering::Relaxed),
            denied_unauthorized: self.denied_unauthorized.load(Ordering::Relaxed),
            approval_gated: self.approval_gated.load(Ordering::Relaxed),
            settled_with_known_usage: self.settled_with_known_usage.load(Ordering::Relaxed),
            settled_without_usage: self.settled_without_usage.load(Ordering::Relaxed),
            upstream_errors: self.upstream_errors.load(Ordering::Relaxed),
            bound_violations: self.bound_violations.load(Ordering::Relaxed),
        }
    }
}

impl GatewayStatsSnapshot {
    /// Every request the gateway refused, for whatever reason.
    pub fn total_denied(&self) -> u64 {
        self.denied_budget + self.denied_unenforceable + self.denied_unauthorized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_start_at_zero_and_increment_independently() {
        let stats = GatewayStats::new();
        assert_eq!(stats.snapshot(), GatewayStatsSnapshot::default());

        stats.record_forwarded();
        stats.record_forwarded();
        stats.record_denied_budget();
        stats.record_approval_gated();
        stats.record_bound_violation();

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.forwarded, 2);
        assert_eq!(snapshot.denied_budget, 1);
        assert_eq!(snapshot.approval_gated, 1);
        assert_eq!(snapshot.bound_violations, 1);
        assert_eq!(snapshot.denied_unenforceable, 0);
    }

    #[test]
    fn total_denied_sums_every_refusal_category() {
        let stats = GatewayStats::new();
        stats.record_denied_budget();
        stats.record_denied_unenforceable();
        stats.record_denied_unenforceable();
        stats.record_denied_unauthorized();
        assert_eq!(stats.snapshot().total_denied(), 4);
    }

    #[test]
    fn counters_are_safe_to_share_across_threads() {
        let stats = std::sync::Arc::new(GatewayStats::new());
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let stats = std::sync::Arc::clone(&stats);
                std::thread::spawn(move || {
                    for _ in 0..1_000 {
                        stats.record_forwarded();
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(stats.snapshot().forwarded, 8_000);
    }
}
