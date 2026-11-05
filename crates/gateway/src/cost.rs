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
    /// The request references input the gateway cannot bound from the
    /// body: a server tool, a url- or file-sourced block, an MCP server
    /// or a container. Only the shared-budget (governor-held) mode
    /// refuses on this (S11b).
    #[error("request references {what}, so its input has no finite upper bound")]
    UnboundedInput { what: &'static str },
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
            worst_case_usd_reservation(input_upper, Some(max_tokens), model, pricing)
        }
        ResourceKind::QuotaPercent => Err(EnforcementGap::UnsupportedResourceKind { kind }),
    }
}

/// The worst-case USD cost of a request whose input is bounded by
/// `input_upper` tokens, rounded up to whole cents (S11d). Input is priced at
/// the model's most expensive tier. [`worst_case_reservation`] derives
/// `input_upper` from the body size; the shared-budget (governor-held) mode
/// derives it from [`hard_input_upper_bound`] instead.
///
/// `max_tokens` must be positive; `None` or zero is
/// [`EnforcementGap::UnboundedRequest`], never a zero reservation.
pub fn worst_case_usd_reservation(
    input_upper: u64,
    max_tokens: Option<u64>,
    model: &str,
    pricing: &PricingTable,
) -> Result<ResourceAmount, EnforcementGap> {
    let Some(max_tokens) = max_tokens.filter(|m| *m > 0) else {
        return Err(EnforcementGap::UnboundedRequest);
    };
    let Some(model_pricing) = pricing.lookup(model) else {
        return Err(EnforcementGap::UnpricedModel {
            model: model.to_string(),
        });
    };
    let usd = (input_upper as f64) * model_pricing.worst_case_input_usd_per_mtok() / 1_000_000.0
        + (max_tokens as f64) * model_pricing.output_usd_per_mtok / 1_000_000.0;
    Ok(ResourceAmount::UsdCents(usd_to_cents_ceil(usd)))
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
        ResourceKind::Tokens => Ok(ResourceAmount::Tokens(usage.fresh_tokens())),
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

/// Pre-flight input-token ceiling for the shared-budget (governor-held)
/// mode, where an overrun is a breach of the cap rather than a statusline
/// note.
///
/// # Contract
/// - `bytes` excludes inline base64 `source.data` payloads; those are
///   billed by rendering, not bytes, and are covered by `image_count`.
/// - Byte-level BPE yields at least one byte per token [I], so `bytes`
///   itself is the ceiling, against the heuristic `bytes / 3` of
///   [`input_token_upper_bound`].
/// - `image_count` blocks each add [`IMAGE_MAX_TOKENS`].
/// - [`TOOL_PROMPT_TOKENS`] is always added for the API's tool-use system
///   prompt, which is not in the body.
pub fn hard_input_upper_bound(bytes: usize, image_count: usize) -> u64 {
    (bytes as u64)
        .saturating_add((image_count as u64).saturating_mul(IMAGE_MAX_TOKENS))
        .saturating_add(TOOL_PROMPT_TOKENS)
}

/// Per-image input ceiling. [I] until S11a pins it from the API docs.
pub const IMAGE_MAX_TOKENS: u64 = 8_000;

/// Allowance for the tool-use system prompt the API prepends. [I] until
/// S11a pins it.
pub const TOOL_PROMPT_TOKENS: u64 = 1_000;

/// Refuses a body whose input cannot be bounded from its own bytes
/// (S11b): server tools (`web_search_*`, `web_fetch_*`,
/// `code_execution_*`), a `url` or `file` content source, `mcp_servers`
/// a `container`, a `container_upload` block (a Files-API reference
/// with no `source`), or a base64 PDF (billed per page, not per byte).
/// Returns the first gap found.
///
/// The list is closed: a server-tool or block type added upstream passes
/// until it is named here, so S11a must re-check it against the API docs.
///
/// Structural only — nothing from the body is retained. A body that does
/// not parse as JSON returns `None`: envelope parsing already refuses it
/// and this is not the place to repeat that decision.
pub fn screen_unbounded_inputs(body: &[u8]) -> Option<EnforcementGap> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let unbounded = |what| Some(EnforcementGap::UnboundedInput { what });

    let server_tool = |tool: &serde_json::Value| {
        tool.get("type").and_then(|t| t.as_str()).is_some_and(|t| {
            ["web_search_", "web_fetch_", "code_execution_"]
                .iter()
                .any(|prefix| t.starts_with(prefix))
        })
    };
    if value
        .get("tools")
        .and_then(|t| t.as_array())
        .is_some_and(|tools| tools.iter().any(server_tool))
    {
        return unbounded("a server tool");
    }
    if value
        .get("mcp_servers")
        .and_then(|m| m.as_array())
        .is_some_and(|servers| !servers.is_empty())
    {
        return unbounded("an MCP server");
    }
    if value.get("container").is_some_and(|c| !c.is_null()) {
        return unbounded("a container");
    }
    has_unbounded_source(&value).then_some(EnforcementGap::UnboundedInput {
        what: "a url, file or base64 PDF content source",
    })
}

/// A base64 PDF is billed per rendered page, which its byte length does not
/// bound (a small PDF can have many pages), so it is as unbounded as a url.
fn is_base64_pdf(source: &serde_json::Value) -> bool {
    source.get("type").and_then(|t| t.as_str()) == Some("base64")
        && source.get("media_type").and_then(|t| t.as_str()) == Some("application/pdf")
}

/// How many image blocks a request body carries, at any depth.
///
/// The shared-budget reservation takes the whole body length as its input
/// bound (looser than excluding inline base64, hence safe) and adds
/// [`IMAGE_MAX_TOKENS`] per image, because a small, highly compressible
/// image can render to more tokens than its bytes. Structural only.
pub fn image_block_count(body: &[u8]) -> usize {
    fn count(value: &serde_json::Value) -> usize {
        match value {
            serde_json::Value::Object(map) => {
                usize::from(map.get("type").and_then(|t| t.as_str()) == Some("image"))
                    + map.values().map(count).sum::<usize>()
            }
            serde_json::Value::Array(items) => items.iter().map(count).sum(),
            _ => 0,
        }
    }
    serde_json::from_slice::<serde_json::Value>(body)
        .map(|v| count(&v))
        .unwrap_or(0)
}

/// `true` if any `source` object in `value`, at any depth, is of type
/// `url` or `file`, or any block is a `container_upload`. Keyed on the
/// `source` and `type` fields so prose that merely mentions those words
/// never matches.
fn has_unbounded_source(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(map) => {
            map.get("type").and_then(|t| t.as_str()) == Some("container_upload")
                || map.iter().any(|(key, child)| {
                    let is_unbounded = key == "source"
                        && child
                            .get("type")
                            .and_then(|t| t.as_str())
                            .is_some_and(|t| t == "url" || t == "file" || is_base64_pdf(child));
                    is_unbounded || has_unbounded_source(child)
                })
        }
        serde_json::Value::Array(items) => items.iter().any(has_unbounded_source),
        _ => false,
    }
}

/// Flags raised while settling a shared-budget request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SettleFlags {
    /// The stream ended without `message_stop` and was settled at the
    /// conservative amount.
    pub truncated_settle: bool,
    /// Reported prompt tokens exceeded the pre-flight input bound.
    pub input_bound_violated: bool,
}

/// What a shared-budget request settles at (S11b). Pure: the caller owns
/// the stats counters and the ledger call.
///
/// - Completed cleanly with usage: the exact tiers, `usage_known = true`.
/// - Cut off after `message_start` usage: the exact input tiers plus
///   `max_tokens` of output (the provider may have produced all of it
///   unseen), `usage_known = false`, `truncated_settle`.
/// - Cut off before any usage, or a model/kind that cannot be priced:
///   `reserved`, `usage_known = false`.
///
/// `input_bound` is the token ceiling the reservation assumed; reported
/// prompt tokens above it set `input_bound_violated`. The amount is never
/// clamped to `reserved`, so an overrun stays visible.
#[allow(clippy::too_many_arguments)]
pub fn settle_amount_for(
    observed: Option<ObservedUsage>,
    completed_cleanly: bool,
    saw_message_start: bool,
    reserved: ResourceAmount,
    max_tokens: u64,
    input_bound: u64,
    kind: ResourceKind,
    model: &str,
    pricing: &PricingTable,
) -> (ResourceAmount, bool, SettleFlags) {
    let mut flags = SettleFlags::default();
    let Some(usage) = observed else {
        return (reserved, false, flags);
    };
    let prompt_tokens = usage
        .input_tokens
        .saturating_add(usage.cache_creation_input_tokens)
        .saturating_add(usage.cache_read_input_tokens);
    flags.input_bound_violated = prompt_tokens > input_bound;

    let (priced, usage_known) = if completed_cleanly {
        (usage, true)
    } else if saw_message_start {
        flags.truncated_settle = true;
        let worst_case_output = ObservedUsage {
            output_tokens: usage.output_tokens.max(max_tokens),
            ..usage
        };
        (worst_case_output, false)
    } else {
        return (reserved, false, flags);
    };
    match settled_cost(kind, &priced, model, pricing) {
        Ok(amount) => (amount, usage_known, flags),
        Err(_) => (reserved, false, flags),
    }
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
    fn token_settlement_excludes_cache_reads_and_stays_within_reservation() {
        let heavy_cache = usage(1_000, 0, 100_000, 800);
        let settled = settled_cost(
            ResourceKind::Tokens,
            &heavy_cache,
            "claude-sonnet-4-5",
            &PricingTable::pinned(),
        )
        .unwrap();
        assert_eq!(settled, ResourceAmount::Tokens(1_800));

        let reserved = worst_case_reservation(
            ResourceKind::Tokens,
            3_000,
            Some(1_000),
            "claude-sonnet-4-5",
            &PricingTable::pinned(),
        )
        .unwrap();
        assert!(settled.as_f64() <= reserved.as_f64());

        // Negative control: a USD budget still prices the cache reads.
        let usd = |usage: &ObservedUsage| {
            settled_cost(
                ResourceKind::Usd,
                usage,
                "claude-sonnet-4-5",
                &PricingTable::pinned(),
            )
            .unwrap()
            .as_f64()
        };
        assert!(usd(&heavy_cache) > usd(&usage(1_000, 0, 0, 800)));
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
    fn explicit_input_bound_prices_like_the_body_derived_one_and_refuses_the_same_gaps() {
        let table = PricingTable::pinned();
        let model = "claude-sonnet-4-5";
        let from_body =
            worst_case_reservation(ResourceKind::Usd, 3_000, Some(1_000), model, &table).unwrap();
        let from_bound =
            worst_case_usd_reservation(input_token_upper_bound(3_000), Some(1_000), model, &table)
                .unwrap();
        assert_eq!(from_body, from_bound);

        assert_eq!(
            worst_case_usd_reservation(10, Some(0), model, &table),
            Err(EnforcementGap::UnboundedRequest)
        );
        assert!(matches!(
            worst_case_usd_reservation(10, Some(1), "no-such-model", &table),
            Err(EnforcementGap::UnpricedModel { .. })
        ));
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
    fn settlement_of_current_models_uses_four_distinct_list_prices() {
        let table = PricingTable::pinned();
        // 1M tokens in every tier: sonnet-5-5 = 2 + 2.5 + 0.10 + 10 = $14.60.
        let observed = usage(1_000_000, 1_000_000, 1_000_000, 1_000_000);
        assert_eq!(
            settled_cost(ResourceKind::Usd, &observed, "claude-sonnet-5-5", &table).unwrap(),
            ResourceAmount::UsdCents(1_460)
        );
        // opus-5-5 = 4 + 5 + 0.20 + 20 = $29.20.
        assert_eq!(
            settled_cost(
                ResourceKind::Usd,
                &observed,
                "claude-opus-5-5-20261024",
                &table
            )
            .unwrap(),
            ResourceAmount::UsdCents(2_920)
        );
        // A model the table still does not know settles as an enforcement gap.
        assert!(matches!(
            settled_cost(ResourceKind::Usd, &observed, "claude-unreleased-9", &table),
            Err(EnforcementGap::UnpricedModel { .. })
        ));
    }

    #[test]
    fn token_settlement_sums_the_fresh_tiers() {
        let settled = settled_cost(
            ResourceKind::Tokens,
            &usage(10, 20, 30, 40),
            "claude-sonnet-4-5",
            &PricingTable::pinned(),
        )
        .unwrap();
        assert_eq!(settled, ResourceAmount::Tokens(70));
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

    // --- S11b: shared-budget settlement rules and input screening ---

    fn sonnet() -> &'static str {
        "claude-sonnet-4-5"
    }

    fn settle(
        observed: Option<ObservedUsage>,
        completed_cleanly: bool,
        saw_message_start: bool,
        kind: ResourceKind,
    ) -> (ResourceAmount, bool, SettleFlags) {
        let reserved = match kind {
            ResourceKind::Tokens => ResourceAmount::Tokens(5_000),
            _ => ResourceAmount::UsdCents(500),
        };
        settle_amount_for(
            observed,
            completed_cleanly,
            saw_message_start,
            reserved,
            1_000,
            2_000,
            kind,
            sonnet(),
            &PricingTable::pinned(),
        )
    }

    #[test]
    fn a_truncated_stream_settles_at_input_plus_max_tokens_output() {
        let (amount, usage_known, flags) = settle(
            Some(usage(120, 0, 0, 17)),
            false,
            true,
            ResourceKind::Tokens,
        );
        assert_eq!(amount, ResourceAmount::Tokens(120 + 1_000));
        assert!(!usage_known);
        assert!(flags.truncated_settle);
    }

    #[test]
    fn a_truncated_usd_settlement_prices_max_tokens_of_output() {
        let (amount, _, _) = settle(Some(usage(120, 0, 0, 17)), false, true, ResourceKind::Usd);
        let expected = settled_cost(
            ResourceKind::Usd,
            &usage(120, 0, 0, 1_000),
            sonnet(),
            &PricingTable::pinned(),
        )
        .unwrap();
        assert_eq!(amount, expected);
    }

    #[test]
    fn a_stream_with_no_message_start_settles_at_the_reservation() {
        let (amount, usage_known, flags) = settle(None, false, false, ResourceKind::Tokens);
        assert_eq!(amount, ResourceAmount::Tokens(5_000));
        assert!(!usage_known);
        assert!(!flags.truncated_settle, "nothing was observed to truncate");
    }

    #[test]
    fn message_start_without_usable_usage_settles_at_the_reservation() {
        let (amount, usage_known, _) = settle(None, false, true, ResourceKind::Tokens);
        assert_eq!(amount, ResourceAmount::Tokens(5_000));
        assert!(!usage_known);
    }

    #[test]
    fn a_clean_completion_settles_at_the_exact_observed_usage() {
        let (amount, usage_known, flags) =
            settle(Some(usage(120, 0, 0, 30)), true, true, ResourceKind::Tokens);
        assert_eq!(amount, ResourceAmount::Tokens(150));
        assert!(usage_known);
        assert_eq!(flags, SettleFlags::default());
    }

    #[test]
    fn a_completion_that_reported_more_output_than_max_tokens_is_not_clamped() {
        let (amount, usage_known, _) = settle(
            Some(usage(120, 0, 0, 5_000)),
            true,
            true,
            ResourceKind::Tokens,
        );
        assert_eq!(amount, ResourceAmount::Tokens(5_120));
        assert!(usage_known);
    }

    #[test]
    fn a_truncated_stream_that_already_exceeded_max_tokens_keeps_the_larger_output() {
        let (amount, _, _) = settle(
            Some(usage(120, 0, 0, 5_000)),
            false,
            true,
            ResourceKind::Tokens,
        );
        assert_eq!(amount, ResourceAmount::Tokens(5_120));
    }

    #[test]
    fn an_unpriced_model_for_a_usd_budget_settles_at_the_reservation() {
        let (amount, usage_known, _) = settle_amount_for(
            Some(usage(120, 0, 0, 30)),
            true,
            true,
            ResourceAmount::UsdCents(500),
            1_000,
            2_000,
            ResourceKind::Usd,
            "an-utterly-unknown-model",
            &PricingTable::pinned(),
        );
        assert_eq!(amount, ResourceAmount::UsdCents(500));
        assert!(!usage_known);
    }

    #[test]
    fn emoji_dense_input_above_the_heuristic_bound_flags_an_input_bound_violation() {
        // 300 emoji are 1200 bytes: the 3-bytes-per-token heuristic
        // allows 400 tokens, and a tokenizer can charge more than that.
        let body_bytes = "\u{1F600}".repeat(300).len();
        let heuristic = input_token_upper_bound(body_bytes);
        let (_, _, flags) = settle_amount_for(
            Some(usage(heuristic + 1, 0, 0, 30)),
            true,
            true,
            ResourceAmount::Tokens(5_000),
            1_000,
            heuristic,
            ResourceKind::Tokens,
            sonnet(),
            &PricingTable::pinned(),
        );
        assert!(flags.input_bound_violated);

        // Negative control: the same usage is inside the hard bound.
        let (_, _, flags) = settle_amount_for(
            Some(usage(heuristic + 1, 0, 0, 30)),
            true,
            true,
            ResourceAmount::Tokens(5_000),
            1_000,
            hard_input_upper_bound(body_bytes, 0),
            ResourceKind::Tokens,
            sonnet(),
            &PricingTable::pinned(),
        );
        assert!(!flags.input_bound_violated);
    }

    #[test]
    fn cache_tiers_count_toward_the_input_bound() {
        let (_, _, flags) = settle_amount_for(
            Some(usage(10, 1_000, 1_000, 30)),
            true,
            true,
            ResourceAmount::Tokens(5_000),
            1_000,
            1_500,
            ResourceKind::Tokens,
            sonnet(),
            &PricingTable::pinned(),
        );
        assert!(flags.input_bound_violated);
    }

    #[test]
    fn the_hard_input_bound_is_one_token_per_byte_plus_image_and_tool_allowances() {
        assert_eq!(hard_input_upper_bound(0, 0), TOOL_PROMPT_TOKENS);
        assert_eq!(hard_input_upper_bound(900, 0), 900 + TOOL_PROMPT_TOKENS);
        assert_eq!(
            hard_input_upper_bound(900, 2),
            900 + 2 * IMAGE_MAX_TOKENS + TOOL_PROMPT_TOKENS
        );
        assert!(hard_input_upper_bound(900, 0) > input_token_upper_bound(900));
    }

    fn gap(body: serde_json::Value) -> Option<EnforcementGap> {
        screen_unbounded_inputs(&serde_json::to_vec(&body).unwrap())
    }

    #[test]
    fn a_plain_body_and_a_base64_image_pass_the_screen() {
        assert_eq!(
            gap(serde_json::json!({
                "model": "m", "max_tokens": 5,
                "tools": [{"name": "Read", "input_schema": {"type": "object"}}],
                "messages": [{"role": "user", "content": [
                    {"type": "text", "text": "hi"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}
                ]}]
            })),
            None
        );
    }

    #[test]
    fn a_web_search_server_tool_is_refused() {
        for tool_type in [
            "web_search_20250305",
            "web_fetch_20250910",
            "code_execution_20250522",
        ] {
            assert!(
                matches!(
                    gap(serde_json::json!({"tools": [{"type": tool_type, "name": "t"}]})),
                    Some(EnforcementGap::UnboundedInput { .. })
                ),
                "{tool_type}"
            );
        }
    }

    #[test]
    fn a_url_or_file_sourced_block_is_refused_even_when_nested() {
        for source_type in ["url", "file"] {
            let body = serde_json::json!({"messages": [{"role": "user", "content": [
                {"type": "tool_result", "content": [
                    {"type": "image", "source": {"type": source_type, "url": "https://x"}}
                ]}
            ]}]});
            assert!(
                matches!(gap(body), Some(EnforcementGap::UnboundedInput { .. })),
                "{source_type}"
            );
        }
    }

    #[test]
    fn a_container_upload_block_is_refused_even_when_nested() {
        let body = serde_json::json!({"messages": [{"role": "user", "content": [
            {"type": "tool_result", "content": [{"type": "container_upload", "file_id": "f"}]}
        ]}]});
        assert!(matches!(
            gap(body),
            Some(EnforcementGap::UnboundedInput { .. })
        ));
    }

    #[test]
    fn a_base64_pdf_is_refused_but_a_base64_image_or_text_document_is_not() {
        let doc = |media_type: &str| {
            serde_json::json!({"messages": [{"role": "user", "content": [
                {"type": "document", "source": {"type": "base64", "media_type": media_type, "data": "AAAA"}}
            ]}]})
        };
        assert!(gap(doc("application/pdf")).is_some());
        assert_eq!(gap(doc("text/plain")), None);
        assert_eq!(gap(doc("image/png")), None);
    }

    #[test]
    fn image_blocks_are_counted_at_any_depth() {
        let body = serde_json::json!({"messages": [{"role": "user", "content": [
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AA"}},
            {"type": "tool_result", "content": [{"type": "image", "source": {}}, {"type": "text", "text": "image"}]}
        ]}]});
        assert_eq!(image_block_count(body.to_string().as_bytes()), 2);
        assert_eq!(image_block_count(b"not json"), 0);
    }

    #[test]
    fn mcp_servers_and_a_container_are_refused() {
        assert!(gap(serde_json::json!({"mcp_servers": [{"name": "m"}]})).is_some());
        assert!(gap(serde_json::json!({"container": "c"})).is_some());
        assert_eq!(gap(serde_json::json!({"mcp_servers": []})), None);
    }

    #[test]
    fn text_that_merely_mentions_a_server_tool_is_not_refused() {
        assert_eq!(
            gap(serde_json::json!({"messages": [{"role": "user",
                "content": "use web_search_20250305 with a url source"}]})),
            None
        );
    }

    #[test]
    fn a_body_that_is_not_json_is_left_to_envelope_parsing() {
        assert_eq!(screen_unbounded_inputs(b"not json"), None);
    }
}
