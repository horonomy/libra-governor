//! Reads a local JSON fixture file and turns it into an
//! [`AcquiredReading`] labeled [`ReadingOrigin::FixtureImport`] — this
//! ticket's actual, buildable delivery path (see the crate's module
//! docs). Never presented as live data: every fixture envelope's
//! `provenance.origin` is forced to [`ReadingOrigin::FixtureImport`] by
//! this module, regardless of what the file itself claims, so a fixture
//! file cannot spoof a different origin.
//!
//! # Exact schema (acceptance criterion 1's "fixture file's own
//! documented schema/contract")
//!
//! ```json
//! {
//!   "schema_version": "quota-window-v1",
//!   "window_id": "<uuid, must match the target QuotaWindow's id>",
//!   "observed_at": "<RFC 3339 UTC timestamp>",
//!   "valid_until": "<RFC 3339 UTC timestamp, optional>",
//!   "declared_reset_at": "<RFC 3339 UTC timestamp, optional>",
//!   "reading": { "state": "undisclosed" }
//!     // or: { "state": "used", "used": { "unit": { "unit": "percent" }, "value": 4200 }, "limit": null }
//!   ,
//!   "confidence": "low" | "medium" | "high",
//!   "provenance": {
//!     "capability": "<free text, e.g. fixture:acme-quota-v1>",
//!     "documented_at": "<free text pointer to this schema's doc>",
//!     "subject": { "kind": "shared_pool", "id": "<pool id>" }
//!       // or { "kind": "principal", "id": "<principal id>" }
//!       // or { "kind": "provider", "id": { "provider": "...", "account": null } }
//!     ,
//!     "trust_owner": "<free text, e.g. a team or role name>"
//!   }
//! }
//! ```
//!
//! `#[serde(deny_unknown_fields)]` on every struct in this module's wire
//! shape, so a fixture file carrying an unexpected field — a `token`, an
//! `authorization`, an `api_key` — is rejected at parse time rather than
//! silently carried along. This is this ticket's concrete, test-owned
//! proof for "no tokens/credentials ever logged, printed, or embedded in
//! any fixture": the schema has structurally nowhere to put one, and an
//! attempt to add one fails the file outright.

use libra_governor_domain::{
    Confidence, GaugeReading, ProviderSnapshot, QuotaSubject, QuotaWindow,
};
use serde::Deserialize;

use crate::provenance::{AcquiredReading, Provenance, ReadingOrigin, SourceDescriptor};
use crate::rfc3339;

#[derive(Debug, thiserror::Error)]
pub enum FixtureError {
    #[error("could not read fixture file {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "fixture file {path} is not valid JSON or does not match the documented schema: {source}"
    )]
    Parse {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("fixture file {path} has an invalid timestamp: {source}")]
    Timestamp {
        path: String,
        #[source]
        source: crate::rfc3339::Rfc3339Error,
    },
    #[error("fixture reading was rejected: {0}")]
    Acquisition(#[from] crate::provenance::AcquisitionError),
    #[error("fixture file {path} does not form a valid provider snapshot: {source}")]
    Snapshot {
        path: String,
        #[source]
        source: libra_governor_domain::QuotaWindowError,
    },
    #[error(
        "fixture file {path} declares schema_version {found:?}, this build understands only {expected:?}"
    )]
    UnsupportedSchemaVersion {
        path: String,
        found: String,
        expected: &'static str,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureProvenance {
    capability: String,
    documented_at: String,
    subject: QuotaSubject,
    trust_owner: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureFile {
    schema_version: String,
    window_id: serde_json::Value,
    observed_at: String,
    #[serde(default)]
    valid_until: Option<String>,
    #[serde(default)]
    declared_reset_at: Option<String>,
    reading: GaugeReading,
    confidence: Confidence,
    provenance: FixtureProvenance,
}

/// Reads and validates `path` against `window` as of `now`, returning an
/// [`AcquiredReading`] labeled [`ReadingOrigin::FixtureImport`].
pub fn import_fixture(
    path: &std::path::Path,
    window: &QuotaWindow,
    now: time::OffsetDateTime,
) -> Result<AcquiredReading, FixtureError> {
    let path_display = path.display().to_string();
    let text = std::fs::read_to_string(path).map_err(|source| FixtureError::Read {
        path: path_display.clone(),
        source,
    })?;
    let fixture: FixtureFile =
        serde_json::from_str(&text).map_err(|source| FixtureError::Parse {
            path: path_display.clone(),
            source,
        })?;
    if fixture.schema_version != libra_governor_domain::QUOTA_WINDOW_SCHEMA_VERSION {
        return Err(FixtureError::UnsupportedSchemaVersion {
            path: path_display,
            found: fixture.schema_version,
            expected: libra_governor_domain::QUOTA_WINDOW_SCHEMA_VERSION,
        });
    }

    let observed_at =
        rfc3339::parse(&fixture.observed_at).map_err(|source| FixtureError::Timestamp {
            path: path_display.clone(),
            source,
        })?;
    let valid_until = fixture
        .valid_until
        .as_deref()
        .map(rfc3339::parse)
        .transpose()
        .map_err(|source| FixtureError::Timestamp {
            path: path_display.clone(),
            source,
        })?;
    let declared_reset_at = fixture
        .declared_reset_at
        .as_deref()
        .map(rfc3339::parse)
        .transpose()
        .map_err(|source| FixtureError::Timestamp {
            path: path_display.clone(),
            source,
        })?;

    // `window_id` round-trips through the domain crate's own
    // `QuotaWindowId` (de)serialization rather than this module parsing
    // the UUID itself, so any future change to that wire shape only ever
    // needs updating in one place.
    let window_id: libra_governor_domain::QuotaWindowId = serde_json::from_value(fixture.window_id)
        .map_err(|source| FixtureError::Parse {
            path: path_display.clone(),
            source,
        })?;

    let snapshot = ProviderSnapshot::validated(
        window_id,
        observed_at,
        valid_until,
        declared_reset_at,
        fixture.reading,
        fixture.confidence,
    )
    .map_err(|source| FixtureError::Snapshot {
        path: path_display.clone(),
        source,
    })?;

    let provenance = Provenance {
        // Forced regardless of anything the file claims — see this
        // module's own docs.
        origin: ReadingOrigin::FixtureImport,
        source: SourceDescriptor {
            capability: fixture.provenance.capability,
            documented_at: fixture.provenance.documented_at,
        },
        subject: fixture.provenance.subject,
        trust_owner: fixture.provenance.trust_owner,
    };

    Ok(AcquiredReading::validated(
        window, snapshot, provenance, now,
    )?)
}

#[cfg(test)]
mod tests {
    use libra_governor_domain::{
        EntitlementSource, PoolId, QuotaScope, QuotaUnit, QuotaWindowId, WindowKind,
    };
    use time::macros::datetime;

    use super::*;

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
                max_staleness_secs: 86_400,
            },
        )
        .unwrap()
    }

    fn write_fixture(dir: &tempfile::TempDir, body: &serde_json::Value) -> std::path::PathBuf {
        let path = dir.path().join("fixture.json");
        std::fs::write(&path, serde_json::to_string_pretty(body).unwrap()).unwrap();
        path
    }

    #[test]
    fn imports_a_well_formed_fixture() {
        let window = gauge_window();
        let dir = tempfile::tempdir().unwrap();
        let body = serde_json::json!({
            "schema_version": "quota-window-v1",
            "window_id": window.id(),
            "observed_at": "2026-10-08T00:30:00Z",
            "reading": { "state": "used", "used": { "unit": { "unit": "percent" }, "value": 2500 }, "limit": null },
            "confidence": "medium",
            "provenance": {
                "capability": "fixture:acme-quota-v1",
                "documented_at": "docs/quota-source-fixture.md",
                "subject": { "kind": "shared_pool", "id": "acme" },
                "trust_owner": "platform-team"
            }
        });
        let path = write_fixture(&dir, &body);
        let reading = import_fixture(&path, &window, datetime!(2026-10-08 01:00:00 UTC)).unwrap();
        assert_eq!(reading.provenance().origin, ReadingOrigin::FixtureImport);
    }

    /// The core "no tokens/credentials in any fixture" proof: an
    /// unexpected field is rejected at parse time, not silently carried.
    #[test]
    fn rejects_a_fixture_with_an_unexpected_field() {
        let window = gauge_window();
        let dir = tempfile::tempdir().unwrap();
        let body = serde_json::json!({
            "schema_version": "quota-window-v1",
            "window_id": window.id(),
            "observed_at": "2026-10-08T00:30:00Z",
            "reading": { "state": "undisclosed" },
            "confidence": "low",
            "token": "PLACEHOLDER-NOT-A-SECRET",
            "provenance": {
                "capability": "fixture:acme-quota-v1",
                "documented_at": "docs/quota-source-fixture.md",
                "subject": { "kind": "shared_pool", "id": "acme" },
                "trust_owner": "platform-team"
            }
        });
        let path = write_fixture(&dir, &body);
        let err = import_fixture(&path, &window, datetime!(2026-10-08 01:00:00 UTC)).unwrap_err();
        assert!(matches!(err, FixtureError::Parse { .. }));
    }

    #[test]
    fn rejects_a_fixture_provenance_with_an_unexpected_field() {
        let window = gauge_window();
        let dir = tempfile::tempdir().unwrap();
        let body = serde_json::json!({
            "schema_version": "quota-window-v1",
            "window_id": window.id(),
            "observed_at": "2026-10-08T00:30:00Z",
            "reading": { "state": "undisclosed" },
            "confidence": "low",
            "provenance": {
                "capability": "fixture:acme-quota-v1",
                "documented_at": "docs/quota-source-fixture.md",
                "subject": { "kind": "shared_pool", "id": "acme" },
                "trust_owner": "platform-team",
                "authorization": "Bearer PLACEHOLDER-NOT-A-SECRET"
            }
        });
        let path = write_fixture(&dir, &body);
        let err = import_fixture(&path, &window, datetime!(2026-10-08 01:00:00 UTC)).unwrap_err();
        assert!(matches!(err, FixtureError::Parse { .. }));
    }

    #[test]
    fn rejects_a_subject_mismatch_from_a_fixture() {
        let window = gauge_window();
        let dir = tempfile::tempdir().unwrap();
        let body = serde_json::json!({
            "schema_version": "quota-window-v1",
            "window_id": window.id(),
            "observed_at": "2026-10-08T00:30:00Z",
            "reading": { "state": "undisclosed" },
            "confidence": "low",
            "provenance": {
                "capability": "fixture:acme-quota-v1",
                "documented_at": "docs/quota-source-fixture.md",
                "subject": { "kind": "shared_pool", "id": "someone-elses-pool" },
                "trust_owner": "platform-team"
            }
        });
        let path = write_fixture(&dir, &body);
        let err = import_fixture(&path, &window, datetime!(2026-10-08 01:00:00 UTC)).unwrap_err();
        assert!(matches!(
            err,
            FixtureError::Acquisition(crate::provenance::AcquisitionError::SubjectMismatch)
        ));
    }

    #[test]
    fn rejects_an_unparseable_timestamp() {
        let window = gauge_window();
        let dir = tempfile::tempdir().unwrap();
        let body = serde_json::json!({
            "schema_version": "quota-window-v1",
            "window_id": window.id(),
            "observed_at": "not-a-timestamp",
            "reading": { "state": "undisclosed" },
            "confidence": "low",
            "provenance": {
                "capability": "fixture:acme-quota-v1",
                "documented_at": "docs/quota-source-fixture.md",
                "subject": { "kind": "shared_pool", "id": "acme" },
                "trust_owner": "platform-team"
            }
        });
        let path = write_fixture(&dir, &body);
        let err = import_fixture(&path, &window, datetime!(2026-10-08 01:00:00 UTC)).unwrap_err();
        assert!(matches!(err, FixtureError::Timestamp { .. }));
    }

    #[test]
    fn rejects_an_unsupported_schema_version() {
        let window = gauge_window();
        let dir = tempfile::tempdir().unwrap();
        let body = serde_json::json!({
            "schema_version": "quota-window-v999-from-the-future",
            "window_id": window.id(),
            "observed_at": "2026-10-08T00:30:00Z",
            "reading": { "state": "undisclosed" },
            "confidence": "low",
            "provenance": {
                "capability": "fixture:acme-quota-v1",
                "documented_at": "docs/quota-source-fixture.md",
                "subject": { "kind": "shared_pool", "id": "acme" },
                "trust_owner": "platform-team"
            }
        });
        let path = write_fixture(&dir, &body);
        let err = import_fixture(&path, &window, datetime!(2026-10-08 01:00:00 UTC)).unwrap_err();
        assert!(matches!(err, FixtureError::UnsupportedSchemaVersion { .. }));
    }

    #[test]
    fn rejects_a_missing_file() {
        let window = gauge_window();
        let path = std::path::PathBuf::from("/does/not/exist/fixture.json");
        let err = import_fixture(&path, &window, datetime!(2026-10-08 01:00:00 UTC)).unwrap_err();
        assert!(matches!(err, FixtureError::Read { .. }));
    }
}
