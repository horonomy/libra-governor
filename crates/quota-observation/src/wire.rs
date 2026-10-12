//! Wire types for the accepted design-v2 quota-observation protocol
//! (HORO-1853/HORO-1854): the bootstrap `describe` envelope and the
//! `snapshot` request/response. Every struct is `deny_unknown_fields` so
//! a malformed or protocol-drifted extension response fails loudly
//! rather than silently dropping a field the host needed.
//!
//! This module performs only *wire-shape* validation (deserialize
//! succeeds, [`Quantity`] parses, the raw coverage rule holds). It does
//! **not** apply trust/freshness/account-binding semantics — that is
//! [`crate::freshness`]'s job, operating on the validated types this
//! module produces.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A structural defect in a parsed wire frame: the JSON deserialized
/// (field names/types matched) but a cross-field or semantic invariant
/// this protocol requires did not hold.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WireValidationError {
    #[error("response declares both a result and an error")]
    BothResultAndError,
    #[error("response declares neither a result nor an error")]
    NeitherResultNorError,
    #[error("retry_after_seconds must be present only when retry is after_delay")]
    RetryAfterSecondsMismatch,
    #[error("budget_ms must be <= 10000")]
    BudgetTooLarge,
    #[error("constraint_ids must not be empty")]
    EmptyConstraintIds,
    #[error("coverage=complete requires missing to be empty and constraints non-empty")]
    CompleteCoverageViolated,
    #[error("coverage=partial requires missing to be non-empty")]
    PartialCoverageViolated,
    #[error("requested constraint id {0:?} appears in both constraints and missing")]
    DuplicateConstraintId(String),
    #[error("requested constraint id {0:?} appears in neither constraints nor missing")]
    MissingConstraintId(String),
    #[error("response contains an id {0:?} that was not requested")]
    UnrequestedConstraintId(String),
}

/// A canonical, unsigned decimal quantity string: `0` or `[1-9][0-9]*`,
/// no leading zero, no sign, no exponent, value `<= i64::MAX`. The
/// canonical string is retained verbatim (not reformatted) so a
/// round-tripped frame is byte-identical.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quantity {
    canonical: String,
    value: i64,
}

impl Serialize for Quantity {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.canonical)
    }
}

impl Quantity {
    /// Parses and validates a canonical decimal string per this type's
    /// contract. Rejects anything that is not exactly `0` or
    /// `[1-9][0-9]*`, and anything exceeding `i64::MAX` — never clamps
    /// or rounds.
    pub fn parse(raw: &str) -> Result<Self, QuantityError> {
        if raw.is_empty() {
            return Err(QuantityError::Empty);
        }
        if !raw.bytes().all(|b| b.is_ascii_digit()) {
            return Err(QuantityError::NotDecimal);
        }
        if raw.len() > 1 && raw.as_bytes()[0] == b'0' {
            return Err(QuantityError::LeadingZero);
        }
        let value: i64 = raw.parse().map_err(|_| QuantityError::TooLarge)?;
        Ok(Quantity {
            canonical: raw.to_owned(),
            value,
        })
    }

    /// The parsed value. Never negative (the canonical form forbids a
    /// sign), never exceeds `i64::MAX`.
    pub fn value(&self) -> i64 {
        self.value
    }

    /// The exact canonical string this quantity was parsed from.
    pub fn as_str(&self) -> &str {
        &self.canonical
    }
}

impl fmt::Display for Quantity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.canonical)
    }
}

impl TryFrom<String> for Quantity {
    type Error = QuantityError;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        Quantity::parse(&raw)
    }
}

impl<'de> Deserialize<'de> for Quantity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Quantity::parse(&raw).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QuantityError {
    #[error("quantity string is empty")]
    Empty,
    #[error("quantity is not an unsigned decimal string")]
    NotDecimal,
    #[error("quantity has a leading zero")]
    LeadingZero,
    #[error("quantity exceeds i64::MAX")]
    TooLarge,
}

/// Retry guidance attached to a [`WireError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryKind {
    Never,
    AfterUserAction,
    AfterDelay,
}

/// Error codes shared by the bootstrap and snapshot error cases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    AuthRequired,
    PermissionDenied,
    Unavailable,
    Timeout,
    IncompatibleProtocol,
    UnsupportedCapability,
    InvalidConfiguration,
    RateLimited,
    ContextChanged,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireError {
    pub code: ErrorCode,
    pub retry: RetryKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
}

impl WireError {
    pub fn validate(&self) -> Result<(), WireValidationError> {
        match (self.retry, self.retry_after_seconds.is_some()) {
            (RetryKind::AfterDelay, true) | (RetryKind::Never, false) => Ok(()),
            (RetryKind::AfterUserAction, false) => Ok(()),
            _ => Err(WireValidationError::RetryAfterSecondsMismatch),
        }
    }
}

// ---------------------------------------------------------------------
// Bootstrap (describe) envelope
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootstrapOperation {
    Describe,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapRequest {
    pub bootstrap_version: String,
    pub request_id: String,
    pub operation: BootstrapOperation,
    pub supported_versions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapResult {
    pub extension_id: String,
    pub extension_version: String,
    pub supported_versions: Vec<String>,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapResponse {
    pub bootstrap_version: String,
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<BootstrapResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<WireError>,
}

impl BootstrapResponse {
    pub fn validate(&self) -> Result<(), WireValidationError> {
        match (&self.result, &self.error) {
            (Some(_), Some(_)) => Err(WireValidationError::BothResultAndError),
            (None, None) => Err(WireValidationError::NeitherResultNorError),
            (None, Some(error)) => error.validate(),
            (Some(_), None) => Ok(()),
        }
    }
}

// ---------------------------------------------------------------------
// Snapshot envelope
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotOperation {
    Snapshot,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotRequest {
    pub protocol_version: String,
    pub request_id: String,
    pub operation: SnapshotOperation,
    pub context_selector: String,
    pub constraint_ids: Vec<String>,
    pub max_age_seconds: u64,
    pub budget_ms: u64,
}

impl SnapshotRequest {
    pub fn validate(&self) -> Result<(), WireValidationError> {
        if self.budget_ms > 10_000 {
            return Err(WireValidationError::BudgetTooLarge);
        }
        if self.constraint_ids.is_empty() {
            return Err(WireValidationError::EmptyConstraintIds);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Scope {
    Principal { handle: String },
    Account { handle: String },
    Team { handle: String },
    Organization { handle: String },
    Project { handle: String },
    SharedPool { handle: String },
    Provider { handle: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    TotalTokens,
    InputTokens,
    OutputTokens,
    Requests,
    Spend,
    CreditUsage,
    Utilization,
}

// `Unit`, `Window`, and `Reading` deserialize manually rather than via
// `#[derive(Deserialize)]` with `#[serde(tag = "kind", deny_unknown_fields)]`.
// serde's internally-tagged representation does not apply
// `deny_unknown_fields` correctly to a variant with no fields at all
// (a true C-like unit variant, e.g. `Tokens` or `Undisclosed`) — it
// matches on `kind` alone and silently ignores any other keys in the
// object, regardless of the `deny_unknown_fields` attribute
// (serde-rs/serde#1358). `Scope`'s variants all carry a required
// `handle` field, so the derive's struct-like path is exercised there
// and works correctly; these three enums have at least one fieldless
// variant, so each gets a small manual `Deserialize` that checks the
// exact allowed key set itself.

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Unit {
    Tokens,
    Requests,
    UsdCents,
    PercentBasisPoints,
    OpaqueCredit { namespace: String },
}

impl<'de> Deserialize<'de> for Unit {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let value = serde_json::Value::deserialize(deserializer)?;
        let object = value
            .as_object()
            .ok_or_else(|| Error::custom("expected a JSON object"))?;
        let kind = object
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| Error::custom("missing field `kind`"))?;
        let allowed: &[&str] = if kind == "opaque_credit" {
            &["kind", "namespace"]
        } else {
            &["kind"]
        };
        if let Some(extra) = object.keys().find(|k| !allowed.contains(&k.as_str())) {
            return Err(Error::custom(format!("unknown field `{extra}`")));
        }
        match kind {
            "tokens" => Ok(Unit::Tokens),
            "requests" => Ok(Unit::Requests),
            "usd_cents" => Ok(Unit::UsdCents),
            "percent_basis_points" => Ok(Unit::PercentBasisPoints),
            "opaque_credit" => {
                let namespace = object
                    .get("namespace")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| Error::custom("missing field `namespace`"))?
                    .to_owned();
                Ok(Unit::OpaqueCredit { namespace })
            }
            other => Err(Error::custom(format!("unknown variant `{other}`"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Window {
    Opaque {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        declared_reset_at: Option<String>,
    },
}

impl<'de> Deserialize<'de> for Window {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let value = serde_json::Value::deserialize(deserializer)?;
        let object = value
            .as_object()
            .ok_or_else(|| Error::custom("expected a JSON object"))?;
        let kind = object
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| Error::custom("missing field `kind`"))?;
        let allowed: &[&str] = &["kind", "declared_reset_at"];
        if let Some(extra) = object.keys().find(|k| !allowed.contains(&k.as_str())) {
            return Err(Error::custom(format!("unknown field `{extra}`")));
        }
        match kind {
            "opaque" => {
                let declared_reset_at = match object.get("declared_reset_at") {
                    None | Some(serde_json::Value::Null) => None,
                    Some(serde_json::Value::String(s)) => Some(s.clone()),
                    Some(_) => return Err(Error::custom("declared_reset_at must be a string")),
                };
                Ok(Window::Opaque { declared_reset_at })
            }
            other => Err(Error::custom(format!("unknown variant `{other}`"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reading {
    Used { value: Quantity, limit: Quantity },
    Remaining { value: Quantity, limit: Quantity },
    Utilization { used_basis_points: Quantity },
    Undisclosed,
}

impl<'de> Deserialize<'de> for Reading {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let value = serde_json::Value::deserialize(deserializer)?;
        let object = value
            .as_object()
            .ok_or_else(|| Error::custom("expected a JSON object"))?;
        let kind = object
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| Error::custom("missing field `kind`"))?;
        let allowed: &[&str] = match kind {
            "used" | "remaining" => &["kind", "value", "limit"],
            "utilization" => &["kind", "used_basis_points"],
            _ => &["kind"],
        };
        if let Some(extra) = object.keys().find(|k| !allowed.contains(&k.as_str())) {
            return Err(Error::custom(format!("unknown field `{extra}`")));
        }
        let quantity = |key: &str| -> Result<Quantity, D::Error> {
            let raw = object
                .get(key)
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| Error::custom(format!("missing field `{key}`")))?;
            Quantity::parse(raw).map_err(|e| Error::custom(e.to_string()))
        };
        match kind {
            "used" => Ok(Reading::Used {
                value: quantity("value")?,
                limit: quantity("limit")?,
            }),
            "remaining" => Ok(Reading::Remaining {
                value: quantity("value")?,
                limit: quantity("limit")?,
            }),
            "utilization" => Ok(Reading::Utilization {
                used_basis_points: quantity("used_basis_points")?,
            }),
            "undisclosed" => Ok(Reading::Undisclosed),
            other => Err(Error::custom(format!("unknown variant `{other}`"))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpstreamEnforcement {
    Hard,
    Soft,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Constraint {
    pub id: String,
    pub scope: Scope,
    pub metric: Metric,
    pub unit: Unit,
    pub window: Window,
    pub reading: Reading,
    /// RFC3339 timestamp, kept as the raw wire string; parsed on demand
    /// by [`crate::freshness`] rather than at deserialize time, so a
    /// malformed timestamp is a freshness-evaluation failure, not a
    /// wire-shape failure.
    pub observed_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_until: Option<String>,
    pub upstream_enforcement: UpstreamEnforcement,
    /// Non-secret deterministic upstream account id, as reported by the
    /// extension. Never echoed from the request — the host must not
    /// trust this value until [`crate::freshness`] checks it against
    /// the registration's expected value for this constraint id.
    pub account_fingerprint: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissingReason {
    PermissionDenied,
    Unavailable,
    Timeout,
    RateLimited,
    UnsupportedMetric,
    UnsupportedUnit,
    InconsistentReading,
    SourceTimeUnknown,
    AccountFingerprintMismatch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MissingConstraint {
    pub id: String,
    pub reason: MissingReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    Complete,
    Partial,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedContext {
    pub selector: String,
    pub handle: String,
    pub principal: String,
    pub epoch: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotResult {
    pub context: ObservedContext,
    pub authority: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_valid_until: Option<String>,
    pub coverage: Coverage,
    pub constraints: Vec<Constraint>,
    pub missing: Vec<MissingConstraint>,
}

impl SnapshotResult {
    /// The raw wire coverage rule (HORO-1853 design-v2): `complete`
    /// requires `missing` empty and `constraints` non-empty; `partial`
    /// requires `missing` non-empty; every id in `requested` must
    /// appear in exactly one of `constraints`/`missing`, and no
    /// unrequested id may appear in either. This is purely structural —
    /// it does not know about freshness or account-fingerprint binding.
    pub fn validate_wire_coverage(&self, requested: &[String]) -> Result<(), WireValidationError> {
        match self.coverage {
            Coverage::Complete => {
                if !self.missing.is_empty() || self.constraints.is_empty() {
                    return Err(WireValidationError::CompleteCoverageViolated);
                }
            }
            Coverage::Partial => {
                if self.missing.is_empty() {
                    return Err(WireValidationError::PartialCoverageViolated);
                }
            }
        }
        let mut seen = std::collections::HashSet::new();
        for id in self.constraints.iter().map(|c| &c.id) {
            if !requested.iter().any(|r| r == id) {
                return Err(WireValidationError::UnrequestedConstraintId(id.clone()));
            }
            if !seen.insert(id.clone()) {
                return Err(WireValidationError::DuplicateConstraintId(id.clone()));
            }
        }
        for id in self.missing.iter().map(|m| &m.id) {
            if !requested.iter().any(|r| r == id) {
                return Err(WireValidationError::UnrequestedConstraintId(id.clone()));
            }
            if !seen.insert(id.clone()) {
                return Err(WireValidationError::DuplicateConstraintId(id.clone()));
            }
        }
        for id in requested {
            if !seen.contains(id) {
                return Err(WireValidationError::MissingConstraintId(id.clone()));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotResponse {
    pub protocol_version: String,
    pub request_id: String,
    pub extension_id: String,
    pub extension_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<SnapshotResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<WireError>,
}

impl SnapshotResponse {
    pub fn validate(&self, requested: &[String]) -> Result<(), WireValidationError> {
        match (&self.result, &self.error) {
            (Some(_), Some(_)) => Err(WireValidationError::BothResultAndError),
            (None, None) => Err(WireValidationError::NeitherResultNorError),
            (None, Some(error)) => error.validate(),
            (Some(result), None) => result.validate_wire_coverage(requested),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantity_accepts_zero_and_i64_max_boundary() {
        assert_eq!(Quantity::parse("0").unwrap().value(), 0);
        assert_eq!(
            Quantity::parse("9223372036854775807").unwrap().value(),
            i64::MAX
        );
    }

    #[test]
    fn quantity_rejects_above_i64_max() {
        assert_eq!(
            Quantity::parse("9223372036854775808").unwrap_err(),
            QuantityError::TooLarge
        );
    }

    #[test]
    fn quantity_rejects_sign_leading_zero_exponent_and_empty() {
        assert_eq!(
            Quantity::parse("+1").unwrap_err(),
            QuantityError::NotDecimal
        );
        assert_eq!(
            Quantity::parse("-0").unwrap_err(),
            QuantityError::NotDecimal
        );
        assert_eq!(
            Quantity::parse("01").unwrap_err(),
            QuantityError::LeadingZero
        );
        assert_eq!(
            Quantity::parse("1e3").unwrap_err(),
            QuantityError::NotDecimal
        );
        assert_eq!(Quantity::parse("").unwrap_err(), QuantityError::Empty);
    }

    #[test]
    fn quantity_rejects_json_number_and_requires_json_string() {
        let err = serde_json::from_str::<Quantity>("5").unwrap_err();
        assert!(err.to_string().contains("string"));
        let ok: Quantity = serde_json::from_str("\"5\"").unwrap();
        assert_eq!(ok.value(), 5);
    }

    #[test]
    fn quantity_round_trips_the_canonical_string_verbatim() {
        let q = Quantity::parse("42").unwrap();
        let json = serde_json::to_string(&q).unwrap();
        assert_eq!(json, "\"42\"");
    }

    #[test]
    fn unit_rejects_extra_field_on_unit_variant() {
        let err = serde_json::from_str::<Unit>(r#"{"kind":"tokens","x":1}"#).unwrap_err();
        assert!(err.to_string().contains("unknown field") || err.to_string().contains("x"));
    }

    #[test]
    fn reading_rejects_extra_field_on_undisclosed_variant() {
        let err = serde_json::from_str::<Reading>(r#"{"kind":"undisclosed","x":1}"#).unwrap_err();
        assert!(err.to_string().contains("unknown field") || err.to_string().contains("x"));
    }

    #[test]
    fn scope_rejects_extra_field() {
        let err = serde_json::from_str::<Scope>(r#"{"kind":"principal","handle":"a","x":1}"#)
            .unwrap_err();
        assert!(err.to_string().contains("unknown field") || err.to_string().contains("x"));
    }

    #[test]
    fn window_rejects_extra_field() {
        let err = serde_json::from_str::<Window>(r#"{"kind":"opaque","x":1}"#).unwrap_err();
        assert!(err.to_string().contains("unknown field"));
        let ok: Window = serde_json::from_str(r#"{"kind":"opaque"}"#).unwrap();
        assert_eq!(
            ok,
            Window::Opaque {
                declared_reset_at: None
            }
        );
    }

    #[test]
    fn trust_mode_like_single_variant_enums_reject_other_values() {
        let err = serde_json::from_str::<BootstrapOperation>(r#""snapshot""#).unwrap_err();
        assert!(!err.to_string().is_empty());
        let ok: BootstrapOperation = serde_json::from_str(r#""describe""#).unwrap();
        assert_eq!(ok, BootstrapOperation::Describe);
    }

    #[test]
    fn wire_error_requires_retry_after_seconds_iff_after_delay() {
        let with_delay = WireError {
            code: ErrorCode::RateLimited,
            retry: RetryKind::AfterDelay,
            retry_after_seconds: Some(5),
        };
        assert!(with_delay.validate().is_ok());
        let missing_delay = WireError {
            code: ErrorCode::RateLimited,
            retry: RetryKind::AfterDelay,
            retry_after_seconds: None,
        };
        assert_eq!(
            missing_delay.validate().unwrap_err(),
            WireValidationError::RetryAfterSecondsMismatch
        );
        let unexpected_delay = WireError {
            code: ErrorCode::Timeout,
            retry: RetryKind::Never,
            retry_after_seconds: Some(5),
        };
        assert_eq!(
            unexpected_delay.validate().unwrap_err(),
            WireValidationError::RetryAfterSecondsMismatch
        );
    }

    #[test]
    fn bootstrap_response_rejects_both_and_neither() {
        let both = BootstrapResponse {
            bootstrap_version: "libra.quota-bootstrap/1".into(),
            request_id: "r1".into(),
            result: Some(BootstrapResult {
                extension_id: "e".into(),
                extension_version: "1.0.0".into(),
                supported_versions: vec![],
                capabilities: vec![],
            }),
            error: Some(WireError {
                code: ErrorCode::Timeout,
                retry: RetryKind::Never,
                retry_after_seconds: None,
            }),
        };
        assert_eq!(
            both.validate().unwrap_err(),
            WireValidationError::BothResultAndError
        );
        let neither = BootstrapResponse {
            bootstrap_version: "libra.quota-bootstrap/1".into(),
            request_id: "r1".into(),
            result: None,
            error: None,
        };
        assert_eq!(
            neither.validate().unwrap_err(),
            WireValidationError::NeitherResultNorError
        );
    }

    #[test]
    fn snapshot_request_validates_budget_and_nonempty_constraints() {
        let base = SnapshotRequest {
            protocol_version: "libra.quota-observation/0.1".into(),
            request_id: "r1".into(),
            operation: SnapshotOperation::Snapshot,
            context_selector: "sel".into(),
            constraint_ids: vec!["c1".into()],
            max_age_seconds: 60,
            budget_ms: 10_000,
        };
        assert!(base.validate().is_ok());
        let mut too_big = base.clone();
        too_big.budget_ms = 10_001;
        assert_eq!(
            too_big.validate().unwrap_err(),
            WireValidationError::BudgetTooLarge
        );
        let mut empty = base.clone();
        empty.constraint_ids = vec![];
        assert_eq!(
            empty.validate().unwrap_err(),
            WireValidationError::EmptyConstraintIds
        );
    }

    fn sample_constraint(id: &str) -> Constraint {
        Constraint {
            id: id.into(),
            scope: Scope::Account {
                handle: "acct".into(),
            },
            metric: Metric::TotalTokens,
            unit: Unit::Tokens,
            window: Window::Opaque {
                declared_reset_at: None,
            },
            reading: Reading::Undisclosed,
            observed_at: "2026-10-12T00:00:00Z".into(),
            valid_until: None,
            upstream_enforcement: UpstreamEnforcement::Unknown,
            account_fingerprint: "fp1".into(),
        }
    }

    #[test]
    fn coverage_rule_complete_requires_empty_missing_and_nonempty_constraints() {
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
        assert_eq!(
            result.validate_wire_coverage(&requested).unwrap_err(),
            WireValidationError::CompleteCoverageViolated
        );
    }

    #[test]
    fn coverage_rule_every_requested_id_exactly_once() {
        let requested = vec!["c1".to_string(), "c2".to_string()];
        let mut result = SnapshotResult {
            context: ObservedContext {
                selector: "s".into(),
                handle: "h".into(),
                principal: "p".into(),
                epoch: "e".into(),
            },
            authority: "a".into(),
            context_valid_until: None,
            coverage: Coverage::Complete,
            constraints: vec![sample_constraint("c1"), sample_constraint("c2")],
            missing: vec![],
        };
        assert!(result.validate_wire_coverage(&requested).is_ok());

        // Present in both -> duplicate.
        result.missing.push(MissingConstraint {
            id: "c1".into(),
            reason: MissingReason::Timeout,
        });
        result.coverage = Coverage::Partial;
        assert_eq!(
            result.validate_wire_coverage(&requested).unwrap_err(),
            WireValidationError::DuplicateConstraintId("c1".into())
        );

        // Present in neither.
        let mut missing_one = SnapshotResult {
            context: result.context.clone(),
            authority: "a".into(),
            context_valid_until: None,
            coverage: Coverage::Partial,
            constraints: vec![sample_constraint("c1")],
            missing: vec![MissingConstraint {
                id: "c3".into(),
                reason: MissingReason::Timeout,
            }],
        };
        assert_eq!(
            missing_one.validate_wire_coverage(&requested).unwrap_err(),
            WireValidationError::UnrequestedConstraintId("c3".into())
        );
        missing_one.missing = vec![];
        missing_one.coverage = Coverage::Complete;
        assert_eq!(
            missing_one.validate_wire_coverage(&requested).unwrap_err(),
            WireValidationError::MissingConstraintId("c2".into())
        );
    }
}
