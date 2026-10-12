//! Freshness evaluation and account-binding enforcement (HORO-1853
//! design-v2).
//!
//! A constraint's reading is "fresh" only if **all** of:
//! - `observed_at <= received_at + 5s` — strict future-timestamp
//!   rejection, with zero tolerance except a documented 5-second
//!   wall-vs-monotonic-clock discontinuity allowance at the moment the
//!   host actually received the frame (`received_at`). No other slack
//!   exists, and this tolerance is never allowed to also extend the
//!   staleness bound below — see [`is_fresh`]'s own docs for why the
//!   staleness anchor is `min(observed_at, received_at)`, not
//!   `observed_at` alone.
//! - `now - min(observed_at, received_at) <= max_staleness_secs` (the
//!   constraint's *registered* bound, from the matching `expected[]`
//!   entry — never a default, since the registration schema requires
//!   it).
//! - `now < valid_until`, if present (strict).
//! - `now < context_valid_until`, if present (strict).
//!
//! A stale reading is never dropped or hidden from `constraints` —
//! [`is_fresh`] is a predicate the host calls before trusting a reading
//! for anything beyond raw display, not a filter. "Present" and
//! "trusted" are deliberately different questions.

use std::collections::HashMap;

use time::{Duration, OffsetDateTime};

use crate::registration::{ExpectedConstraint, Registration};
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
    /// A whole-response rejection (HORO-1854 adversarial review, B3): the
    /// response's declared `context`/`authority` fields disagree with
    /// what this registration declared it was registered against. Unlike
    /// [`MissingConstraint`], which demotes one constraint, this rejects
    /// the entire [`SnapshotResult`] — a context mismatch means the
    /// extension answered about the *wrong account/selector entirely*,
    /// which no amount of per-constraint filtering can make safe to
    /// trust any part of.
    #[error(
        "response context field `{field}` mismatch: registration expects {expected:?}, got {actual:?}"
    )]
    ContextMismatch {
        field: &'static str,
        expected: String,
        actual: String,
    },
    /// `max_staleness_secs` (a `u64`, per the registration schema) does
    /// not fit in an `i64` and so cannot be used to build a [`Duration`].
    /// Rejecting outright (rather than casting with `as i64`, which
    /// silently wraps `u64::MAX` to `-1`) is the fail-closed choice: a
    /// wrapped `-1` would make every past reading look stale (safe) but
    /// also make any reading up to ~5s in the *future* look fresh
    /// (unsafe) — see HORO-1854's adversarial review.
    #[error("max_staleness_secs {0} does not fit in an i64 and is rejected rather than wrapped")]
    MaxStalenessOutOfRange(u64),
}

fn parse_rfc3339(raw: &str) -> Result<OffsetDateTime, FreshnessError> {
    OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339)
        .map_err(|error| FreshnessError::InvalidTimestamp(raw.to_owned(), error.to_string()))
}

/// Evaluates the four-part freshness rule above for one constraint.
/// `max_staleness_secs` must come from the registration's matching
/// `expected[]` entry — this function takes no default.
///
/// `received_at` is when the host actually received and parsed this
/// response frame; `now` is when the caller is evaluating freshness
/// (which may be later, e.g. a cached result displayed after some
/// delay). They are deliberately separate parameters — see "Staleness
/// is anchored at receipt, not just claimed `observed_at`" below.
///
/// # Staleness is anchored at receipt, not just claimed `observed_at`
///
/// (HORO-1854 adversarial review, should-fix "freshness staleness-bound
/// escape".) The 5-second [`CLOCK_TOLERANCE`] exists only to forgive a
/// small wall-vs-monotonic clock discontinuity between the extension's
/// clock and the host's, at the moment the host actually received the
/// frame — it is not supposed to let a reading be treated as fresher,
/// for staleness-math purposes, than when the host actually received
/// it. Before this fix, staleness was computed as `now - observed_at`
/// using the extension-claimed `observed_at` directly: an extension
/// that claimed `observed_at = (true observation time) + 5s` (within
/// tolerance) got 5 extra seconds added to its effective staleness
/// budget on top of `max_staleness_secs`, confirmed by probe (observe
/// at `T`, claim `T+5`, evaluate at `T+605` with `max_staleness_secs =
/// 600` returned fresh = `true`, even though the real reading was 605s
/// old). The fix: clamp the anchor used for the staleness comparison to
/// `min(observed_at, received_at)` — a claimed-future `observed_at` is
/// still checked against `received_at + CLOCK_TOLERANCE` for the
/// future-timestamp rule, but cannot push the staleness clock itself
/// forward past receipt.
pub fn is_fresh(
    constraint: &Constraint,
    max_staleness_secs: u64,
    context_valid_until: Option<&str>,
    received_at: OffsetDateTime,
    now: OffsetDateTime,
) -> Result<bool, FreshnessError> {
    let observed_at = parse_rfc3339(&constraint.observed_at)?;
    if observed_at > received_at + CLOCK_TOLERANCE {
        return Ok(false);
    }
    let staleness_anchor = observed_at.min(received_at);
    let max_staleness = i64::try_from(max_staleness_secs)
        .map_err(|_| FreshnessError::MaxStalenessOutOfRange(max_staleness_secs))?;
    if now - staleness_anchor > Duration::seconds(max_staleness) {
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

/// Whole-response rejection (HORO-1854 adversarial review, B3): the
/// response's declared `context`/`authority` must match the
/// registration's declared `context.selector`/`handle`/`principal`/
/// `authority` exactly. `ObservedContext::epoch` has no registered
/// counterpart to compare against and is intentionally not checked
/// here. Before this check existed, `result.context`/`result.authority`
/// were copied straight into [`BoundSnapshotResult`] with no
/// verification against what was requested at all — an extension could
/// answer about an entirely different account/selector and every
/// constraint in a `coverage: complete` response would still look
/// trustworthy.
fn verify_context_binding(
    result: &SnapshotResult,
    registration: &Registration,
) -> Result<(), FreshnessError> {
    if result.context.selector != registration.context.selector {
        return Err(FreshnessError::ContextMismatch {
            field: "selector",
            expected: registration.context.selector.clone(),
            actual: result.context.selector.clone(),
        });
    }
    if result.context.handle != registration.context.handle {
        return Err(FreshnessError::ContextMismatch {
            field: "handle",
            expected: registration.context.handle.clone(),
            actual: result.context.handle.clone(),
        });
    }
    if result.context.principal != registration.context.principal {
        return Err(FreshnessError::ContextMismatch {
            field: "principal",
            expected: registration.context.principal.clone(),
            actual: result.context.principal.clone(),
        });
    }
    if result.authority != registration.context.authority {
        return Err(FreshnessError::ContextMismatch {
            field: "authority",
            expected: registration.context.authority.clone(),
            actual: result.authority.clone(),
        });
    }
    Ok(())
}

/// Applies the account-binding checks (HORO-1853 design-v2, hardened by
/// the HORO-1854 adversarial review, B3): first validates the raw wire
/// coverage rule and rejects a malformed response outright, then
/// rejects the *entire* response if its declared context/authority
/// don't match the registration ([`verify_context_binding`]), then
/// re-homes any constraint whose `unit`/`metric`/`scope`/
/// `account_fingerprint` disagrees with the registration's matching
/// `expected[]` entry into `missing`, then recomputes `coverage` into
/// the host-side [`BoundSnapshotResult`] type.
///
/// Before this fix, only `account_fingerprint` was checked — `unit`,
/// `metric`, and `scope` were never compared at all, so a constraint
/// declared `TotalTokens`/`Tokens`/`Account` in `expected[]` could be
/// satisfied by a response reporting `Spend`/`UsdCents`/`Principal{...}`
/// for a *different* account, with `coverage: complete`: a USD reading
/// would land silently in a tokens pool.
pub fn apply_account_binding(
    result: &SnapshotResult,
    requested: &[String],
    registration: &Registration,
) -> Result<BoundSnapshotResult, FreshnessError> {
    result.validate_wire_coverage(requested)?;
    verify_context_binding(result, registration)?;

    let expected_by_id: HashMap<&str, &ExpectedConstraint> = registration
        .expected
        .iter()
        .map(|entry| (entry.id.as_str(), entry))
        .collect();

    let mut constraints = Vec::with_capacity(result.constraints.len());
    let mut missing = result.missing.clone();
    for constraint in &result.constraints {
        match expected_by_id.get(constraint.id.as_str()) {
            Some(expected) if expected.unit != constraint.unit => {
                missing.push(MissingConstraint {
                    id: constraint.id.clone(),
                    reason: MissingReason::UnsupportedUnit,
                });
            }
            Some(expected) if expected.metric != constraint.metric => {
                missing.push(MissingConstraint {
                    id: constraint.id.clone(),
                    reason: MissingReason::UnsupportedMetric,
                });
            }
            // Scope mismatch (wrong scope kind, e.g. `Account` vs
            // `Principal`, or the same kind with a wrong `handle`, e.g.
            // the right account type but someone else's account): there
            // is no dedicated `missing.reason` variant for "returned
            // scope disagrees with the registered scope" in this
            // protocol's reason enum. A scope mismatch is the same class
            // of failure as an account-fingerprint mismatch — the
            // reading is bound to the wrong entity, not malformed or
            // unavailable — so it reuses `AccountFingerprintMismatch`
            // rather than inventing a new variant this slice has no
            // mandate to add to the wire schema.
            Some(expected) if expected.scope != constraint.scope => {
                missing.push(MissingConstraint {
                    id: constraint.id.clone(),
                    reason: MissingReason::AccountFingerprintMismatch,
                });
            }
            Some(expected) if expected.account_fingerprint != constraint.account_fingerprint => {
                missing.push(MissingConstraint {
                    id: constraint.id.clone(),
                    reason: MissingReason::AccountFingerprintMismatch,
                });
            }
            Some(_) => {
                constraints.push(constraint.clone());
            }
            None => {
                // No matching `expected[]` entry at all (should not
                // happen once `validate_wire_coverage` has already
                // confirmed every id was one of `requested`, and
                // `Registration::validate` rejects duplicate
                // `expected[].id`s — but fail closed rather than panic
                // or silently trust an unbound id).
                missing.push(MissingConstraint {
                    id: constraint.id.clone(),
                    reason: MissingReason::AccountFingerprintMismatch,
                });
            }
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
        assert!(is_fresh(&constraint, 600, None, now, now).unwrap());
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
        assert!(!is_fresh(&constraint, 600, None, now, now).unwrap());
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
        assert!(!is_fresh(&constraint, 600, None, now, now).unwrap());
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
        assert!(is_fresh(&constraint, 600, None, now, now).unwrap());
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
        assert!(!is_fresh(&constraint, 600, None, now, now).unwrap());

        constraint.valid_until = Some(
            (now + Duration::seconds(1))
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap(),
        );
        assert!(is_fresh(&constraint, 600, None, now, now).unwrap());

        constraint.valid_until = None;
        let context_valid_until = now
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        assert!(!is_fresh(&constraint, 600, Some(&context_valid_until), now, now).unwrap());
    }

    // Regression test for the "freshness staleness-bound escape"
    // should-fix (HORO-1854 adversarial review): this is the exact
    // probe from the review — observe at `t`, claim `observed_at =
    // t+5s` (within the 5s clock tolerance), evaluate at `t+605s` with
    // `max_staleness_secs = 600`. Before the `received_at` anchor fix,
    // this returned `true` (the 5s of claimed future skew silently
    // added 5 extra seconds of staleness budget); it must now return
    // `false`.
    #[test]
    fn staleness_cannot_be_extended_by_a_claimed_future_observed_at() {
        let t = datetime!(2026-10-12 00:00:00 UTC);
        let received_at = t;
        let observed_at = t + Duration::seconds(5);
        let now = t + Duration::seconds(605);
        let constraint = sample_constraint(
            &observed_at
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap(),
            "fp",
        );
        assert!(!is_fresh(&constraint, 600, None, received_at, now).unwrap());
    }

    // Regression test for the `as i64` cast wrap (HORO-1854 adversarial
    // review): `max_staleness_secs: u64::MAX` previously wrapped to -1
    // via `as i64`, making every past reading look stale and a
    // near-future reading look fresh. It must now be a hard error
    // instead, for both a clearly-past and a within-tolerance-future
    // reading.
    #[test]
    fn max_staleness_u64_max_is_rejected_rather_than_wrapped() {
        let now = datetime!(2026-10-12 00:00:00 UTC);
        let past_observed_at = now - Duration::seconds(10);
        let constraint = sample_constraint(
            &past_observed_at
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap(),
            "fp",
        );
        assert_eq!(
            is_fresh(&constraint, u64::MAX, None, now, now).unwrap_err(),
            FreshnessError::MaxStalenessOutOfRange(u64::MAX)
        );

        let future_observed_at = now + Duration::seconds(3);
        let constraint = sample_constraint(
            &future_observed_at
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap(),
            "fp",
        );
        assert_eq!(
            is_fresh(&constraint, u64::MAX, None, now, now).unwrap_err(),
            FreshnessError::MaxStalenessOutOfRange(u64::MAX)
        );
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

    /// Context/authority that exactly matches `registration_with_expected`'s
    /// `RegisteredContext` (`sel`/`handle`/`principal`/`authority`), so a
    /// test can focus on the one field it's exercising without tripping
    /// the (now-enforced) whole-response context check incidentally.
    /// `epoch` has no registered counterpart, so any value is fine there.
    fn matching_observed_context() -> (ObservedContext, String) {
        (
            ObservedContext {
                selector: "sel".into(),
                handle: "handle".into(),
                principal: "principal".into(),
                epoch: "e".into(),
            },
            "authority".into(),
        )
    }

    #[test]
    fn account_fingerprint_mismatch_moves_constraint_to_missing_and_downgrades_coverage() {
        let registration = registration_with_expected("c1", "fp-expected", 600);
        let requested = vec!["c1".to_string()];
        let (context, authority) = matching_observed_context();
        let result = SnapshotResult {
            context,
            authority,
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
        let (context, authority) = matching_observed_context();
        let result = SnapshotResult {
            context,
            authority,
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
        let (context, authority) = matching_observed_context();
        let result = SnapshotResult {
            context,
            authority,
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

    // Regression tests for B3 (HORO-1854 adversarial review): `unit`,
    // `metric`, and `scope` were never compared at all before this fix —
    // only `account_fingerprint` was. Each of these constructs a
    // constraint that matches on fingerprint but disagrees on exactly
    // one of the other three binding fields, and must be demoted to
    // `missing` with the documented reason rather than accepted as
    // `Complete`.
    #[test]
    fn unit_mismatch_is_rejected_into_missing_with_unsupported_unit() {
        let registration = registration_with_expected("c1", "fp", 600);
        let requested = vec!["c1".to_string()];
        let (context, authority) = matching_observed_context();
        let mut constraint = sample_constraint("2026-10-12T00:00:00Z", "fp");
        constraint.unit = crate::wire::Unit::UsdCents;
        constraint.metric = crate::wire::Metric::Spend;
        constraint.reading = crate::wire::Reading::Utilization {
            used_basis_points: crate::wire::Quantity::parse("1").unwrap(),
        };
        let result = SnapshotResult {
            context,
            authority,
            context_valid_until: None,
            coverage: Coverage::Complete,
            constraints: vec![constraint],
            missing: vec![],
        };
        let bound = apply_account_binding(&result, &requested, &registration).unwrap();
        assert_eq!(bound.coverage, Coverage::Partial);
        assert!(bound.constraints.is_empty());
        assert_eq!(bound.missing[0].reason, MissingReason::UnsupportedUnit);
    }

    #[test]
    fn metric_mismatch_is_rejected_into_missing_with_unsupported_metric() {
        let registration = registration_with_expected("c1", "fp", 600);
        let requested = vec!["c1".to_string()];
        let (context, authority) = matching_observed_context();
        let mut constraint = sample_constraint("2026-10-12T00:00:00Z", "fp");
        constraint.metric = crate::wire::Metric::Requests;
        let result = SnapshotResult {
            context,
            authority,
            context_valid_until: None,
            coverage: Coverage::Complete,
            constraints: vec![constraint],
            missing: vec![],
        };
        let bound = apply_account_binding(&result, &requested, &registration).unwrap();
        assert_eq!(bound.coverage, Coverage::Partial);
        assert!(bound.constraints.is_empty());
        assert_eq!(bound.missing[0].reason, MissingReason::UnsupportedMetric);
    }

    #[test]
    fn scope_kind_mismatch_is_rejected_into_missing() {
        let registration = registration_with_expected("c1", "fp", 600);
        let requested = vec!["c1".to_string()];
        let (context, authority) = matching_observed_context();
        let mut constraint = sample_constraint("2026-10-12T00:00:00Z", "fp");
        constraint.scope = crate::wire::Scope::Principal {
            handle: "acct".into(),
        };
        let result = SnapshotResult {
            context,
            authority,
            context_valid_until: None,
            coverage: Coverage::Complete,
            constraints: vec![constraint],
            missing: vec![],
        };
        let bound = apply_account_binding(&result, &requested, &registration).unwrap();
        assert_eq!(bound.coverage, Coverage::Partial);
        assert!(bound.constraints.is_empty());
        assert_eq!(
            bound.missing[0].reason,
            MissingReason::AccountFingerprintMismatch
        );
    }

    #[test]
    fn scope_handle_mismatch_is_rejected_into_missing() {
        let registration = registration_with_expected("c1", "fp", 600);
        let requested = vec!["c1".to_string()];
        let (context, authority) = matching_observed_context();
        let mut constraint = sample_constraint("2026-10-12T00:00:00Z", "fp");
        constraint.scope = crate::wire::Scope::Account {
            handle: "someone-elses-acct".into(),
        };
        let result = SnapshotResult {
            context,
            authority,
            context_valid_until: None,
            coverage: Coverage::Complete,
            constraints: vec![constraint],
            missing: vec![],
        };
        let bound = apply_account_binding(&result, &requested, &registration).unwrap();
        assert_eq!(bound.coverage, Coverage::Partial);
        assert!(bound.constraints.is_empty());
        assert_eq!(
            bound.missing[0].reason,
            MissingReason::AccountFingerprintMismatch
        );
    }

    #[test]
    fn principal_handle_mismatch_on_a_principal_scoped_constraint_is_rejected() {
        let mut registration = registration_with_expected("c1", "fp", 600);
        registration.expected[0].scope = crate::wire::Scope::Principal {
            handle: "right-principal".into(),
        };
        let requested = vec!["c1".to_string()];
        let (context, authority) = matching_observed_context();
        let mut constraint = sample_constraint("2026-10-12T00:00:00Z", "fp");
        constraint.scope = crate::wire::Scope::Principal {
            handle: "wrong-principal".into(),
        };
        let result = SnapshotResult {
            context,
            authority,
            context_valid_until: None,
            coverage: Coverage::Complete,
            constraints: vec![constraint],
            missing: vec![],
        };
        let bound = apply_account_binding(&result, &requested, &registration).unwrap();
        assert_eq!(bound.coverage, Coverage::Partial);
        assert!(bound.constraints.is_empty());
        assert_eq!(
            bound.missing[0].reason,
            MissingReason::AccountFingerprintMismatch
        );
    }

    // Regression tests for B3's whole-result context rejection: a
    // mismatch on any of the four registered context/authority fields
    // must reject the entire response, not just demote a constraint.
    #[test]
    fn context_selector_mismatch_rejects_the_whole_response() {
        let registration = registration_with_expected("c1", "fp", 600);
        let requested = vec!["c1".to_string()];
        let (mut context, authority) = matching_observed_context();
        context.selector = "wrong-selector".into();
        let result = SnapshotResult {
            context,
            authority,
            context_valid_until: None,
            coverage: Coverage::Complete,
            constraints: vec![sample_constraint("2026-10-12T00:00:00Z", "fp")],
            missing: vec![],
        };
        assert!(matches!(
            apply_account_binding(&result, &requested, &registration).unwrap_err(),
            FreshnessError::ContextMismatch {
                field: "selector",
                ..
            }
        ));
    }

    #[test]
    fn context_handle_mismatch_rejects_the_whole_response() {
        let registration = registration_with_expected("c1", "fp", 600);
        let requested = vec!["c1".to_string()];
        let (mut context, authority) = matching_observed_context();
        context.handle = "wrong-handle".into();
        let result = SnapshotResult {
            context,
            authority,
            context_valid_until: None,
            coverage: Coverage::Complete,
            constraints: vec![sample_constraint("2026-10-12T00:00:00Z", "fp")],
            missing: vec![],
        };
        assert!(matches!(
            apply_account_binding(&result, &requested, &registration).unwrap_err(),
            FreshnessError::ContextMismatch {
                field: "handle",
                ..
            }
        ));
    }

    #[test]
    fn context_principal_mismatch_rejects_the_whole_response() {
        let registration = registration_with_expected("c1", "fp", 600);
        let requested = vec!["c1".to_string()];
        let (mut context, authority) = matching_observed_context();
        context.principal = "wrong-principal".into();
        let result = SnapshotResult {
            context,
            authority,
            context_valid_until: None,
            coverage: Coverage::Complete,
            constraints: vec![sample_constraint("2026-10-12T00:00:00Z", "fp")],
            missing: vec![],
        };
        assert!(matches!(
            apply_account_binding(&result, &requested, &registration).unwrap_err(),
            FreshnessError::ContextMismatch {
                field: "principal",
                ..
            }
        ));
    }

    #[test]
    fn authority_mismatch_rejects_the_whole_response() {
        let registration = registration_with_expected("c1", "fp", 600);
        let requested = vec!["c1".to_string()];
        let (context, _authority) = matching_observed_context();
        let result = SnapshotResult {
            context,
            authority: "wrong-authority".into(),
            context_valid_until: None,
            coverage: Coverage::Complete,
            constraints: vec![sample_constraint("2026-10-12T00:00:00Z", "fp")],
            missing: vec![],
        };
        assert!(matches!(
            apply_account_binding(&result, &requested, &registration).unwrap_err(),
            FreshnessError::ContextMismatch {
                field: "authority",
                ..
            }
        ));
    }
}
