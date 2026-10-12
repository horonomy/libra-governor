//! Freshness evaluation and account-binding enforcement (HORO-1853
//! design-v2).
//!
//! A constraint's reading is "fresh" only if **all** of:
//! - `observed_at <= now + 5s` — strict future-timestamp rejection, with
//!   zero tolerance except a documented 5-second wall-vs-monotonic-clock
//!   discontinuity allowance. No other slack exists.
//! - `now - observed_at <= max_staleness_secs` (the constraint's
//!   *registered* bound, from the matching `expected[]` entry — never a
//!   default, since the registration schema requires it).
//! - `now < valid_until`, if present (strict).
//! - `now < context_valid_until`, if present (strict).
//!
//! A stale reading is never dropped or hidden from `constraints` —
//! [`is_fresh`] is a predicate the host calls before trusting a reading
//! for anything beyond raw display, not a filter. "Present" and
//! "trusted" are deliberately different questions.

use std::collections::HashMap;

use time::{Duration, OffsetDateTime};

use crate::registration::Registration;
use crate::wire::{
    Constraint, Coverage, MissingConstraint, MissingReason, ObservedContext, SnapshotRequest,
    SnapshotResponse, SnapshotResult, WireValidationError,
};

/// Clock-discontinuity tolerance applied **only** to `observed_at`'s
/// future-timestamp check — `valid_until` and `context_valid_until` are
/// strict (`now < valid_until`, no slack), per the four-part rule above.
/// This is the *only* slack this module grants anywhere — not a general
/// leniency knob.
const CLOCK_TOLERANCE: Duration = Duration::seconds(5);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FreshnessError {
    #[error("invalid RFC3339 timestamp {0:?}: {1}")]
    InvalidTimestamp(String, String),
    #[error("wire validation failed: {0}")]
    Wire(#[from] WireValidationError),
    #[error("requested constraint_ids must not be empty")]
    EmptyRequested,
    #[error("requested id {0:?} has no matching expected[] entry in the registration")]
    NoMatchingExpectation(String),
    #[error("response.protocol_version {actual:?} does not match request {expected:?}")]
    ProtocolVersionMismatch { expected: String, actual: String },
    #[error("response.request_id {actual:?} does not match request {expected:?}")]
    RequestIdMismatch { expected: String, actual: String },
    #[error("response.extension_id {actual:?} does not match registration {expected:?}")]
    ExtensionIdMismatch { expected: String, actual: String },
    #[error("response.extension_version {actual:?} does not match registration {expected:?}")]
    ExtensionVersionMismatch { expected: String, actual: String },
}

fn parse_rfc3339(raw: &str) -> Result<OffsetDateTime, FreshnessError> {
    OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339)
        .map_err(|error| FreshnessError::InvalidTimestamp(raw.to_owned(), error.to_string()))
}

/// Evaluates the four-part freshness rule above for one constraint.
/// `max_staleness_secs` must come from the registration's matching
/// `expected[]` entry — this function takes no default.
pub fn is_fresh(
    constraint: &Constraint,
    max_staleness_secs: u64,
    context_valid_until: Option<&str>,
    now: OffsetDateTime,
) -> Result<bool, FreshnessError> {
    let observed_at = parse_rfc3339(&constraint.observed_at)?;
    if observed_at > now + CLOCK_TOLERANCE {
        return Ok(false);
    }
    if now - observed_at > Duration::seconds(max_staleness_secs as i64) {
        return Ok(false);
    }
    if let Some(valid_until) = &constraint.valid_until {
        if now >= parse_rfc3339(valid_until)? {
            return Ok(false);
        }
    }
    if let Some(context_valid_until) = context_valid_until {
        if now >= parse_rfc3339(context_valid_until)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The min over the requested ids' registered `max_staleness_secs` — the
/// host-computed `max_age_seconds` the design doc requires on every
/// [`SnapshotRequest`]. There is no default: an empty `requested` or a
/// requested id absent from the registration's `expected[]` is an
/// error, never silently zero.
pub fn compute_max_age_seconds(
    registration: &Registration,
    requested: &[String],
) -> Result<u64, FreshnessError> {
    if requested.is_empty() {
        return Err(FreshnessError::EmptyRequested);
    }
    let by_id: HashMap<&str, u64> = registration
        .expected
        .iter()
        .map(|entry| (entry.id.as_str(), entry.max_staleness_secs))
        .collect();
    let mut min = u64::MAX;
    for id in requested {
        let max_staleness = by_id
            .get(id.as_str())
            .copied()
            .ok_or_else(|| FreshnessError::NoMatchingExpectation(id.clone()))?;
        min = min.min(max_staleness);
    }
    Ok(min)
}

/// Correlates a response against the request that produced it and the
/// registration it was sent to. An extension that returns a mismatched
/// `protocol_version`/`request_id`/`extension_id`/`extension_version`
/// must never have its reading trusted, even if every other field
/// parses cleanly.
pub fn verify_response_identifiers(
    request: &SnapshotRequest,
    response: &SnapshotResponse,
    registration: &Registration,
) -> Result<(), FreshnessError> {
    if response.protocol_version != request.protocol_version {
        return Err(FreshnessError::ProtocolVersionMismatch {
            expected: request.protocol_version.clone(),
            actual: response.protocol_version.clone(),
        });
    }
    if response.request_id != request.request_id {
        return Err(FreshnessError::RequestIdMismatch {
            expected: request.request_id.clone(),
            actual: response.request_id.clone(),
        });
    }
    if response.extension_id != registration.extension_id {
        return Err(FreshnessError::ExtensionIdMismatch {
            expected: registration.extension_id.clone(),
            actual: response.extension_id.clone(),
        });
    }
    if response.extension_version != registration.extension_version {
        return Err(FreshnessError::ExtensionVersionMismatch {
            expected: registration.extension_version.clone(),
            actual: response.extension_version.clone(),
        });
    }
    Ok(())
}

/// A host-side snapshot result after account-fingerprint binding has
/// been enforced. Deliberately a distinct type from [`SnapshotResult`]
/// rather than a mutation of it in place: a constraint whose
/// `account_fingerprint` does not match the registration's expectation
/// has been *moved* into `missing` with reason
/// `account_fingerprint_mismatch`, and `coverage` has been recomputed
/// accordingly — a wire-"complete" response with one mismatch is a
/// host-"partial" result, never silently displayed as healthy under
/// the wrong account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundSnapshotResult {
    pub context: ObservedContext,
    pub authority: String,
    pub context_valid_until: Option<String>,
    pub coverage: Coverage,
    pub constraints: Vec<Constraint>,
    pub missing: Vec<MissingConstraint>,
}

/// Applies the account-fingerprint binding check (HORO-1853 design-v2):
/// first validates the raw wire coverage rule and rejects a malformed
/// response outright, then re-homes any constraint whose
/// `account_fingerprint` disagrees with the registration's matching
/// `expected[]` entry into `missing`, then recomputes `coverage` into
/// the host-side [`BoundSnapshotResult`] type.
pub fn apply_account_binding(
    result: &SnapshotResult,
    requested: &[String],
    registration: &Registration,
) -> Result<BoundSnapshotResult, FreshnessError> {
    result.validate_wire_coverage(requested)?;

    let expected_fingerprints: HashMap<&str, &str> = registration
        .expected
        .iter()
        .map(|entry| (entry.id.as_str(), entry.account_fingerprint.as_str()))
        .collect();

    let mut constraints = Vec::with_capacity(result.constraints.len());
    let mut missing = result.missing.clone();
    for constraint in &result.constraints {
        let expected = expected_fingerprints.get(constraint.id.as_str());
        match expected {
            Some(expected) if **expected == constraint.account_fingerprint => {
                constraints.push(constraint.clone());
            }
            _ => missing.push(MissingConstraint {
                id: constraint.id.clone(),
                reason: MissingReason::AccountFingerprintMismatch,
            }),
        }
    }

    let coverage = if missing.is_empty() {
        Coverage::Complete
    } else {
        Coverage::Partial
    };

    Ok(BoundSnapshotResult {
        context: result.context.clone(),
        authority: result.authority.clone(),
        context_valid_until: result.context_valid_until.clone(),
        coverage,
        constraints,
        missing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registration::{
        Approval, Consent, ExpectedConstraint, FixedEnvironment, ObservationConfidence,
        RegisteredContext, Transport, TransportKind, TrustMode,
    };
    use crate::wire::{Metric, Reading, Scope, SnapshotOperation, Unit, UpstreamEnforcement};
    use time::macros::datetime;

    fn sample_constraint(observed_at: &str, fingerprint: &str) -> Constraint {
        Constraint {
            id: "c1".into(),
            scope: Scope::Account {
                handle: "acct".into(),
            },
            metric: Metric::TotalTokens,
            unit: Unit::Tokens,
            window: crate::wire::Window::Opaque {
                declared_reset_at: None,
            },
            reading: Reading::Undisclosed,
            observed_at: observed_at.into(),
            valid_until: None,
            upstream_enforcement: UpstreamEnforcement::Unknown,
            account_fingerprint: fingerprint.into(),
        }
    }

    fn registration_with_expected(id: &str, fingerprint: &str, max_staleness: u64) -> Registration {
        Registration {
            registration_id: "reg-a".into(),
            extension_id: "ext.example".into(),
            extension_version: "1.0.0".into(),
            enabled: true,
            transport: Transport {
                kind: TransportKind::OneshotStdio,
                executable: "/bin/true".into(),
                args: vec![],
                environment: FixedEnvironment {
                    lang: "C.UTF-8".into(),
                    lc_all: "C.UTF-8".into(),
                },
            },
            context: RegisteredContext {
                selector: "sel".into(),
                handle: "handle".into(),
                principal: "principal".into(),
                authority: "authority".into(),
            },
            expected: vec![ExpectedConstraint {
                id: id.into(),
                scope: Scope::Account {
                    handle: "acct".into(),
                },
                metric: Metric::TotalTokens,
                unit: Unit::Tokens,
                window_uuid: "win-1".into(),
                provider: "anthropic".into(),
                account_fingerprint: fingerprint.into(),
                pool_alias: "alias".into(),
                max_staleness_secs: max_staleness,
                window_length_secs: 3600,
                capacity_ceiling: None,
            }],
            binding_revision: "1".into(),
            binding_verified_at: "2026-10-12T00:00:00Z".into(),
            scope_configured_at: "2026-10-12T00:00:00Z".into(),
            timeout_ms: 5_000,
            trust_mode: TrustMode::ObservationOnly,
            observation_confidence: ObservationConfidence::Low,
            approval: Approval {
                approved: true,
                revision: "1".into(),
                executable_sha256: String::new(),
                manifest_sha256: String::new(),
                launch_tuple_sha256: String::new(),
                runtime_files: vec![],
                endpoint_owner: "owner".into(),
                consent: Consent {
                    consented_by: "operator".into(),
                    recorded_at_reference: "ref-1".into(),
                },
            },
        }
    }

    #[test]
    fn exactly_at_max_staleness_is_fresh() {
        let now = datetime!(2026-10-12 00:10:00 UTC);
        let observed_at = datetime!(2026-10-12 00:00:00 UTC); // exactly 600s before now
        let constraint = sample_constraint(
            &observed_at
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap(),
            "fp",
        );
        assert!(is_fresh(&constraint, 600, None, now).unwrap());
    }

    #[test]
    fn one_second_past_max_staleness_is_not_fresh() {
        let now = datetime!(2026-10-12 00:10:01 UTC);
        let observed_at = datetime!(2026-10-12 00:00:00 UTC); // 601s before now
        let constraint = sample_constraint(
            &observed_at
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap(),
            "fp",
        );
        assert!(!is_fresh(&constraint, 600, None, now).unwrap());
    }

    #[test]
    fn future_timestamp_beyond_five_second_tolerance_is_rejected() {
        let now = datetime!(2026-10-12 00:00:00 UTC);
        let observed_at = now + Duration::seconds(6);
        let constraint = sample_constraint(
            &observed_at
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap(),
            "fp",
        );
        assert!(!is_fresh(&constraint, 600, None, now).unwrap());
    }

    #[test]
    fn future_timestamp_within_five_second_tolerance_is_accepted() {
        let now = datetime!(2026-10-12 00:00:00 UTC);
        let observed_at = now + Duration::seconds(5);
        let constraint = sample_constraint(
            &observed_at
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap(),
            "fp",
        );
        assert!(is_fresh(&constraint, 600, None, now).unwrap());
    }

    #[test]
    fn valid_until_and_context_valid_until_are_strict() {
        let now = datetime!(2026-10-12 00:00:00 UTC);
        let observed_at_str = now
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        let mut constraint = sample_constraint(&observed_at_str, "fp");
        constraint.valid_until = Some(
            now.format(&time::format_description::well_known::Rfc3339)
                .unwrap(),
        );
        // now == valid_until -> strict rejection.
        assert!(!is_fresh(&constraint, 600, None, now).unwrap());

        constraint.valid_until = Some(
            (now + Duration::seconds(1))
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap(),
        );
        assert!(is_fresh(&constraint, 600, None, now).unwrap());

        constraint.valid_until = None;
        let context_valid_until = now
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        assert!(!is_fresh(&constraint, 600, Some(&context_valid_until), now).unwrap());
    }

    #[test]
    fn compute_max_age_seconds_is_the_min_over_requested_and_errors_on_empty() {
        let mut registration = registration_with_expected("c1", "fp", 600);
        registration.expected.push(ExpectedConstraint {
            id: "c2".into(),
            scope: Scope::Account {
                handle: "acct".into(),
            },
            metric: Metric::Requests,
            unit: Unit::Requests,
            window_uuid: "win-2".into(),
            provider: "anthropic".into(),
            account_fingerprint: "fp".into(),
            pool_alias: "alias2".into(),
            max_staleness_secs: 60,
            window_length_secs: 3600,
            capacity_ceiling: None,
        });
        let requested = vec!["c1".to_string(), "c2".to_string()];
        assert_eq!(
            compute_max_age_seconds(&registration, &requested).unwrap(),
            60
        );
        assert_eq!(
            compute_max_age_seconds(&registration, &[]).unwrap_err(),
            FreshnessError::EmptyRequested
        );
        let unknown = vec!["c3".to_string()];
        assert_eq!(
            compute_max_age_seconds(&registration, &unknown).unwrap_err(),
            FreshnessError::NoMatchingExpectation("c3".into())
        );
    }

    #[test]
    fn verify_response_identifiers_rejects_each_mismatch() {
        let registration = registration_with_expected("c1", "fp", 600);
        let request = SnapshotRequest {
            protocol_version: "libra.quota-observation/0.1".into(),
            request_id: "r1".into(),
            operation: SnapshotOperation::Snapshot,
            context_selector: "sel".into(),
            constraint_ids: vec!["c1".into()],
            max_age_seconds: 600,
            budget_ms: 1000,
        };
        let good = SnapshotResponse {
            protocol_version: request.protocol_version.clone(),
            request_id: request.request_id.clone(),
            extension_id: registration.extension_id.clone(),
            extension_version: registration.extension_version.clone(),
            result: None,
            error: None,
        };
        assert!(verify_response_identifiers(&request, &good, &registration).is_ok());

        let mut bad = good.clone();
        bad.protocol_version = "other".into();
        assert!(matches!(
            verify_response_identifiers(&request, &bad, &registration).unwrap_err(),
            FreshnessError::ProtocolVersionMismatch { .. }
        ));

        let mut bad = good.clone();
        bad.request_id = "other".into();
        assert!(matches!(
            verify_response_identifiers(&request, &bad, &registration).unwrap_err(),
            FreshnessError::RequestIdMismatch { .. }
        ));

        let mut bad = good.clone();
        bad.extension_id = "other".into();
        assert!(matches!(
            verify_response_identifiers(&request, &bad, &registration).unwrap_err(),
            FreshnessError::ExtensionIdMismatch { .. }
        ));

        let mut bad = good.clone();
        bad.extension_version = "other".into();
        assert!(matches!(
            verify_response_identifiers(&request, &bad, &registration).unwrap_err(),
            FreshnessError::ExtensionVersionMismatch { .. }
        ));
    }

    #[test]
    fn account_fingerprint_mismatch_moves_constraint_to_missing_and_downgrades_coverage() {
        let registration = registration_with_expected("c1", "fp-expected", 600);
        let requested = vec!["c1".to_string()];
        let result = SnapshotResult {
            context: ObservedContext {
                selector: "s".into(),
                handle: "h".into(),
                principal: "p".into(),
                epoch: "e".into(),
            },
            authority: "a".into(),
            context_valid_until: None,
            coverage: Coverage::Complete,
            constraints: vec![sample_constraint("2026-10-12T00:00:00Z", "fp-wrong")],
            missing: vec![],
        };
        let bound = apply_account_binding(&result, &requested, &registration).unwrap();
        assert_eq!(bound.coverage, Coverage::Partial);
        assert!(bound.constraints.is_empty());
        assert_eq!(bound.missing.len(), 1);
        assert_eq!(
            bound.missing[0].reason,
            MissingReason::AccountFingerprintMismatch
        );
    }

    #[test]
    fn matching_account_fingerprint_keeps_constraint_and_complete_coverage() {
        let registration = registration_with_expected("c1", "fp-expected", 600);
        let requested = vec!["c1".to_string()];
        let result = SnapshotResult {
            context: ObservedContext {
                selector: "s".into(),
                handle: "h".into(),
                principal: "p".into(),
                epoch: "e".into(),
            },
            authority: "a".into(),
            context_valid_until: None,
            coverage: Coverage::Complete,
            constraints: vec![sample_constraint("2026-10-12T00:00:00Z", "fp-expected")],
            missing: vec![],
        };
        let bound = apply_account_binding(&result, &requested, &registration).unwrap();
        assert_eq!(bound.coverage, Coverage::Complete);
        assert_eq!(bound.constraints.len(), 1);
        assert!(bound.missing.is_empty());
    }

    #[test]
    fn malformed_wire_coverage_is_rejected_before_account_binding_is_applied() {
        let registration = registration_with_expected("c1", "fp-expected", 600);
        let requested = vec!["c1".to_string()];
        let result = SnapshotResult {
            context: ObservedContext {
                selector: "s".into(),
                handle: "h".into(),
                principal: "p".into(),
                epoch: "e".into(),
            },
            authority: "a".into(),
            context_valid_until: None,
            coverage: Coverage::Complete,
            constraints: vec![],
            missing: vec![],
        };
        assert!(matches!(
            apply_account_binding(&result, &requested, &registration).unwrap_err(),
            FreshnessError::Wire(_)
        ));
    }
}
