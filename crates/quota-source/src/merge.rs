//! Conflict resolution between multiple [`AcquiredReading`]s for the same
//! window — acceptance criterion 3: "conflicting external quota readings
//! never silently overwrite more-trustworthy facts".
//!
//! `quota_window::evaluate::evaluate_gauge` itself only ever looks at the
//! single *latest* snapshot by `observed_at` (with a conservative
//! same-instant tie-break) — it has no concept of "trustworthy" at all,
//! because `Confidence` lives one layer up, on this crate's own
//! [`AcquiredReading`]/provenance, not on `ProviderSnapshot` in isolation
//! from a `QuotaScope`. This module is where that trust comparison
//! belongs: deciding, from a list of candidate readings for one window,
//! which single one an acquisition pipeline should keep — before
//! anything is ever handed to `evaluate`.
//!
//! # The rule
//!
//! Readings are considered in chronological order (`observed_at`
//! ascending). The currently-kept reading is replaced by the next one
//! **only if** the next one is strictly newer *and* at least as trusted
//! (`Confidence`, `>=`). A newer-but-less-trusted reading never replaces
//! it — the central rule acceptance criterion 3 requires. Every reading
//! that loses is logged with why, never silently dropped.

use crate::provenance::AcquiredReading;

/// Why an incoming reading lost to the one currently kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictReason {
    /// The rejected reading is chronologically older than the one kept
    /// and was never in a position to replace it.
    NotNewer,
    /// The rejected reading is newer than the one kept but strictly less
    /// trusted (`Confidence`) — acceptance criterion 3's central rule:
    /// newer never overrides more-trustworthy.
    LessTrusted,
    /// The rejected reading shares the exact same `observed_at` and
    /// `Confidence` as the one kept; the kept one wins this module's own
    /// deterministic, arbitrary-but-documented tie-break (see
    /// [`merge_readings`]) rather than letting input order decide.
    TieKeptExisting,
    /// The rejected reading is an *older* fact that was superseded by a
    /// later, at-least-as-trusted reading — not rejected for being
    /// untrustworthy, simply no longer the current fact.
    SupersededByNewerTrusted,
}

/// One rejected candidate and why it lost, kept for an honest record —
/// acceptance criterion 3 requires conflicts never be silent.
#[derive(Debug, Clone, PartialEq)]
pub struct Conflicted {
    pub rejected: AcquiredReading,
    pub reason: ConflictReason,
}

/// The result of merging a non-empty set of candidate readings for one
/// window: the single winner, plus every reading that lost and why.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeOutcome {
    pub winner: AcquiredReading,
    pub conflicts: Vec<Conflicted>,
}

/// A deterministic total order over readings sharing an `observed_at`:
/// higher `Confidence` first, then `trust_owner` ascending. Exists purely
/// so an exact tie resolves the same way regardless of input order — see
/// [`merge_readings`]'s permutation-determinism test. Which specific
/// candidate an exact full tie (same instant, same confidence) resolves
/// to is an arbitrary but fixed, documented choice, not a meaningful
/// trust judgment.
fn tie_break_key(
    reading: &AcquiredReading,
) -> (std::cmp::Reverse<libra_governor_domain::Confidence>, String) {
    (
        std::cmp::Reverse(reading.snapshot().confidence()),
        reading.provenance().trust_owner.clone(),
    )
}

/// Merges `candidates` (all assumed already validated against the same
/// window) into a single winner plus a conflict log, by folding over them
/// in chronological order and replacing the kept reading only when a
/// later one is at least as trusted. Deterministic regardless of
/// `candidates`' input order, because the candidates are always sorted
/// (`observed_at` ascending, [`tie_break_key`] breaking exact ties)
/// before folding.
///
/// Returns `None` only if `candidates` is empty.
pub fn merge_readings(candidates: Vec<AcquiredReading>) -> Option<MergeOutcome> {
    if candidates.is_empty() {
        return None;
    }
    let mut sorted = candidates;
    sorted.sort_by(|a, b| {
        a.snapshot()
            .observed_at()
            .cmp(&b.snapshot().observed_at())
            .then_with(|| tie_break_key(a).cmp(&tie_break_key(b)))
    });

    let mut iter = sorted.into_iter();
    let mut current = iter.next().expect("checked non-empty above");
    let mut conflicts = Vec::new();

    for next in iter {
        let current_at = current.snapshot().observed_at();
        let next_at = next.snapshot().observed_at();
        if next_at == current_at {
            // Exact tie — `tie_break_key` already ordered these, so
            // `current` is the tie-break winner; `next` always loses.
            let reason = if next.snapshot().confidence() == current.snapshot().confidence() {
                ConflictReason::TieKeptExisting
            } else {
                ConflictReason::LessTrusted
            };
            conflicts.push(Conflicted {
                rejected: next,
                reason,
            });
        } else if next.snapshot().confidence() >= current.snapshot().confidence() {
            // `next` is strictly newer (sorted ascending) and at least as
            // trusted — it becomes the new current fact; the previous
            // `current` is superseded, not distrusted.
            let superseded = std::mem::replace(&mut current, next);
            conflicts.push(Conflicted {
                rejected: superseded,
                reason: ConflictReason::SupersededByNewerTrusted,
            });
        } else {
            // `next` is strictly newer but less trusted — acceptance
            // criterion 3's central rule: refused outright.
            conflicts.push(Conflicted {
                rejected: next,
                reason: ConflictReason::LessTrusted,
            });
        }
    }

    Some(MergeOutcome {
        winner: current,
        conflicts,
    })
}

/// Folds one `incoming` reading against an already-kept `existing`
/// reading, returning whichever one should be kept plus, when `incoming`
/// lost, the reason why. Exposed separately from [`merge_readings`] for
/// callers that process readings one at a time as they arrive (e.g. a
/// future streaming acquisition loop, where `incoming` may arrive out of
/// chronological order relative to `existing`) rather than collecting a
/// batch first.
pub fn resolve_pair(existing: AcquiredReading, incoming: AcquiredReading) -> MergeOutcome {
    let existing_at = existing.snapshot().observed_at();
    let incoming_at = incoming.snapshot().observed_at();

    if incoming_at > existing_at {
        if incoming.snapshot().confidence() >= existing.snapshot().confidence() {
            MergeOutcome {
                winner: incoming,
                conflicts: vec![Conflicted {
                    rejected: existing,
                    reason: ConflictReason::SupersededByNewerTrusted,
                }],
            }
        } else {
            MergeOutcome {
                winner: existing,
                conflicts: vec![Conflicted {
                    rejected: incoming,
                    reason: ConflictReason::LessTrusted,
                }],
            }
        }
    } else if incoming_at < existing_at {
        MergeOutcome {
            winner: existing,
            conflicts: vec![Conflicted {
                rejected: incoming,
                reason: ConflictReason::NotNewer,
            }],
        }
    } else {
        // Exact tie — delegate to the same deterministic rule
        // `merge_readings` uses, so both entry points always agree.
        merge_readings(vec![existing, incoming]).expect("two elements, never empty")
    }
}

#[cfg(test)]
mod tests {
    use libra_governor_domain::{
        Confidence, EntitlementSource, GaugeReading, PoolId, QuotaAmount, QuotaScope, QuotaSubject,
        QuotaUnit, QuotaWindow, QuotaWindowId, WindowKind,
    };
    use time::macros::datetime;

    use super::*;
    use crate::provenance::{Provenance, ReadingOrigin, SourceDescriptor};

    fn window() -> QuotaWindow {
        QuotaWindow::validated(
            QuotaWindowId::new(),
            QuotaScope {
                subject: QuotaSubject::SharedPool(PoolId("acme".to_string())),
                source: EntitlementSource::ProviderDeclared,
                confidence: Confidence::Medium,
                observed_at: datetime!(2026-10-08 00:00:00 UTC),
                valid_until: None,
            },
            QuotaUnit::Percent,
            WindowKind::OpaqueProviderSnapshot {
                max_staleness_secs: 86_400,
            },
        )
        .unwrap()
    }

    fn reading_at(
        window: &QuotaWindow,
        observed_at: time::OffsetDateTime,
        confidence: Confidence,
        trust_owner: &str,
    ) -> AcquiredReading {
        let snapshot = libra_governor_domain::ProviderSnapshot::validated(
            window.id(),
            observed_at,
            None,
            None,
            GaugeReading::Used {
                used: QuotaAmount::new(QuotaUnit::Percent, 1000),
                limit: None,
            },
            confidence,
        )
        .unwrap();
        AcquiredReading::validated(
            window,
            snapshot,
            Provenance {
                origin: ReadingOrigin::FixtureImport,
                source: SourceDescriptor {
                    capability: "fixture:test".to_string(),
                    documented_at: "docs/quota-source-fixture.md".to_string(),
                },
                subject: QuotaSubject::SharedPool(PoolId("acme".to_string())),
                trust_owner: trust_owner.to_string(),
            },
            observed_at + time::Duration::seconds(1),
        )
        .unwrap()
    }

    /// Acceptance criterion 3's core rule: a newer but *less trusted*
    /// reading must never silently overwrite a more-trustworthy older one.
    #[test]
    fn a_newer_but_less_trusted_reading_never_overwrites_a_more_trustworthy_older_one() {
        let w = window();
        let older_high = reading_at(
            &w,
            datetime!(2026-10-08 01:00:00 UTC),
            Confidence::High,
            "a",
        );
        let newer_low = reading_at(&w, datetime!(2026-10-08 02:00:00 UTC), Confidence::Low, "b");

        let outcome = merge_readings(vec![older_high.clone(), newer_low.clone()]).unwrap();
        assert_eq!(outcome.winner, older_high);
        assert_eq!(outcome.conflicts.len(), 1);
        assert_eq!(outcome.conflicts[0].rejected, newer_low);
        assert_eq!(outcome.conflicts[0].reason, ConflictReason::LessTrusted);
    }

    /// The same scenario, input in the opposite order — the winner must
    /// not depend on which one was listed first.
    #[test]
    fn the_less_trusted_newer_reading_loses_regardless_of_input_order() {
        let w = window();
        let older_high = reading_at(
            &w,
            datetime!(2026-10-08 01:00:00 UTC),
            Confidence::High,
            "a",
        );
        let newer_low = reading_at(&w, datetime!(2026-10-08 02:00:00 UTC), Confidence::Low, "b");

        let outcome = merge_readings(vec![newer_low, older_high.clone()]).unwrap();
        assert_eq!(outcome.winner, older_high);
    }

    #[test]
    fn a_newer_and_at_least_as_trusted_reading_wins() {
        let w = window();
        let older = reading_at(
            &w,
            datetime!(2026-10-08 01:00:00 UTC),
            Confidence::Medium,
            "a",
        );
        let newer = reading_at(
            &w,
            datetime!(2026-10-08 02:00:00 UTC),
            Confidence::Medium,
            "b",
        );

        let outcome = merge_readings(vec![older, newer.clone()]).unwrap();
        assert_eq!(outcome.winner, newer);
        assert_eq!(
            outcome.conflicts[0].reason,
            ConflictReason::SupersededByNewerTrusted
        );
    }

    /// Across a three-reading chronological sequence, a middle
    /// newer-but-less-trusted reading must not knock out the earlier
    /// trusted fact, and a still-less-trusted later reading must not
    /// either — the original fact survives both.
    #[test]
    fn an_earlier_trusted_fact_survives_multiple_later_less_trusted_readings() {
        let w = window();
        let t1 = reading_at(
            &w,
            datetime!(2026-10-08 01:00:00 UTC),
            Confidence::High,
            "a",
        );
        let t2 = reading_at(&w, datetime!(2026-10-08 02:00:00 UTC), Confidence::Low, "b");
        let t3 = reading_at(
            &w,
            datetime!(2026-10-08 03:00:00 UTC),
            Confidence::Medium,
            "c",
        );

        let outcome = merge_readings(vec![t1.clone(), t2, t3]).unwrap();
        assert_eq!(outcome.winner, t1);
        assert_eq!(outcome.conflicts.len(), 2);
        assert!(outcome
            .conflicts
            .iter()
            .all(|c| c.reason == ConflictReason::LessTrusted));
    }

    /// Permutation determinism, mirroring `quota_window::tests`'s own
    /// discipline for `evaluate`: the winner must not depend on input
    /// order.
    #[test]
    fn merge_is_permutation_deterministic() {
        let w = window();
        let a = reading_at(
            &w,
            datetime!(2026-10-08 01:00:00 UTC),
            Confidence::High,
            "a",
        );
        let b = reading_at(&w, datetime!(2026-10-08 02:00:00 UTC), Confidence::Low, "b");
        let c = reading_at(
            &w,
            datetime!(2026-10-08 01:30:00 UTC),
            Confidence::Medium,
            "c",
        );

        let winner1 = merge_readings(vec![a.clone(), b.clone(), c.clone()])
            .unwrap()
            .winner;
        let winner2 = merge_readings(vec![c.clone(), a.clone(), b.clone()])
            .unwrap()
            .winner;
        let winner3 = merge_readings(vec![b, c, a]).unwrap().winner;
        assert_eq!(winner1, winner2);
        assert_eq!(winner2, winner3);
    }

    #[test]
    fn an_exact_tie_is_resolved_deterministically_and_flagged() {
        let w = window();
        let t = datetime!(2026-10-08 01:00:00 UTC);
        let a = reading_at(&w, t, Confidence::Medium, "aaa");
        let b = reading_at(&w, t, Confidence::Medium, "zzz");

        let outcome1 = merge_readings(vec![a.clone(), b.clone()]).unwrap();
        let outcome2 = merge_readings(vec![b, a]).unwrap();
        assert_eq!(
            outcome1.winner, outcome2.winner,
            "tie-break must be order-independent"
        );
        assert_eq!(
            outcome1.conflicts[0].reason,
            ConflictReason::TieKeptExisting
        );
    }

    #[test]
    fn merge_readings_returns_none_for_an_empty_list() {
        assert!(merge_readings(vec![]).is_none());
    }

    #[test]
    fn resolve_pair_agrees_with_merge_readings_on_a_simple_case() {
        let w = window();
        let older_high = reading_at(
            &w,
            datetime!(2026-10-08 01:00:00 UTC),
            Confidence::High,
            "a",
        );
        let newer_low = reading_at(&w, datetime!(2026-10-08 02:00:00 UTC), Confidence::Low, "b");

        let outcome = resolve_pair(older_high.clone(), newer_low);
        assert_eq!(outcome.winner, older_high);
    }

    /// `resolve_pair` additionally covers the out-of-chronological-order
    /// arrival case `merge_readings` never needs to (it always sorts
    /// first): an `incoming` reading older than `existing`.
    #[test]
    fn resolve_pair_keeps_existing_when_incoming_is_older() {
        let w = window();
        let existing = reading_at(&w, datetime!(2026-10-08 02:00:00 UTC), Confidence::Low, "a");
        let incoming = reading_at(
            &w,
            datetime!(2026-10-08 01:00:00 UTC),
            Confidence::High,
            "b",
        );

        let outcome = resolve_pair(existing.clone(), incoming);
        assert_eq!(outcome.winner, existing);
        assert_eq!(outcome.conflicts[0].reason, ConflictReason::NotNewer);
    }
}
