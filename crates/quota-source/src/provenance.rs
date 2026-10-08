//! [`AcquiredReading`] — a HORO-1762 [`ProviderSnapshot`] wrapped with the
//! provenance this ticket's acceptance criterion 1 requires: exact
//! source/capability, scope (via the snapshot's unit and the window it is
//! validated against), freshness (already on `ProviderSnapshot` itself),
//! and a trust owner. Constructed only via [`AcquiredReading::validated`],
//! so an invalid reading — wrong window, wrong subject, a bogus
//! timestamp — can never exist, mirroring the discipline
//! `libra_governor_domain::quota_window::QuotaWindow::validated` already
//! established.

use libra_governor_domain::{ProviderSnapshot, QuotaSubject, QuotaWindow, WindowKind};
use serde::{Deserialize, Serialize};

/// Where an [`AcquiredReading`] came from. Never a third, implicit "live"
/// variant in this crate — see the crate's module docs on why no live
/// adapter exists here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadingOrigin {
    /// Imported from a local JSON fixture file via [`crate::fixture`],
    /// clearly labeled non-live.
    FixtureImport,
    /// Entered by an operator by hand (e.g. via a future CLI command),
    /// not read from any file or endpoint.
    ManualImport,
}

/// Exactly which capability produced a reading — acceptance criterion 1's
/// "document exact API/capability". Free-text by design: this crate has
/// no live endpoint to derive a capability identifier from, so the
/// fixture/manual-import path must say so explicitly rather than this
/// type inventing a fake API name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceDescriptor {
    /// e.g. `"fixture:quota-source-fixture-v1"` or
    /// `"manual:operator-entered"` — never a real endpoint path, since
    /// none exists in this build.
    pub capability: String,
    /// A human-readable note on where this capability's contract is
    /// documented (e.g. `"docs/quota-source-fixture.md"`).
    pub documented_at: String,
}

/// The provenance envelope acceptance criterion 1 requires alongside a
/// `ProviderSnapshot`: who/what the reading claims to be for, who
/// produced it, and who is responsible for trusting it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    pub origin: ReadingOrigin,
    pub source: SourceDescriptor,
    /// Who this reading claims to describe. Checked against the target
    /// window's own `QuotaScope::subject` in [`AcquiredReading::validated`]
    /// — acceptance criterion 5's "incorrect principal attribution".
    pub subject: QuotaSubject,
    /// Who owns the decision to trust this reading (an operator name,
    /// team, or role — never a credential, never a secret).
    pub trust_owner: String,
}

/// Every way [`AcquiredReading::validated`] refuses to construct a
/// reading. Each variant names a concrete acceptance-criterion failure
/// mode this type exists to catch rather than silently accept.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AcquisitionError {
    /// The target window is not an `OpaqueProviderSnapshot` window — a
    /// `ProviderSnapshot` is only ever evaluated against that one window
    /// kind (`quota_window::evaluate::evaluate_gauge`); feeding one to a
    /// period/bucket window would simply be ignored by evaluation, which
    /// would silently hide a real reading rather than reject it loudly.
    #[error("target window {window_id:?} is not an OpaqueProviderSnapshot window")]
    WindowKindMismatch {
        window_id: libra_governor_domain::QuotaWindowId,
    },
    /// The snapshot's own `window_id` does not match the window it is
    /// being validated against.
    #[error("snapshot window_id {snapshot_window_id:?} does not match target window {target_window_id:?}")]
    WindowIdMismatch {
        snapshot_window_id: libra_governor_domain::QuotaWindowId,
        target_window_id: libra_governor_domain::QuotaWindowId,
    },
    /// Acceptance criterion 5: "incorrect principal attribution" — the
    /// provenance's claimed subject does not match the window's own
    /// declared entitlement subject.
    #[error("provenance subject does not match the target window's entitlement subject")]
    SubjectMismatch,
    /// The snapshot claims to have been observed in the future relative
    /// to `now` — never trusted, since it cannot have really happened yet.
    #[error("observed_at is in the future relative to the evaluation instant")]
    FutureObservedAt,
    /// Acceptance criterion 5: "bogus reset timestamp" — `valid_until` at
    /// or before `observed_at` is not a valid freshness window.
    #[error("valid_until ({valid_until:?}) is not after observed_at ({observed_at:?})")]
    BogusValidUntil {
        observed_at: time::OffsetDateTime,
        valid_until: time::OffsetDateTime,
    },
    /// Acceptance criterion 5's bogus-reset-timestamp class, applied to
    /// the provider's own declared reset claim.
    #[error(
        "declared_reset_at ({declared_reset_at:?}) is not after observed_at ({observed_at:?})"
    )]
    BogusDeclaredResetAt {
        observed_at: time::OffsetDateTime,
        declared_reset_at: time::OffsetDateTime,
    },
}

/// A HORO-1762 [`ProviderSnapshot`] plus the provenance this ticket's
/// acceptance criteria require. Construct only via [`Self::validated`].
#[derive(Debug, Clone, PartialEq)]
pub struct AcquiredReading {
    snapshot: ProviderSnapshot,
    provenance: Provenance,
}

impl AcquiredReading {
    /// Validates `snapshot`/`provenance` against `window` as of `now`.
    /// Every check is a pure comparison — no I/O, no clock access beyond
    /// the caller-supplied `now` (mirrors `QuotaWindow::evaluate`'s own
    /// discipline of taking `now` as an explicit argument rather than
    /// reading the system clock itself, which is why this function is
    /// trivially testable with fixed timestamps).
    pub fn validated(
        window: &QuotaWindow,
        snapshot: ProviderSnapshot,
        provenance: Provenance,
        now: time::OffsetDateTime,
    ) -> Result<Self, AcquisitionError> {
        if !matches!(window.kind(), WindowKind::OpaqueProviderSnapshot { .. }) {
            return Err(AcquisitionError::WindowKindMismatch {
                window_id: window.id(),
            });
        }
        if snapshot.window_id() != window.id() {
            return Err(AcquisitionError::WindowIdMismatch {
                snapshot_window_id: snapshot.window_id(),
                target_window_id: window.id(),
            });
        }
        if provenance.subject != window.scope().subject {
            return Err(AcquisitionError::SubjectMismatch);
        }
        if snapshot.observed_at() > now {
            return Err(AcquisitionError::FutureObservedAt);
        }
        if let Some(valid_until) = snapshot.valid_until() {
            if valid_until <= snapshot.observed_at() {
                return Err(AcquisitionError::BogusValidUntil {
                    observed_at: snapshot.observed_at(),
                    valid_until,
                });
            }
        }
        if let Some(declared_reset_at) = snapshot.declared_reset_at() {
            if declared_reset_at <= snapshot.observed_at() {
                return Err(AcquisitionError::BogusDeclaredResetAt {
                    observed_at: snapshot.observed_at(),
                    declared_reset_at,
                });
            }
        }
        Ok(Self {
            snapshot,
            provenance,
        })
    }

    pub fn snapshot(&self) -> &ProviderSnapshot {
        &self.snapshot
    }

    pub fn provenance(&self) -> &Provenance {
        &self.provenance
    }

    /// Whether `window` would actually count this reading's `QuotaSubject`
    /// as belonging to it, for an extra defense-in-depth check at call
    /// sites that hold both values (`validated` already enforces this at
    /// construction time; this is a read-only re-check, e.g. after a
    /// reading has been stored and is being replayed against a window
    /// loaded from a different source).
    pub fn matches_subject(&self, subject: &QuotaSubject) -> bool {
        &self.provenance.subject == subject
    }
}

#[cfg(test)]
mod tests {
    use libra_governor_domain::{
        AlignedPeriod, Confidence, EntitlementSource, GaugeReading, IanaTimeZone, PoolId,
        QuotaAmount, QuotaScope, QuotaSubject, QuotaUnit, QuotaWindowId, WindowKind,
    };
    use time::macros::datetime;

    use super::*;

    fn gauge_window(subject: QuotaSubject) -> QuotaWindow {
        QuotaWindow::validated(
            QuotaWindowId::new(),
            QuotaScope {
                subject,
                source: EntitlementSource::ProviderDeclared,
                confidence: Confidence::Medium,
                observed_at: datetime!(2026-10-08 00:00:00 UTC),
                valid_until: None,
            },
            QuotaUnit::Percent,
            WindowKind::OpaqueProviderSnapshot {
                max_staleness_secs: 3600,
            },
        )
        .unwrap()
    }

    fn period_window(subject: QuotaSubject) -> QuotaWindow {
        QuotaWindow::validated(
            QuotaWindowId::new(),
            QuotaScope {
                subject,
                source: EntitlementSource::OperatorConfigured,
                confidence: Confidence::High,
                observed_at: datetime!(2026-10-08 00:00:00 UTC),
                valid_until: None,
            },
            QuotaUnit::Tokens,
            WindowKind::FixedAligned {
                period: AlignedPeriod::Hour,
                time_zone: IanaTimeZone::new("UTC").unwrap(),
                limit: 1000,
            },
        )
        .unwrap()
    }

    fn snapshot_for(window: &QuotaWindow, observed_at: time::OffsetDateTime) -> ProviderSnapshot {
        ProviderSnapshot::validated(
            window.id(),
            observed_at,
            None,
            None,
            GaugeReading::Used {
                used: QuotaAmount::new(QuotaUnit::Percent, 1000),
                limit: None,
            },
            Confidence::Medium,
        )
        .unwrap()
    }

    fn provenance(subject: QuotaSubject) -> Provenance {
        Provenance {
            origin: ReadingOrigin::FixtureImport,
            source: SourceDescriptor {
                capability: "fixture:test".to_string(),
                documented_at: "docs/quota-source-fixture.md".to_string(),
            },
            subject,
            trust_owner: "test-owner".to_string(),
        }
    }

    fn pool(name: &str) -> QuotaSubject {
        QuotaSubject::SharedPool(PoolId(name.to_string()))
    }

    #[test]
    fn a_well_formed_reading_validates() {
        let window = gauge_window(pool("acme"));
        let snapshot = snapshot_for(&window, datetime!(2026-10-08 01:00:00 UTC));
        let reading = AcquiredReading::validated(
            &window,
            snapshot,
            provenance(pool("acme")),
            datetime!(2026-10-08 01:05:00 UTC),
        )
        .unwrap();
        assert!(reading.matches_subject(&pool("acme")));
    }

    #[test]
    fn rejects_a_non_gauge_window() {
        let window = period_window(pool("acme"));
        let snapshot = ProviderSnapshot::validated(
            window.id(),
            datetime!(2026-10-08 01:00:00 UTC),
            None,
            None,
            GaugeReading::Undisclosed,
            Confidence::Low,
        )
        .unwrap();
        let err = AcquiredReading::validated(
            &window,
            snapshot,
            provenance(pool("acme")),
            datetime!(2026-10-08 01:05:00 UTC),
        )
        .unwrap_err();
        assert!(matches!(err, AcquisitionError::WindowKindMismatch { .. }));
    }

    #[test]
    fn rejects_a_mismatched_window_id() {
        let window = gauge_window(pool("acme"));
        let other_window = gauge_window(pool("acme"));
        let snapshot = snapshot_for(&other_window, datetime!(2026-10-08 01:00:00 UTC));
        let err = AcquiredReading::validated(
            &window,
            snapshot,
            provenance(pool("acme")),
            datetime!(2026-10-08 01:05:00 UTC),
        )
        .unwrap_err();
        assert!(matches!(err, AcquisitionError::WindowIdMismatch { .. }));
    }

    /// Acceptance criterion 5: incorrect principal attribution must be
    /// rejected, not silently accepted.
    #[test]
    fn rejects_a_subject_mismatch() {
        let window = gauge_window(pool("acme"));
        let snapshot = snapshot_for(&window, datetime!(2026-10-08 01:00:00 UTC));
        let err = AcquiredReading::validated(
            &window,
            snapshot,
            provenance(pool("someone-elses-pool")),
            datetime!(2026-10-08 01:05:00 UTC),
        )
        .unwrap_err();
        assert_eq!(err, AcquisitionError::SubjectMismatch);
    }

    #[test]
    fn rejects_an_observed_at_in_the_future() {
        let window = gauge_window(pool("acme"));
        let snapshot = snapshot_for(&window, datetime!(2026-10-08 02:00:00 UTC));
        let err = AcquiredReading::validated(
            &window,
            snapshot,
            provenance(pool("acme")),
            datetime!(2026-10-08 01:00:00 UTC),
        )
        .unwrap_err();
        assert_eq!(err, AcquisitionError::FutureObservedAt);
    }

    /// Acceptance criterion 5: a bogus reset timestamp (`valid_until` at
    /// or before `observed_at`) must be rejected.
    #[test]
    fn rejects_a_valid_until_at_or_before_observed_at() {
        let window = gauge_window(pool("acme"));
        let observed_at = datetime!(2026-10-08 01:00:00 UTC);
        let snapshot = ProviderSnapshot::validated(
            window.id(),
            observed_at,
            Some(observed_at), // bogus: not after observed_at
            None,
            GaugeReading::Used {
                used: QuotaAmount::new(QuotaUnit::Percent, 1000),
                limit: None,
            },
            Confidence::Medium,
        )
        .unwrap();
        let err = AcquiredReading::validated(
            &window,
            snapshot,
            provenance(pool("acme")),
            datetime!(2026-10-08 01:05:00 UTC),
        )
        .unwrap_err();
        assert!(matches!(err, AcquisitionError::BogusValidUntil { .. }));
    }

    #[test]
    fn rejects_a_declared_reset_at_before_observed_at() {
        let window = gauge_window(pool("acme"));
        let observed_at = datetime!(2026-10-08 01:00:00 UTC);
        let bogus_reset = datetime!(2026-10-07 00:00:00 UTC);
        let snapshot = ProviderSnapshot::validated(
            window.id(),
            observed_at,
            None,
            Some(bogus_reset),
            GaugeReading::Used {
                used: QuotaAmount::new(QuotaUnit::Percent, 1000),
                limit: None,
            },
            Confidence::Medium,
        )
        .unwrap();
        let err = AcquiredReading::validated(
            &window,
            snapshot,
            provenance(pool("acme")),
            datetime!(2026-10-08 01:05:00 UTC),
        )
        .unwrap_err();
        assert!(matches!(err, AcquisitionError::BogusDeclaredResetAt { .. }));
    }
}
