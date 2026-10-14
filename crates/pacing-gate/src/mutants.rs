//! Negative-control ("mutant") strategies for HORO-1768 PR-B.
//!
//! Each strategy here deliberately violates exactly one guarantee AC2
//! claims this crate's admission model structurally enforces. They exist
//! so `verify::verify`'s checks can be proven to actually catch a real
//! violation, rather than rubber-stamping whatever a strategy claims
//! about itself — see `tests/mutants_are_caught.rs`, which runs each of
//! these against a fixture built to expose exactly its own defect, and
//! pairs it with an honest strategy on the same fixture that is NOT
//! flagged (so the check is proven discriminating, not "flags
//! everything").
//!
//! None of these implementations call into `pacing`, `daemon`,
//! `gateway`, or `ledger` — same invariant `lib.rs` states for the whole
//! crate.

use time::OffsetDateTime;

use crate::strategy::{Capability, DriveEvent, Strategy};

/// Ignores admission entirely: starts every ready task immediately, with
/// no concurrency or quota check of any kind. Exists to prove
/// `verify::verify`'s hard-limit check actually fires on a real
/// violation — see `fixtures::x0_unguarded`, whose tiny 120-token window
/// only ever fits one of its five 100-token ready tasks at a time, so
/// starting all five at once (500 tokens) visibly oversubscribes it by
/// more than 4x.
pub struct Unguarded;

impl Strategy for Unguarded {
    fn name(&self) -> &str {
        "unguarded"
    }

    fn capability(&self) -> Capability {
        Capability::LiveCapable
    }

    fn decide(&mut self, _now: OffsetDateTime, ready: &[u32]) -> Vec<u32> {
        ready.to_vec()
    }
}

/// Claims (via [`Strategy::owning_principal`]) to act only on behalf of
/// one principal, but — like [`Unguarded`] — starts every ready task
/// regardless of which principal actually owns it. Exists to prove
/// `verify::verify` catches an identity violation from the ground-truth
/// `trace::TraceTask::principal` the driver actually started a task
/// under, never from anything this strategy itself reports (reading a
/// strategy's own self-described identity back would be exactly the kind
/// of rubber-stamp PR-B's verifier must not do). See
/// `fixtures::x1_foreign_session`, the only fixture in this crate with
/// more than one principal, built specifically so this mutant's
/// violation is isolated from any quota/hard-limit concern (its one
/// window has ample headroom for both tasks).
pub struct ForeignSession {
    owning_principal: String,
}

impl ForeignSession {
    pub fn new(owning_principal: impl Into<String>) -> Self {
        Self {
            owning_principal: owning_principal.into(),
        }
    }
}

impl Strategy for ForeignSession {
    fn name(&self) -> &str {
        "foreign_session"
    }

    fn capability(&self) -> Capability {
        Capability::LiveCapable
    }

    fn owning_principal(&self) -> Option<&str> {
        Some(&self.owning_principal)
    }

    fn decide(&mut self, _now: OffsetDateTime, ready: &[u32]) -> Vec<u32> {
        ready.to_vec()
    }
}

/// A [`crate::strategy::StaticBudget`]-shaped strategy that never checks
/// in: it ignores every [`DriveEvent::TaskCompleted`] notification, so
/// its own belief of how many slots are occupied only ever grows and
/// never shrinks. Once `believed_active` reaches `max_concurrent` it
/// never admits anything else again, even though the real `World` (and
/// every quota window) has long since freed the capacity. Exists to
/// prove `verify::verify`'s false-throttle/incompleteness metrics catch
/// a strategy that silently leaks its own capacity accounting — distinct
/// from [`Unguarded`] (too permissive) and from a legitimate
/// quota-driven hold (`Paced` correctly holding on t1/t5/t6).
pub struct ForgetfulBaseline {
    max_concurrent: usize,
    believed_active: usize,
}

impl ForgetfulBaseline {
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            max_concurrent,
            believed_active: 0,
        }
    }
}

impl Strategy for ForgetfulBaseline {
    fn name(&self) -> &str {
        "forgetful_baseline"
    }

    fn capability(&self) -> Capability {
        Capability::LiveCapable
    }

    fn decide(&mut self, _now: OffsetDateTime, ready: &[u32]) -> Vec<u32> {
        let mut started = Vec::new();
        for &task in ready {
            if self.believed_active >= self.max_concurrent {
                break;
            }
            self.believed_active += 1;
            started.push(task);
        }
        started
    }

    /// Deliberately a no-op for every event, including `TaskCompleted` —
    /// this is the mutant's whole point: never renews, never checks in,
    /// abandons tracking the instant a task it started actually finishes.
    fn notify(&mut self, _event: &DriveEvent) {}
}
