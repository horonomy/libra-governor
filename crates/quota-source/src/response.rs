//! A **pure** mapping from a caller-supplied, already-received response
//! shape to a safe [`SourceState`]. Nothing in this module makes a
//! network call, follows a redirect, or retries anything — it is a
//! function from data to data, which is exactly why it can be tested
//! exhaustively with synthetic inputs and is covered by
//! `tests/no_network_symbols.rs`'s static scan.
//!
//! Acceptance criterion 2: an HTTP error, a 401, a stale reading, an
//! unknown/mismatched unit, or a redirect to a domain other than the one
//! single authorized endpoint must all yield [`SourceState::Unavailable`]
//! or a read-only [`SourceState::Degraded`] — never silently
//! [`SourceState::Admissible`].

use libra_governor_domain::{
    decode_provider_snapshot, DecodedProviderSnapshot, GaugeReading, ProviderSnapshot, QuotaUnit,
    QuotaWindow, WindowKind,
};

use crate::provenance::{AcquiredReading, AcquisitionError, Provenance};

/// A response shape this crate knows how to interpret, built entirely by
/// the caller (a test, or a future fixture/simulator) — never produced by
/// an HTTP client this crate owns, since it owns none.
#[derive(Debug, Clone)]
pub enum RawSourceResponse {
    /// A non-2xx status other than 401 (which gets its own variant so
    /// callers never have to remember the magic number).
    HttpError { status: u16 },
    /// An explicit 401 Unauthorized.
    Unauthorized,
    /// The response was a redirect to `to_origin` (scheme + host\[:port\],
    /// e.g. `"https://proxy.example.invalid"`). This crate has no HTTP
    /// client and therefore never follows any redirect, authorized origin
    /// or not — see [`SourceState::Unavailable`]'s
    /// [`UnavailableReason::RedirectNotFollowed`] for the same-origin case
    /// and [`UnavailableReason::ExternalRedirect`] for the
    /// outside-the-authorized-endpoint case this acceptance criterion
    /// names explicitly.
    Redirect { to_origin: String },
    /// A successful body to decode as a [`ProviderSnapshot`] via
    /// [`decode_provider_snapshot`].
    Body(serde_json::Value),
}

/// Why a reading was refused outright.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UnavailableReason {
    #[error("HTTP error status {status}")]
    HttpError { status: u16 },
    #[error("authentication failed (401)")]
    Unauthorized,
    #[error("redirected to an external domain {origin:?}, outside the one authorized endpoint")]
    ExternalRedirect { origin: String },
    #[error(
        "redirected to {origin:?}; this adapter has no HTTP client and never follows a redirect"
    )]
    RedirectNotFollowed { origin: String },
    #[error("response body did not decode as a provider snapshot: {detail}")]
    MalformedResponse { detail: String },
    #[error("response declared an unsupported schema_version {schema_version:?}")]
    UnsupportedSchemaVersion { schema_version: String },
    #[error("reading unit {got:?} does not match the target window's unit {expected:?}")]
    UnknownUnit { expected: QuotaUnit, got: QuotaUnit },
    #[error("reading rejected: {0}")]
    Invalid(#[from] AcquisitionError),
}

/// Why a reading is visible but not trusted for an admission decision —
/// mirrors `quota_window::evaluate::GaugeState::trusted_reading`'s own
/// "stale stays visible, never trusted" discipline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DegradedReason {
    /// Older than the window's own `max_staleness_secs`.
    Stale {
        age_secs: u64,
        max_staleness_secs: u64,
    },
}

/// The outcome of [`ingest_response`]. Only [`Self::Admissible`] carries a
/// value an admission decision may actually trust — mirrors
/// `GaugeState::trusted_reading`'s "stale stays visible in `latest` but is
/// never trusted" rule one level up, at the acquisition boundary rather
/// than the evaluation boundary.
#[derive(Debug, Clone, PartialEq)]
pub enum SourceState {
    Admissible(AcquiredReading),
    /// Visible for display/debugging only — never fed to an admission
    /// decision as if it were current.
    Degraded {
        reason: DegradedReason,
        snapshot: ProviderSnapshot,
    },
    Unavailable(UnavailableReason),
}

impl SourceState {
    /// `Some` only for [`Self::Admissible`] — the one state an admission
    /// decision may actually use.
    pub fn admissible_reading(&self) -> Option<&AcquiredReading> {
        match self {
            SourceState::Admissible(reading) => Some(reading),
            _ => None,
        }
    }
}

/// Maps `response` to a safe [`SourceState`] for `window`, never
/// producing [`SourceState::Admissible`] except for a reading that is
/// fresh, correctly scoped, and internally consistent.
///
/// `authorized_origin` is the one endpoint origin this call site is
/// authorized to have received a response from (e.g.
/// `"https://proxy.example.invalid"`) — supplied by the caller, never
/// guessed or defaulted by this function, since this crate has no
/// endpoint configuration of its own (see [`crate::authorization`]).
pub fn ingest_response(
    window: &QuotaWindow,
    provenance: Provenance,
    authorized_origin: &str,
    response: &RawSourceResponse,
    now: time::OffsetDateTime,
) -> SourceState {
    match response {
        RawSourceResponse::HttpError { status } => {
            SourceState::Unavailable(UnavailableReason::HttpError { status: *status })
        }
        RawSourceResponse::Unauthorized => {
            SourceState::Unavailable(UnavailableReason::Unauthorized)
        }
        RawSourceResponse::Redirect { to_origin } => {
            if to_origin == authorized_origin {
                SourceState::Unavailable(UnavailableReason::RedirectNotFollowed {
                    origin: to_origin.clone(),
                })
            } else {
                SourceState::Unavailable(UnavailableReason::ExternalRedirect {
                    origin: to_origin.clone(),
                })
            }
        }
        RawSourceResponse::Body(raw) => ingest_body(window, provenance, raw, now),
    }
}

fn ingest_body(
    window: &QuotaWindow,
    provenance: Provenance,
    raw: &serde_json::Value,
    now: time::OffsetDateTime,
) -> SourceState {
    let decoded = match decode_provider_snapshot(raw) {
        Ok(decoded) => decoded,
        Err(e) => {
            return SourceState::Unavailable(UnavailableReason::MalformedResponse {
                detail: e.to_string(),
            })
        }
    };
    let snapshot = match decoded {
        DecodedProviderSnapshot::Current(snapshot) => snapshot,
        DecodedProviderSnapshot::Unsupported { schema_version, .. } => {
            return SourceState::Unavailable(UnavailableReason::UnsupportedSchemaVersion {
                schema_version,
            })
        }
    };

    // Unit mismatch — the window's own evaluator (`evaluate_gauge`) would
    // report this as `SnapshotUnitMismatch`; this acquisition boundary
    // refuses the reading outright rather than letting a wrong-unit
    // reading ever reach the evidence a decision is evaluated against.
    if let GaugeReading::Used { used, .. } = snapshot.reading() {
        if used.unit != *window.unit() {
            return SourceState::Unavailable(UnavailableReason::UnknownUnit {
                expected: window.unit().clone(),
                got: used.unit.clone(),
            });
        }
    }

    let max_staleness_secs = match window.kind() {
        WindowKind::OpaqueProviderSnapshot { max_staleness_secs } => *max_staleness_secs,
        _ => {
            // AcquiredReading::validated below will refuse this with the
            // precise WindowKindMismatch error; no staleness concept
            // applies to a non-gauge window kind.
            0
        }
    };

    match AcquiredReading::validated(window, snapshot, provenance, now) {
        Ok(reading) => {
            let age = now - reading.snapshot().observed_at();
            let age_secs = age.whole_seconds().max(0) as u64;
            if age_secs > max_staleness_secs {
                SourceState::Degraded {
                    reason: DegradedReason::Stale {
                        age_secs,
                        max_staleness_secs,
                    },
                    snapshot: reading.snapshot().clone(),
                }
            } else {
                SourceState::Admissible(reading)
            }
        }
        Err(e) => SourceState::Unavailable(UnavailableReason::Invalid(e)),
    }
}

#[cfg(test)]
mod tests {
    use libra_governor_domain::{
        Confidence, EntitlementSource, GaugeReading, PoolId, QuotaScope, QuotaSubject, QuotaUnit,
        QuotaWindowId, WindowKind,
    };
    use time::macros::datetime;

    use super::*;
    use crate::provenance::{ReadingOrigin, SourceDescriptor};

    const AUTHORIZED: &str = "https://proxy.example.invalid";

    fn gauge_window() -> QuotaWindow {
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
                max_staleness_secs: 300,
            },
        )
        .unwrap()
    }

    fn provenance() -> Provenance {
        Provenance {
            origin: ReadingOrigin::FixtureImport,
            source: SourceDescriptor {
                capability: "fixture:test".to_string(),
                documented_at: "docs/quota-source-fixture.md".to_string(),
            },
            subject: QuotaSubject::SharedPool(PoolId("acme".to_string())),
            trust_owner: "test-owner".to_string(),
        }
    }

    fn snapshot_json(window: &QuotaWindow, observed_at: &str) -> serde_json::Value {
        serde_json::json!({
            "schema_version": "quota-window-v1",
            "window_id": window.id(),
            "observed_at": observed_at,
            "reading": { "state": "used", "used": { "unit": { "unit": "percent" }, "value": 500 }, "limit": null },
            "confidence": "medium",
        })
    }

    #[test]
    fn http_error_is_unavailable() {
        let window = gauge_window();
        let state = ingest_response(
            &window,
            provenance(),
            AUTHORIZED,
            &RawSourceResponse::HttpError { status: 500 },
            datetime!(2026-10-08 01:00:00 UTC),
        );
        assert!(matches!(
            state,
            SourceState::Unavailable(UnavailableReason::HttpError { status: 500 })
        ));
        assert!(state.admissible_reading().is_none());
    }

    #[test]
    fn a_401_is_unavailable() {
        let window = gauge_window();
        let state = ingest_response(
            &window,
            provenance(),
            AUTHORIZED,
            &RawSourceResponse::Unauthorized,
            datetime!(2026-10-08 01:00:00 UTC),
        );
        assert!(matches!(
            state,
            SourceState::Unavailable(UnavailableReason::Unauthorized)
        ));
    }

    #[test]
    fn a_redirect_to_an_external_domain_is_unavailable() {
        let window = gauge_window();
        let state = ingest_response(
            &window,
            provenance(),
            AUTHORIZED,
            &RawSourceResponse::Redirect {
                to_origin: "https://attacker.example.invalid".to_string(),
            },
            datetime!(2026-10-08 01:00:00 UTC),
        );
        assert!(matches!(
            state,
            SourceState::Unavailable(UnavailableReason::ExternalRedirect { .. })
        ));
    }

    #[test]
    fn a_redirect_to_the_authorized_origin_is_still_not_followed() {
        let window = gauge_window();
        let state = ingest_response(
            &window,
            provenance(),
            AUTHORIZED,
            &RawSourceResponse::Redirect {
                to_origin: AUTHORIZED.to_string(),
            },
            datetime!(2026-10-08 01:00:00 UTC),
        );
        assert!(matches!(
            state,
            SourceState::Unavailable(UnavailableReason::RedirectNotFollowed { .. })
        ));
    }

    #[test]
    fn a_stale_reading_is_degraded_not_admissible() {
        let window = gauge_window(); // max_staleness_secs: 300
        let body = snapshot_json(&window, "2026-10-08T00:00:00Z");
        let state = ingest_response(
            &window,
            provenance(),
            AUTHORIZED,
            &RawSourceResponse::Body(body),
            datetime!(2026-10-08 01:00:00 UTC), // 1 hour later, far past 300s
        );
        assert!(matches!(
            state,
            SourceState::Degraded {
                reason: DegradedReason::Stale { .. },
                ..
            }
        ));
        assert!(state.admissible_reading().is_none());
    }

    #[test]
    fn a_fresh_well_formed_reading_is_admissible() {
        let window = gauge_window();
        let body = snapshot_json(&window, "2026-10-08T00:59:30Z");
        let state = ingest_response(
            &window,
            provenance(),
            AUTHORIZED,
            &RawSourceResponse::Body(body),
            datetime!(2026-10-08 01:00:00 UTC),
        );
        assert!(state.admissible_reading().is_some());
    }

    /// Acceptance criterion 2: "unknown unit" — here, a reading whose
    /// unit does not match the target window's declared unit must never
    /// be admitted.
    #[test]
    fn a_unit_mismatch_is_unavailable() {
        let window = gauge_window(); // unit: Percent
        let body = serde_json::json!({
            "schema_version": "quota-window-v1",
            "window_id": window.id(),
            "observed_at": "2026-10-08T00:59:30Z",
            "reading": { "state": "used", "used": { "unit": { "unit": "tokens" }, "value": 500 }, "limit": 1000 },
            "confidence": "medium",
        });
        let state = ingest_response(
            &window,
            provenance(),
            AUTHORIZED,
            &RawSourceResponse::Body(body),
            datetime!(2026-10-08 01:00:00 UTC),
        );
        assert!(matches!(
            state,
            SourceState::Unavailable(UnavailableReason::UnknownUnit { .. })
        ));
    }

    #[test]
    fn an_unsupported_schema_version_is_unavailable() {
        let window = gauge_window();
        let body = serde_json::json!({
            "schema_version": "quota-window-v999-from-the-future",
            "window_id": window.id(),
            "observed_at": "2026-10-08T00:59:30Z",
            "reading": { "state": "undisclosed" },
            "confidence": "low",
        });
        let state = ingest_response(
            &window,
            provenance(),
            AUTHORIZED,
            &RawSourceResponse::Body(body),
            datetime!(2026-10-08 01:00:00 UTC),
        );
        assert!(matches!(
            state,
            SourceState::Unavailable(UnavailableReason::UnsupportedSchemaVersion { .. })
        ));
    }

    #[test]
    fn a_malformed_body_is_unavailable() {
        let window = gauge_window();
        let body = serde_json::json!({ "not": "a snapshot" });
        let state = ingest_response(
            &window,
            provenance(),
            AUTHORIZED,
            &RawSourceResponse::Body(body),
            datetime!(2026-10-08 01:00:00 UTC),
        );
        assert!(matches!(
            state,
            SourceState::Unavailable(UnavailableReason::MalformedResponse { .. })
        ));
    }

    /// An undisclosed reading must still be admissible as data (the
    /// window's own evaluator treats `Undisclosed` as `Indeterminate`,
    /// never as available) — this acquisition boundary must not invent a
    /// 100%-idle default by refusing it outright either.
    #[test]
    fn an_undisclosed_reading_is_admissible_as_data_not_assumed_available() {
        let window = gauge_window();
        let body = serde_json::json!({
            "schema_version": "quota-window-v1",
            "window_id": window.id(),
            "observed_at": "2026-10-08T00:59:30Z",
            "reading": { "state": "undisclosed" },
            "confidence": "low",
        });
        let state = ingest_response(
            &window,
            provenance(),
            AUTHORIZED,
            &RawSourceResponse::Body(body),
            datetime!(2026-10-08 01:00:00 UTC),
        );
        let reading = state.admissible_reading().expect("must be admissible data");
        assert!(matches!(
            reading.snapshot().reading(),
            GaugeReading::Undisclosed
        ));
    }
}
