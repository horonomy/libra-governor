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
}
