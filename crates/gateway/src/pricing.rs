//! [`PricingTable`] — a pinned, versioned snapshot of per-model token
//! prices (HORO-1144).
//!
//! # Why a pinned snapshot and not a live feed
//!
//! A live pricing feed would put a network dependency on the enforcement
//! path: a request could not be admitted while the price server was slow
//! or down, and the number used to refuse a call would not be
//! reproducible afterwards. A pinned table is the honest claim — every
//! reservation, settlement, and `gateway_requests` row records
//! [`PRICING_VERSION`], so any spend figure can be traced back to exactly
//! the prices that produced it, and a stale table is *visible* rather
//! than silently wrong.
//!
//! The cost of that choice is equally explicit: when a provider changes
//! its prices, this table is wrong until someone updates it and bumps
//! [`PRICING_VERSION`]. [`PricingTable::with_overrides`] exists so an
//! operator can correct or extend it without waiting for a release.
//!
//! # What happens to an unpriced model
//!
//! Nothing is guessed. A model with no entry here and no override cannot
//! be priced, and a request for it against a
//! [`libra_governor_domain::ResourceKind::Usd`] budget is refused
//! (`403 unpriced_model`) rather than admitted at a made-up price. A
//! token-denominated budget does not need prices at all and is unaffected
//! — see [`crate::cost`].

use std::collections::BTreeMap;

/// Identifies this pinned price snapshot. Recorded on every reservation,
/// settlement, and provenance row. Bump whenever any number in
/// [`PricingTable::pinned`] changes.
///
/// Source: Anthropic official pricing page
/// <https://platform.claude.com/docs/en/about-claude/pricing> (model ids from
/// <https://platform.claude.com/docs/en/models/overview>), retrieved
/// 2026-10-24. Figures are list prices; USD in this crate means
/// Libra-accounted list-price-equivalent USD at this version, never a
/// provider bill or quota.
pub const PRICING_VERSION: &str = "pricing-2026-10-static-v2";

/// Per-million-token USD prices for one model.
///
/// Four tiers, not two: Anthropic bills cache-creation input above
/// ordinary input and cache-read input far below it, and a reservation
/// that assumed a single input price would be wrong in whichever
/// direction matters. Stored as `f64` dollars per million tokens, which
/// is how the provider publishes them; conversion into the integer cents
/// [`libra_governor_domain::ResourceAmount::UsdCents`] stores happens
/// once, at the end of the calculation in [`crate::cost`], so rounding
/// error cannot compound across tiers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPricing {
    pub input_usd_per_mtok: f64,
    pub cache_creation_input_usd_per_mtok: f64,
    pub cache_read_input_usd_per_mtok: f64,
    pub output_usd_per_mtok: f64,
}

impl ModelPricing {
    /// The most expensive per-token input price this model can charge.
    ///
    /// Used for the pre-flight reservation: before the request is sent we
    /// cannot know which cache tier its input will land in, so the
    /// reservation must assume the worst one. Settlement then corrects
    /// downward using the provider's own per-tier counts.
    pub fn worst_case_input_usd_per_mtok(&self) -> f64 {
        self.input_usd_per_mtok
            .max(self.cache_creation_input_usd_per_mtok)
            .max(self.cache_read_input_usd_per_mtok)
    }
}

/// A pinned set of per-model prices, optionally extended or corrected by
/// operator-supplied overrides.
#[derive(Debug, Clone, Default)]
pub struct PricingTable {
    entries: BTreeMap<String, ModelPricing>,
}

/// The pinned snapshot's per-model entries.
///
/// Prices are US dollars per million tokens, transcribed from Anthropic's
/// published list price as of the date in [`PRICING_VERSION`]. They are a
/// static record, not a live quote — see this module's docs. Model ids
/// are matched by *prefix* (see [`PricingTable::lookup`]) so a dated
/// release suffix does not silently fall off the table.
const PINNED: &[(&str, ModelPricing)] = &[
    // Current generation (HORO-2192). `cache_creation_input_usd_per_mtok` is
    // the 5-minute cache-write price; the 1-hour tier (2x base input) is not
    // modelled by `ModelPricing`. Longest-prefix matching keeps e.g.
    // `claude-opus-5-5` from falling to `claude-opus-5`.
    // Fable 5.1 and Opus/Sonnet 5.5 have a discounted cache-read multiplier
    // (0.025x and 0.05x of base input respectively) per the pricing page.
    (
        "claude-fable-5-1",
        ModelPricing {
            input_usd_per_mtok: 10.0,
            cache_creation_input_usd_per_mtok: 12.5,
            cache_read_input_usd_per_mtok: 0.25,
            output_usd_per_mtok: 50.0,
        },
    ),
    (
        "claude-fable-5",
        ModelPricing {
            input_usd_per_mtok: 10.0,
            cache_creation_input_usd_per_mtok: 12.5,
            cache_read_input_usd_per_mtok: 1.0,
            output_usd_per_mtok: 50.0,
        },
    ),
    (
        "claude-opus-5-5",
        ModelPricing {
            input_usd_per_mtok: 4.0,
            cache_creation_input_usd_per_mtok: 5.0,
            cache_read_input_usd_per_mtok: 0.2,
            output_usd_per_mtok: 20.0,
        },
    ),
    (
        "claude-opus-5",
        ModelPricing {
            input_usd_per_mtok: 5.0,
            cache_creation_input_usd_per_mtok: 6.25,
            cache_read_input_usd_per_mtok: 0.5,
            output_usd_per_mtok: 25.0,
        },
    ),
    (
        "claude-opus-4-8",
        ModelPricing {
            input_usd_per_mtok: 5.0,
            cache_creation_input_usd_per_mtok: 6.25,
            cache_read_input_usd_per_mtok: 0.5,
            output_usd_per_mtok: 25.0,
        },
    ),
    (
        "claude-opus-4-7",
        ModelPricing {
            input_usd_per_mtok: 5.0,
            cache_creation_input_usd_per_mtok: 6.25,
            cache_read_input_usd_per_mtok: 0.5,
            output_usd_per_mtok: 25.0,
        },
    ),
    (
        "claude-opus-4-6",
        ModelPricing {
            input_usd_per_mtok: 5.0,
            cache_creation_input_usd_per_mtok: 6.25,
            cache_read_input_usd_per_mtok: 0.5,
            output_usd_per_mtok: 25.0,
        },
    ),
    (
        "claude-opus-4-5",
        ModelPricing {
            input_usd_per_mtok: 5.0,
            cache_creation_input_usd_per_mtok: 6.25,
            cache_read_input_usd_per_mtok: 0.5,
            output_usd_per_mtok: 25.0,
        },
    ),
    (
        "claude-sonnet-5-5",
        ModelPricing {
            input_usd_per_mtok: 2.0,
            cache_creation_input_usd_per_mtok: 2.5,
            cache_read_input_usd_per_mtok: 0.1,
            output_usd_per_mtok: 10.0,
        },
    ),
    (
        "claude-sonnet-5",
        ModelPricing {
            input_usd_per_mtok: 2.0,
            cache_creation_input_usd_per_mtok: 2.5,
            cache_read_input_usd_per_mtok: 0.2,
            output_usd_per_mtok: 10.0,
        },
    ),
    (
        "claude-sonnet-4-6",
        ModelPricing {
            input_usd_per_mtok: 3.0,
            cache_creation_input_usd_per_mtok: 3.75,
            cache_read_input_usd_per_mtok: 0.3,
            output_usd_per_mtok: 15.0,
        },
    ),
    // Haiku 5.5 is priced by prompt length on the official page: $0.10 input /
    // $0.125 cache write / $0.01 cache read / $0.50 output up to 100k prompt
    // tokens, and 5x that above 100k. `ModelPricing` has one price per tier, so
    // the over-100k prices are pinned: reservations stay conservative and
    // short-prompt settlements over-account rather than under-account.
    (
        "claude-haiku-5-5",
        ModelPricing {
            input_usd_per_mtok: 0.5,
            cache_creation_input_usd_per_mtok: 0.625,
            cache_read_input_usd_per_mtok: 0.05,
            output_usd_per_mtok: 2.5,
        },
    ),
    (
        "claude-opus-4-1",
        ModelPricing {
            input_usd_per_mtok: 15.0,
            cache_creation_input_usd_per_mtok: 18.75,
            cache_read_input_usd_per_mtok: 1.50,
            output_usd_per_mtok: 75.0,
        },
    ),
    (
        "claude-opus-4",
        ModelPricing {
            input_usd_per_mtok: 15.0,
            cache_creation_input_usd_per_mtok: 18.75,
            cache_read_input_usd_per_mtok: 1.50,
            output_usd_per_mtok: 75.0,
        },
    ),
    (
        "claude-sonnet-4-5",
        ModelPricing {
            input_usd_per_mtok: 3.0,
            cache_creation_input_usd_per_mtok: 3.75,
            cache_read_input_usd_per_mtok: 0.30,
            output_usd_per_mtok: 15.0,
        },
    ),
    (
        "claude-sonnet-4",
        ModelPricing {
            input_usd_per_mtok: 3.0,
            cache_creation_input_usd_per_mtok: 3.75,
            cache_read_input_usd_per_mtok: 0.30,
            output_usd_per_mtok: 15.0,
        },
    ),
    (
        "claude-haiku-4-5",
        ModelPricing {
            input_usd_per_mtok: 1.0,
            cache_creation_input_usd_per_mtok: 1.25,
            cache_read_input_usd_per_mtok: 0.10,
            output_usd_per_mtok: 5.0,
        },
    ),
    (
        "claude-3-5-haiku",
        ModelPricing {
            input_usd_per_mtok: 0.80,
            cache_creation_input_usd_per_mtok: 1.0,
            cache_read_input_usd_per_mtok: 0.08,
            output_usd_per_mtok: 4.0,
        },
    ),
];

impl PricingTable {
    /// The pinned snapshot, with no operator overrides applied.
    pub fn pinned() -> Self {
        Self {
            entries: PINNED
                .iter()
                .map(|(model, pricing)| ((*model).to_string(), *pricing))
                .collect(),
        }
    }

    /// The pinned snapshot with `overrides` applied on top: an entry with
    /// the same key replaces the pinned one, a new key extends the table.
    ///
    /// This is the escape hatch for a price change (or a model) that
    /// landed after this build. It deliberately cannot *remove* the
    /// version tag — an overridden table still reports
    /// [`PRICING_VERSION`] alongside the fact that overrides were applied
    /// (see [`Self::override_count`]), so "why was I charged that" stays
    /// answerable.
    pub fn with_overrides(overrides: BTreeMap<String, ModelPricing>) -> Self {
        let mut table = Self::pinned();
        table.entries.extend(overrides);
        table
    }

    /// How many entries differ from (or are absent in) the pinned
    /// snapshot. Recorded alongside [`PRICING_VERSION`] rather than
    /// silently folded into it.
    pub fn override_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|(model, pricing)| !PINNED.iter().any(|(m, p)| m == model && p == *pricing))
            .count()
    }

    /// Looks up `model`, preferring an exact match and otherwise falling
    /// back to the longest entry that is a prefix of it.
    ///
    /// Provider model ids carry a dated release suffix
    /// (`claude-sonnet-4-5-20260929`) that changes without the price
    /// changing. Exact-match-only would drop every such id off the table
    /// and refuse real traffic as "unpriced"; longest-prefix keeps the
    /// entry matched while still letting a more specific entry
    /// (`claude-opus-4-1` over `claude-opus-4`) win.
    ///
    /// Returns `None` when nothing matches — the caller must then refuse,
    /// never guess. See [`crate::cost::worst_case_reservation`].
    pub fn lookup(&self, model: &str) -> Option<ModelPricing> {
        if let Some(exact) = self.entries.get(model) {
            return Some(*exact);
        }
        self.entries
            .iter()
            .filter(|(key, _)| model.starts_with(key.as_str()))
            .max_by_key(|(key, _)| key.len())
            .map(|(_, pricing)| *pricing)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_table_prices_a_dated_model_id_by_prefix() {
        let table = PricingTable::pinned();
        let pricing = table
            .lookup("claude-sonnet-4-5-20260929")
            .expect("a dated release suffix must not drop a model off the table");
        assert_eq!(pricing.input_usd_per_mtok, 3.0);
        assert_eq!(pricing.output_usd_per_mtok, 15.0);
    }

    #[test]
    fn longest_prefix_wins_over_a_shorter_one() {
        let table = PricingTable::pinned();
        let opus_41 = table.lookup("claude-opus-4-1-20260805").unwrap();
        let sonnet_45 = table.lookup("claude-sonnet-4-5-20260929").unwrap();
        // `claude-opus-4` is also a prefix of the 4-1 id; the more
        // specific entry must be the one that matches.
        assert_eq!(opus_41.output_usd_per_mtok, 75.0);
        assert_eq!(sonnet_45.output_usd_per_mtok, 15.0);
    }

    #[test]
    fn an_unknown_model_is_unpriced_rather_than_guessed() {
        let table = PricingTable::pinned();
        assert_eq!(table.lookup("some-other-vendor-model-v9"), None);
    }

    #[test]
    fn overrides_extend_and_replace_the_pinned_table() {
        let mut overrides = BTreeMap::new();
        overrides.insert(
            "brand-new-model".to_string(),
            ModelPricing {
                input_usd_per_mtok: 2.0,
                cache_creation_input_usd_per_mtok: 2.5,
                cache_read_input_usd_per_mtok: 0.2,
                output_usd_per_mtok: 10.0,
            },
        );
        overrides.insert(
            "claude-sonnet-4-5".to_string(),
            ModelPricing {
                input_usd_per_mtok: 4.0,
                cache_creation_input_usd_per_mtok: 5.0,
                cache_read_input_usd_per_mtok: 0.4,
                output_usd_per_mtok: 20.0,
            },
        );
        let table = PricingTable::with_overrides(overrides);

        assert_eq!(
            table.lookup("brand-new-model").unwrap().output_usd_per_mtok,
            10.0
        );
        assert_eq!(
            table
                .lookup("claude-sonnet-4-5")
                .unwrap()
                .input_usd_per_mtok,
            4.0,
            "an override must replace the pinned price, not be shadowed by it"
        );
        assert_eq!(table.override_count(), 2);
    }

    #[test]
    fn pinned_table_reports_no_overrides() {
        assert_eq!(PricingTable::pinned().override_count(), 0);
    }

    #[test]
    fn worst_case_input_price_is_the_most_expensive_tier() {
        let pricing = PricingTable::pinned().lookup("claude-sonnet-4-5").unwrap();
        assert_eq!(
            pricing.worst_case_input_usd_per_mtok(),
            pricing.cache_creation_input_usd_per_mtok,
            "cache creation is the priciest input tier — a reservation must assume it"
        );
    }

    #[test]
    fn every_pinned_entry_has_strictly_positive_finite_prices() {
        for (model, pricing) in PINNED {
            for price in [
                pricing.input_usd_per_mtok,
                pricing.cache_creation_input_usd_per_mtok,
                pricing.cache_read_input_usd_per_mtok,
                pricing.output_usd_per_mtok,
            ] {
                assert!(
                    price.is_finite() && price > 0.0,
                    "{model} carries a non-finite or non-positive price, which would \
                     produce a NaN or zero reservation"
                );
            }
        }
    }

    #[test]
    fn current_generation_models_are_priced_at_official_list_prices() {
        // (model id, input, 5m cache write, cache read, output)
        let expected = [
            ("claude-fable-5-1", 10.0, 12.5, 0.25, 50.0),
            ("claude-fable-5", 10.0, 12.5, 1.0, 50.0),
            ("claude-opus-5-5", 4.0, 5.0, 0.20, 20.0),
            ("claude-opus-5", 5.0, 6.25, 0.50, 25.0),
            ("claude-opus-4-8", 5.0, 6.25, 0.50, 25.0),
            ("claude-opus-4-7", 5.0, 6.25, 0.50, 25.0),
            ("claude-opus-4-6", 5.0, 6.25, 0.50, 25.0),
            ("claude-opus-4-5", 5.0, 6.25, 0.50, 25.0),
            ("claude-sonnet-5-5", 2.0, 2.5, 0.10, 10.0),
            ("claude-sonnet-5", 2.0, 2.5, 0.20, 10.0),
            ("claude-sonnet-4-6", 3.0, 3.75, 0.30, 15.0),
            ("claude-haiku-5-5", 0.50, 0.625, 0.05, 2.50),
        ];
        let table = PricingTable::pinned();
        for (model, input, write, read, output) in expected {
            for id in [model.to_string(), format!("{model}-20261024")] {
                let p = table.lookup(&id).unwrap_or_else(|| panic!("{id} unpriced"));
                assert_eq!(
                    (
                        p.input_usd_per_mtok,
                        p.cache_creation_input_usd_per_mtok,
                        p.cache_read_input_usd_per_mtok,
                        p.output_usd_per_mtok
                    ),
                    (input, write, read, output),
                    "{id}"
                );
            }
        }
    }

    #[test]
    fn point_release_prefixes_do_not_fall_to_the_older_generation_entry() {
        let table = PricingTable::pinned();
        // `claude-opus-4` ($15/$75) is a prefix of `claude-opus-4-5`.
        assert_eq!(
            table.lookup("claude-opus-4-5").unwrap().output_usd_per_mtok,
            25.0
        );
        // `claude-sonnet-5` / `claude-opus-5` are prefixes of the 5.5 ids.
        assert_eq!(
            table
                .lookup("claude-sonnet-5-5")
                .unwrap()
                .cache_read_input_usd_per_mtok,
            0.10
        );
        assert_eq!(
            table.lookup("claude-opus-5-5").unwrap().input_usd_per_mtok,
            4.0
        );
        assert_eq!(
            table
                .lookup("claude-fable-5-1")
                .unwrap()
                .cache_read_input_usd_per_mtok,
            0.25
        );
    }

    #[test]
    fn pricing_version_names_this_snapshot() {
        assert_eq!(PRICING_VERSION, "pricing-2026-10-static-v2");
    }

    #[test]
    fn unpriced_models_stay_unpriced_after_the_refresh() {
        let table = PricingTable::pinned();
        assert_eq!(table.lookup("claude-unreleased-9"), None);
        assert_eq!(table.lookup("claude-mythos-5-1"), None);
    }
}
