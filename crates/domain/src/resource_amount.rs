//! [`ResourceAmount`] — a generic representation of "how much this cost".
//!
//! Libra's North Star is about affordability, not dollars specifically:
//! a provider may bill in real currency, a task may be metered in raw
//! tokens with no price attached, or a subscription plan may only expose
//! a quota percentage consumed. Hardcoding a `cost_usd: f64` field would
//! force a fake, potentially misleading USD conversion onto all three
//! cases. Instead every amount carries its own unit explicitly, so
//! estimator and ledger code must handle (or deliberately reject mixing)
//! units rather than silently summing incompatible numbers.

use serde::{Deserialize, Serialize};

/// A single resource measurement, tagged with the unit it was measured in.
///
/// `Usd` stores minor units (cents) as an integer to avoid floating-point
/// rounding error in money accounting; `Tokens` is an exact integer count;
/// `QuotaPercent` is a `0.0..=100.0` fraction of a subscription's period
/// quota consumed, since some providers only expose usage as a percentage
/// with no per-request price at all.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "amount", rename_all = "snake_case")]
pub enum ResourceAmount {
    /// US dollars, represented as integer cents.
    UsdCents(i64),
    /// A raw token count (input + output, or as defined by the caller).
    Tokens(u64),
    /// Percentage (0.0-100.0) of a subscription period quota consumed.
    QuotaPercent(f32),
}

impl ResourceAmount {
    /// The [`ResourceKind`] this amount is measured in.
    pub fn kind(&self) -> ResourceKind {
        match self {
            ResourceAmount::UsdCents(_) => ResourceKind::Usd,
            ResourceAmount::Tokens(_) => ResourceKind::Tokens,
            ResourceAmount::QuotaPercent(_) => ResourceKind::QuotaPercent,
        }
    }

    /// The raw numeric value, regardless of unit (HORO-1137).
    ///
    /// Deliberately does not implement `PartialOrd`/`Ord` on the enum
    /// itself: comparing two [`ResourceAmount`]s of different
    /// [`ResourceKind`]s (e.g. dollars vs. tokens) is meaningless and
    /// must not silently type-check. Callers that already know (or have
    /// validated) both amounts share a kind — such as
    /// `crate::policy::Policy` evaluation, which validates this at
    /// construction — may compare via this accessor. Callers that
    /// haven't established a shared kind should use [`Self::kind`] to
    /// check first.
    pub fn as_f64(&self) -> f64 {
        match self {
            ResourceAmount::UsdCents(c) => *c as f64,
            ResourceAmount::Tokens(t) => *t as f64,
            ResourceAmount::QuotaPercent(p) => *p as f64,
        }
    }

    /// Scales this amount by `factor`, preserving its [`ResourceKind`]
    /// (HORO-1137). Used to derive an elastic/hard ceiling from a target
    /// amount (e.g. `target.scaled(1.5)`) without hardcoding a
    /// unit-specific multiplication in policy-preset code. Rounds to the
    /// unit's natural precision (whole cents, whole tokens).
    ///
    /// [`ResourceAmount::QuotaPercent`]'s documented domain is
    /// `0.0..=100.0` — a subscription cannot consume more than 100% of
    /// its own period quota. `scaled` saturates at that domain limit
    /// (e.g. `QuotaPercent(80.0).scaled(2.0)` is `QuotaPercent(100.0)`,
    /// not `160.0`) rather than producing a value the type's own docs
    /// say cannot exist.
    pub fn scaled(&self, factor: f64) -> ResourceAmount {
        match self {
            ResourceAmount::UsdCents(c) => {
                ResourceAmount::UsdCents(((*c as f64) * factor).round() as i64)
            }
            ResourceAmount::Tokens(t) => {
                ResourceAmount::Tokens(((*t as f64) * factor).round() as u64)
            }
            ResourceAmount::QuotaPercent(p) => {
                ResourceAmount::QuotaPercent((*p * factor as f32).clamp(0.0, 100.0))
            }
        }
    }

    /// Reconstructs a non-negative amount from a stored `(kind, value)`
    /// pair — the shape the reservation ledger persists (a `REAL` column
    /// plus a kind discriminator) so `available` headroom can be computed
    /// in a single SQL expression rather than summed from JSON blobs
    /// (HORO-1141). Rounds to the unit's natural precision and saturates
    /// `QuotaPercent` at its documented `0.0..=100.0` domain, exactly as
    /// [`Self::scaled`] does. Negative input saturates at zero: this is
    /// the *non-negative stored amount* constructor — signed headroom
    /// that can genuinely go negative (an overrun) is [`Headroom`], which
    /// exists precisely because `Tokens(u64)`/`QuotaPercent` cannot
    /// represent that.
    pub fn from_kind_f64(kind: ResourceKind, value: f64) -> ResourceAmount {
        let value = value.max(0.0);
        match kind {
            ResourceKind::Usd => ResourceAmount::UsdCents(value.round() as i64),
            ResourceKind::Tokens => ResourceAmount::Tokens(value.round() as u64),
            ResourceKind::QuotaPercent => {
                ResourceAmount::QuotaPercent((value as f32).clamp(0.0, 100.0))
            }
        }
    }
}

/// Signed remaining capacity for one resource kind (HORO-1141).
///
/// Deliberately NOT a [`ResourceAmount`]: `Tokens(u64)` and
/// `QuotaPercent(0.0..=100.0)` cannot represent the negative headroom an
/// overrun produces, and saturating at zero at the storage layer would
/// hide exactly the condition a caller needs to see (the reservation
/// ledger's "actual cost above reservation" failure case).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Headroom {
    pub kind: ResourceKind,
    pub value: f64,
}

impl Headroom {
    /// `true` when there is no remaining capacity left (zero or
    /// negative).
    pub fn is_exhausted(&self) -> bool {
        self.value <= 0.0
    }

    /// For display/serialization only — clamps a negative value at zero.
    /// Never use this for an admission comparison; compare `value`
    /// directly so an overrun stays visible.
    pub fn as_resource_amount_clamped(&self) -> ResourceAmount {
        ResourceAmount::from_kind_f64(self.kind, self.value)
    }
}

/// The unit a [`ResourceAmount`] is measured in, without the value —
/// useful for comparing/grouping amounts by kind before deciding whether
/// they may be combined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Usd,
    Tokens,
    QuotaPercent,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_matches_variant() {
        assert_eq!(ResourceAmount::UsdCents(199).kind(), ResourceKind::Usd);
        assert_eq!(ResourceAmount::Tokens(500).kind(), ResourceKind::Tokens);
        assert_eq!(
            ResourceAmount::QuotaPercent(12.5).kind(),
            ResourceKind::QuotaPercent
        );
    }

    #[test]
    fn as_f64_reads_the_raw_numeric_value() {
        assert_eq!(ResourceAmount::UsdCents(199).as_f64(), 199.0);
        assert_eq!(ResourceAmount::Tokens(500).as_f64(), 500.0);
        assert_eq!(ResourceAmount::QuotaPercent(12.5).as_f64(), 12.5_f64);
    }

    #[test]
    fn scaled_preserves_kind_and_multiplies() {
        assert_eq!(
            ResourceAmount::UsdCents(1000).scaled(1.5),
            ResourceAmount::UsdCents(1500)
        );
        assert_eq!(
            ResourceAmount::Tokens(200).scaled(2.0),
            ResourceAmount::Tokens(400)
        );
        assert_eq!(
            ResourceAmount::QuotaPercent(10.0).scaled(2.0),
            ResourceAmount::QuotaPercent(20.0)
        );
    }

    #[test]
    fn scaled_saturates_quota_percent_at_its_documented_domain() {
        assert_eq!(
            ResourceAmount::QuotaPercent(80.0).scaled(2.0),
            ResourceAmount::QuotaPercent(100.0),
            "160% of a period quota is not representable — must saturate at 100.0"
        );
    }

    #[test]
    fn from_kind_f64_reconstructs_amounts_by_kind() {
        assert_eq!(
            ResourceAmount::from_kind_f64(ResourceKind::Usd, 199.0),
            ResourceAmount::UsdCents(199)
        );
        assert_eq!(
            ResourceAmount::from_kind_f64(ResourceKind::Tokens, 500.0),
            ResourceAmount::Tokens(500)
        );
        assert_eq!(
            ResourceAmount::from_kind_f64(ResourceKind::QuotaPercent, 12.5),
            ResourceAmount::QuotaPercent(12.5)
        );
    }

    #[test]
    fn from_kind_f64_saturates_negative_input_at_zero() {
        assert_eq!(
            ResourceAmount::from_kind_f64(ResourceKind::Tokens, -5.0),
            ResourceAmount::Tokens(0)
        );
    }

    #[test]
    fn from_kind_f64_saturates_quota_percent_at_its_documented_domain() {
        assert_eq!(
            ResourceAmount::from_kind_f64(ResourceKind::QuotaPercent, 160.0),
            ResourceAmount::QuotaPercent(100.0)
        );
    }

    #[test]
    fn headroom_reports_exhaustion_including_negative_overrun() {
        let positive = Headroom {
            kind: ResourceKind::Tokens,
            value: 10.0,
        };
        let zero = Headroom {
            kind: ResourceKind::Tokens,
            value: 0.0,
        };
        let negative = Headroom {
            kind: ResourceKind::Tokens,
            value: -5.0,
        };
        assert!(!positive.is_exhausted());
        assert!(zero.is_exhausted());
        assert!(negative.is_exhausted());
    }

    #[test]
    fn headroom_clamped_display_never_goes_negative() {
        let negative = Headroom {
            kind: ResourceKind::Tokens,
            value: -5.0,
        };
        assert_eq!(
            negative.as_resource_amount_clamped(),
            ResourceAmount::Tokens(0)
        );
    }
}
