//! Worst-case pre-flight reservation and exact post-hoc settlement
//! arithmetic (HORO-1144).
//!
//! # The direction of every approximation
//!
//! Before a request is forwarded the gateway knows three things: the
//! request body's byte length, the declared `max_tokens`, and the model.
//! It does not know what the provider's tokenizer will make of the body,
//! and it does not know which cache tier the input will be billed at.
//!
//! Every approximation here therefore errs toward reserving **too much**,
//! never too little, because the failure modes are not symmetric:
//! over-reserving costs the user some temporarily unavailable headroom
//! that settlement refunds moments later, while under-reserving means the
//! hard boundary this whole component exists to provide was never
//! actually there. [`CONSERVATIVE_BYTES_PER_TOKEN`] is a *lower* bound on
//! bytes per token precisely so `bytes / that` is an *upper* bound on
//! tokens, and a USD reservation prices input at the most expensive cache
//! tier the model has.
//!
//! Settlement then corrects downward from the provider's own exact
//! figures — see [`settled_cost`] and [`crate::usage`].
//!
//! # The resource kind comes from the budget, never from what we can price
//!
//! `LedgerStore::reserve` returns a hard error (not a rejection) when a
//! reservation's [`ResourceKind`] differs from the task budget's own. So
//! [`worst_case_reservation`] takes the budget's kind as an input and
//! branches on it. Having pricing available never causes it to produce a
//! USD amount for a token-denominated budget.

use libra_governor_domain::{ResourceAmount, ResourceKind};

use crate::pricing::{ModelPricing, PricingTable};
use crate::usage::ObservedUsage;

/// A deliberately low bytes-per-token figure. Real English prose runs
/// nearer 4 bytes/token and code somewhat lower; using 3.0 makes
/// `ceil(bytes / 3.0)` an upper bound on the token count for realistic
/// request bodies rather than a central estimate. See module docs on why
/// the bound must point this way.
pub const CONSERVATIVE_BYTES_PER_TOKEN: f64 = 3.0;

/// Why a request cannot be metered exactly enough to enforce a hard
/// boundary against it (HORO-1144).
///
/// Every variant is a *refusal*, not a warning: the gateway forwards
/// nothing it cannot account for, because forwarding it would make the
/// enforcement claim false. Each variant names the concrete missing
/// input so the `x-libra-decision` header and the `gateway_requests` row
/// can say why without leaking anything.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EnforcementGap {
    /// The task budget is denominated in a kind that cannot be derived
    /// from token counts. A subscription quota percentage is the real
    /// case: nothing in a request or response tells us what fraction of
    /// an opaque period quota it consumed.
    #[error("resource kind {kind:?} cannot be derived from token counts")]
    UnsupportedResourceKind { kind: ResourceKind },
    /// The request declared no positive `max_tokens`, so its output is
    /// unbounded and no finite worst case exists. Reserving zero would be
    /// an unbounded call wearing a bounded reservation.
    #[error("request declares no positive max_tokens, so its cost has no finite upper bound")]
    UnboundedRequest,
    /// The pinned pricing table (plus any operator overrides) has no
    /// entry for this model, and the budget is denominated in currency.
    #[error("model {model} has no pinned price, so a USD budget cannot be enforced against it")]
    UnpricedModel { model: String },
}

/// Upper bound on the input tokens a request body of `body_bytes` can
/// possibly represent. Saturates at zero for an empty body.
pub fn input_token_upper_bound(body_bytes: usize) -> u64 {
    (body_bytes as f64 / CONSERVATIVE_BYTES_PER_TOKEN).ceil() as u64
}

/// Converts a USD figure into the integer cents
/// [`ResourceAmount::UsdCents`] stores, rounding **up** so the conversion
/// itself can never round a reservation below the real cost.
fn usd_to_cents_ceil(usd: f64) -> i64 {
    if !usd.is_finite() || usd <= 0.0 {
        return 0;
    }
    (usd * 100.0).ceil() as i64
}

/// The worst-case cost of a request, in `kind`, given its body size, its
/// declared `max_tokens`, and (for a currency budget) its model's price.
///
/// `max_tokens` must be positive; `None` or zero is
/// [`EnforcementGap::UnboundedRequest`], never a zero reservation.
pub fn worst_case_reservation(
    kind: ResourceKind,
    body_bytes: usize,
    max_tokens: Option<u64>,
    model: &str,
    pricing: &PricingTable,
) -> Result<ResourceAmount, EnforcementGap> {
    let Some(max_tokens) = max_tokens.filter(|m| *m > 0) else {
        return Err(EnforcementGap::UnboundedRequest);
    };
    let input_upper = input_token_upper_bound(body_bytes);

    match kind {
        ResourceKind::Tokens => Ok(ResourceAmount::Tokens(
            input_upper.saturating_add(max_tokens),
        )),
        ResourceKind::Usd => {
            let Some(model_pricing) = pricing.lookup(model) else {
                return Err(EnforcementGap::UnpricedModel {
                    model: model.to_string(),
                });
            };
            let usd = (input_upper as f64) * model_pricing.worst_case_input_usd_per_mtok()
                / 1_000_000.0
                + (max_tokens as f64) * model_pricing.output_usd_per_mtok / 1_000_000.0;
            Ok(ResourceAmount::UsdCents(usd_to_cents_ceil(usd)))
        }
        ResourceKind::QuotaPercent => Err(EnforcementGap::UnsupportedResourceKind { kind }),
    }
}

/// The exact cost of a completed request, in `kind`, from the provider's
/// own reported per-tier token counts.
///
/// Unlike [`worst_case_reservation`] this rounds *up* to whole cents too:
/// a settlement that rounded down would, across many requests,
/// systematically under-count real spend against a hard limit.
pub fn settled_cost(
    kind: ResourceKind,
    usage: &ObservedUsage,
    model: &str,
    pricing: &PricingTable,
) -> Result<ResourceAmount, EnforcementGap> {
    match kind {
        ResourceKind::Tokens => Ok(ResourceAmount::Tokens(usage.total_tokens())),
        ResourceKind::Usd => {
            let Some(model_pricing) = pricing.lookup(model) else {
                return Err(EnforcementGap::UnpricedModel {
                    model: model.to_string(),
                });
            };
            Ok(ResourceAmount::UsdCents(usd_to_cents_ceil(exact_usd(
                usage,
                &model_pricing,
            ))))
        }
        ResourceKind::QuotaPercent => Err(EnforcementGap::UnsupportedResourceKind { kind }),
    }
}

/// Prices each reported token tier at its own rate. Separated from
/// [`settled_cost`] so the per-tier arithmetic is directly testable
/// without constructing a [`ResourceAmount`].
fn exact_usd(usage: &ObservedUsage, pricing: &ModelPricing) -> f64 {
    (usage.input_tokens as f64) * pricing.input_usd_per_mtok / 1_000_000.0
        + (usage.cache_creation_input_tokens as f64) * pricing.cache_creation_input_usd_per_mtok
            / 1_000_000.0
        + (usage.cache_read_input_tokens as f64) * pricing.cache_read_input_usd_per_mtok
            / 1_000_000.0
        + (usage.output_tokens as f64) * pricing.output_usd_per_mtok / 1_000_000.0
}

/// `true` when the provider reported more output tokens than the request
/// declared as its `max_tokens` ceiling.
///
/// That should be impossible. It is checked rather than assumed because
/// the entire reservation rests on `max_tokens` genuinely bounding
/// output: if it ever does not, the bound is wrong and the operator needs
/// to know, not have it silently clamped away. Recorded as
/// `gateway_requests.bound_violated`.
pub fn bound_violated(max_tokens: u64, observed_output_tokens: u64) -> bool {
    observed_output_tokens > max_tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, cache_create: u64, cache_read: u64, output: u64) -> ObservedUsage {
        ObservedUsage {
            input_tokens: input,
            cache_creation_input_tokens: cache_create,
            cache_read_input_tokens: cache_read,
            output_tokens: output,
        }
    }

    #[test]
    fn token_reservation_is_input_upper_bound_plus_max_tokens() {
        let reserved = worst_case_reservation(
            ResourceKind::Tokens,
            3_000,
            Some(1_000),
            "claude-sonnet-4-5",
            &PricingTable::pinned(),
        )
        .unwrap();
        assert_eq!(reserved, ResourceAmount::Tokens(1_000 + 1_000));
    }

    #[test]
    fn input_bound_rounds_up_so_it_can_never_under_count() {
        assert_eq!(input_token_upper_bound(0), 0);
        assert_eq!(input_token_upper_bound(1), 1);
        assert_eq!(input_token_upper_bound(4), 2, "4/3 = 1.33 must round to 2");
        assert_eq!(input_token_upper_bound(3), 1);
    }

    #[test]
    fn a_token_budget_never_needs_pricing() {
        let reserved = worst_case_reservation(
            ResourceKind::Tokens,
            300,
            Some(50),
            "an-utterly-unknown-model",
            &PricingTable::pinned(),
        )
        .expect("a token-denominated budget must not depend on a price list");
        assert_eq!(reserved, ResourceAmount::Tokens(150));
    }

    #[test]
    fn a_usd_budget_refuses_an_unpriced_model_rather_than_guessing() {
        let gap = worst_case_reservation(
            ResourceKind::Usd,
            300,
            Some(50),
            "an-utterly-unknown-model",
            &PricingTable::pinned(),
        )
        .unwrap_err();
        assert_eq!(
            gap,
            EnforcementGap::UnpricedModel {
                model: "an-utterly-unknown-model".to_string()
            }
        );
    }

    #[test]
    fn quota_percent_is_refused_before_any_reservation_is_attempted() {
        let gap = worst_case_reservation(
            ResourceKind::QuotaPercent,
            300,
            Some(50),
            "claude-sonnet-4-5",
            &PricingTable::pinned(),
        )
        .unwrap_err();
        assert_eq!(
            gap,
            EnforcementGap::UnsupportedResourceKind {
                kind: ResourceKind::QuotaPercent
            }
        );
    }

    #[test]
    fn a_missing_or_zero_max_tokens_is_refused_never_reserved_as_zero() {
        for max_tokens in [None, Some(0)] {
            let gap = worst_case_reservation(
                ResourceKind::Tokens,
                300,
                max_tokens,
                "claude-sonnet-4-5",
                &PricingTable::pinned(),
            )
            .unwrap_err();
            assert_eq!(gap, EnforcementGap::UnboundedRequest);
        }
    }

    #[test]
    fn usd_reservation_prices_input_at_the_most_expensive_cache_tier() {
        let table = PricingTable::pinned();
        let pricing = table.lookup("claude-sonnet-4-5").unwrap();
        // 3000 bytes -> 1000 input tokens upper bound; 1000 max_tokens.
        let reserved = worst_case_reservation(
            ResourceKind::Usd,
            3_000,
            Some(1_000),
            "claude-sonnet-4-5",
            &table,
        )
        .unwrap();

        let expected_usd = 1_000.0 * pricing.cache_creation_input_usd_per_mtok / 1e6
            + 1_000.0 * pricing.output_usd_per_mtok / 1e6;
        assert_eq!(
            reserved,
            ResourceAmount::UsdCents((expected_usd * 100.0).ceil() as i64)
        );
    }

    #[test]
    fn a_usd_reservation_always_covers_the_exact_settlement_it_bounds() {
        let table = PricingTable::pinned();
        // Worst case: every input token billed at cache-creation rate and
        // the model emits exactly its max_tokens.
        let reserved = worst_case_reservation(
            ResourceKind::Usd,
            3_000,
            Some(1_000),
            "claude-sonnet-4-5",
            &table,
        )
        .unwrap();
        let settled = settled_cost(
            ResourceKind::Usd,
            &usage(0, 1_000, 0, 1_000),
            "claude-sonnet-4-5",
            &table,
        )
        .unwrap();
        assert!(
            settled.as_f64() <= reserved.as_f64(),
            "settlement ({settled:?}) exceeded the reservation ({reserved:?}) that was \
             supposed to bound it"
        );
    }

    #[test]
    fn settlement_prices_each_reported_tier_at_its_own_rate() {
        let table = PricingTable::pinned();
        let pricing = table.lookup("claude-sonnet-4-5").unwrap();
        let observed = usage(1_000, 2_000, 4_000, 500);
        let settled =
            settled_cost(ResourceKind::Usd, &observed, "claude-sonnet-4-5", &table).unwrap();

        let expected = 1_000.0 * pricing.input_usd_per_mtok / 1e6
            + 2_000.0 * pricing.cache_creation_input_usd_per_mtok / 1e6
            + 4_000.0 * pricing.cache_read_input_usd_per_mtok / 1e6
            + 500.0 * pricing.output_usd_per_mtok / 1e6;
        assert_eq!(
            settled,
            ResourceAmount::UsdCents((expected * 100.0).ceil() as i64)
        );
    }

    #[test]
    fn token_settlement_sums_every_reported_tier() {
        let settled = settled_cost(
            ResourceKind::Tokens,
            &usage(10, 20, 30, 40),
            "claude-sonnet-4-5",
            &PricingTable::pinned(),
        )
        .unwrap();
        assert_eq!(settled, ResourceAmount::Tokens(100));
    }

    #[test]
    fn cents_conversion_never_yields_a_negative_or_nan_amount() {
        assert_eq!(usd_to_cents_ceil(f64::NAN), 0);
        assert_eq!(usd_to_cents_ceil(f64::INFINITY), 0);
        assert_eq!(usd_to_cents_ceil(-1.0), 0);
        assert_eq!(usd_to_cents_ceil(0.0), 0);
        assert_eq!(usd_to_cents_ceil(0.001), 1, "a sub-cent cost must round up");
    }

    #[test]
    fn bound_violation_is_detected_rather_than_clamped() {
        assert!(!bound_violated(1_000, 1_000));
        assert!(!bound_violated(1_000, 999));
        assert!(bound_violated(1_000, 1_001));
    }
}
