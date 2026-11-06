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
/// Sources: Anthropic official pricing page
/// <https://platform.claude.com/docs/en/about-claude/pricing> (model ids from
/// <https://platform.claude.com/docs/en/models/overview>), retrieved
/// 2026-10-24; OpenAI official pricing page
/// <https://developers.openai.com/api/docs/pricing>, retrieved 2026-11-05 (the
/// page carries no update date; standard tier, short context). Figures are
/// list prices; USD in this crate means
/// Libra-accounted list-price-equivalent USD at this version, never a
/// provider bill or quota.
pub const PRICING_VERSION: &str = "pricing-2026-11-static-v4";

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
    /// The 1-hour cache-write price. Anthropic charges 2x base input for the
    /// 1-hour tier (and 1.25x for the 5-minute tier this struct stores), so
    /// it is derived rather than stored: existing four-field overrides keep
    /// parsing. A provider whose cache write is not dearer than ordinary
    /// input (OpenAI rows) has no 1-hour tier, so the price stays at input.
    pub fn one_hour_cache_creation_usd_per_mtok(&self) -> f64 {
        if self.cache_creation_input_usd_per_mtok > self.input_usd_per_mtok {
            self.input_usd_per_mtok * 2.0
        } else {
            self.input_usd_per_mtok
        }
    }

    /// The most expensive per-token input price this model can charge.
    ///
    /// Used for the pre-flight reservation: before the request is sent we
    /// cannot know which cache tier its input will land in, so the
    /// reservation must assume the worst one (the 1-hour cache write).
    /// Settlement then corrects downward using the provider's own per-tier
    /// counts.
    pub fn worst_case_input_usd_per_mtok(&self) -> f64 {
        self.input_usd_per_mtok
            .max(self.cache_creation_input_usd_per_mtok)
            .max(self.one_hour_cache_creation_usd_per_mtok())
            .max(self.cache_read_input_usd_per_mtok)
    }
}

/// How a request's cache-write tokens split by TTL, as reported by the
/// provider's `usage.cache_creation` object (`ephemeral_5m_input_tokens`,
/// `ephemeral_1h_input_tokens`). Official docs: the two sum to the top-level
/// `cache_creation_input_tokens`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheWriteSplit {
    /// No breakdown reported. Real Claude Code transcripts always carry one,
    /// so an absent one is unknown and is priced at the dearer 1-hour rate.
    #[default]
    Absent,
    Split {
        five_minute: u64,
        one_hour: u64,
    },
    /// A breakdown count that is not a non-negative integer.
    Malformed,
}

impl CacheWriteSplit {
    /// `(five_minute, one_hour)` tokens for a request that wrote `total`
    /// cache tokens, or `None` when the report is contradictory. Tokens the
    /// breakdown does not account for are priced as 1-hour (never cheaper).
    pub fn resolve(self, total: u64) -> Option<(u64, u64)> {
        match self {
            Self::Absent => Some((0, total)),
            Self::Split {
                five_minute,
                one_hour,
            } => {
                let sum = five_minute.checked_add(one_hour)?;
                let rest = total.checked_sub(sum)?;
                Some((five_minute, one_hour.saturating_add(rest)))
            }
            Self::Malformed => None,
        }
    }
}

/// The `usage.speed` a response reported (`"standard"` or `"fast"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SpeedReport {
    /// Field absent: fast mode is opt-in, so absent means standard. ASSUMED
    /// (real hosts always carry it; a missing one has not been observed
    /// meaning anything else).
    #[default]
    Absent,
    Standard,
    Fast,
    /// Any other value, or a non-string: cannot be priced.
    Unrecognised,
}

impl SpeedReport {
    pub fn from_json(value: Option<&serde_json::Value>) -> Self {
        match value {
            None | Some(serde_json::Value::Null) => Self::Absent,
            Some(serde_json::Value::String(s)) if s == "standard" => Self::Standard,
            Some(serde_json::Value::String(s)) if s == "fast" => Self::Fast,
            Some(_) => Self::Unrecognised,
        }
    }
}

/// The `usage.inference_geo` a response reported (`"global"` or `"us"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GeoReport {
    /// Field absent: treated as global. ASSUMED (a workspace default can make
    /// a request US-only with nothing in the request; real hosts report it).
    #[default]
    Absent,
    Global,
    Us,
    /// Any other value (e.g. `not_available` on a model without the feature).
    Other,
}

impl GeoReport {
    pub fn from_json(value: Option<&serde_json::Value>) -> Self {
        match value {
            None | Some(serde_json::Value::Null) => Self::Absent,
            Some(serde_json::Value::String(s)) if s == "global" => Self::Global,
            Some(serde_json::Value::String(s)) if s == "us" => Self::Us,
            Some(_) => Self::Other,
        }
    }
}

/// Why a request could not be priced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestPriceError {
    /// No entry for the model (see [`PricingTable::lookup`]).
    Unpriced,
    /// The usage report is contradictory or names a modifier this table
    /// cannot price for the model (fast mode on a model without it, US geo
    /// on a pre-4.6 model, an unrecognised value). Nothing is guessed.
    Unmeasurable,
}

/// Models with a fast-mode price (official: Opus 5.5, Opus 5, Opus 4.8). Fast
/// mode is 2x the standard row on all four tiers (the caching multipliers
/// stack on top).
const FAST_MODE_MODELS: &[&str] = &["claude-opus-5-5", "claude-opus-5", "claude-opus-4-8"];
/// Models that accept `inference_geo` (official: Claude 4.6 and later).
/// `"us"` is 1.1x on every tier.
const US_GEO_MODELS: &[&str] = &[
    "claude-fable-5-1",
    "claude-fable-5",
    "claude-opus-5-5",
    "claude-opus-5",
    "claude-opus-4-8",
    "claude-opus-4-7",
    "claude-opus-4-6",
    "claude-sonnet-5-5",
    "claude-sonnet-5",
    "claude-sonnet-4-6",
    "claude-haiku-5-5",
];
/// Prompt length (input + cache writes + cache reads of one request) above
/// which Haiku 5.5 bills its higher row.
pub const LONG_PROMPT_TOKENS: u64 = 100_000;

/// A pinned set of per-model prices, optionally extended or corrected by
/// operator-supplied overrides.
#[derive(Debug, Clone, Default)]
pub struct PricingTable {
    entries: BTreeMap<String, ModelPricing>,
}

/// An OpenAI row: cache creation is priced as ordinary input (OpenAI has no
/// cache-write surcharge), so only input, cached input and output differ.
const fn openai(input: f64, cached_input: f64, output: f64) -> ModelPricing {
    ModelPricing {
        input_usd_per_mtok: input,
        cache_creation_input_usd_per_mtok: input,
        cache_read_input_usd_per_mtok: cached_input,
        output_usd_per_mtok: output,
    }
}

/// The pinned snapshot's per-model entries.
///
/// Prices are US dollars per million tokens, transcribed from Anthropic's
/// published list price as of the date in [`PRICING_VERSION`]. They are a
/// static record, not a live quote — see this module's docs. Model ids
/// are matched exactly or with a dated `-YYYYMMDD` snapshot suffix (see
/// [`PricingTable::lookup`]), so a dated release does not silently fall off the
/// table and a newer sibling id is never priced at an older entry.
const PINNED: &[(&str, ModelPricing)] = &[
    // Current generation (HORO-2192). `cache_creation_input_usd_per_mtok` is
    // the 5-minute cache-write price; the 1-hour tier (2x base input) is
    // derived (`ModelPricing::one_hour_cache_creation_usd_per_mtok`). Exact/dated
    // matching keeps e.g.
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
    // $0.125 cache write / $0.01 cache read / $0.50 output up to 100,000 prompt
    // tokens ([`SHORT_PROMPT_TIERS`]), and 5x that above. This entry is the
    // over-100k row: a reservation cannot know the prompt length, so it takes
    // the dearer row; settlement picks the row per request.
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
    // OpenAI (C9). Source: <https://developers.openai.com/api/docs/pricing>,
    // retrieved 2026-11-05 (no update date on the page). Standard tier, SHORT
    // context only (<= 272K input tokens), USD per 1M tokens. OpenAI bills
    // prompts above 272K input tokens at a higher rate and `ModelPricing` has
    // no tier for that, so a long-context turn is UNDER-counted here.
    // OpenAI has no cache-write surcharge, so cache creation is priced as
    // ordinary input. `gpt-5.6-*` is deliberately NOT pinned: its Sol row has
    // promotional pricing until 2026-11-30, so its list price is not stable;
    // it stays unpriced (flat hold) unless the operator adds an override.
    ("gpt-6.1-sol", openai(2.0, 0.10, 10.0)),
    ("gpt-6.1-terra", openai(1.0, 0.05, 5.0)),
    ("gpt-6-sol", openai(2.0, 0.20, 10.0)),
    ("gpt-6-astra", openai(10.0, 1.0, 50.0)),
    ("gpt-6-luna", openai(0.10, 0.01, 0.50)),
    ("gpt-5.3-codex", openai(1.75, 0.175, 14.0)),
];

/// Rows used instead of the [`PINNED`] entry of the same id when one request's
/// prompt is at most [`LONG_PROMPT_TOKENS`] (official Haiku 5.5 "prompts up
/// to 100,000 tokens" row, retrieved 2026-11-06). Applies only while the
/// entry is the pinned one: an operator override of that id wins outright.
const SHORT_PROMPT_TIERS: &[(&str, ModelPricing)] = &[(
    "claude-haiku-5-5",
    ModelPricing {
        input_usd_per_mtok: 0.10,
        cache_creation_input_usd_per_mtok: 0.125,
        cache_read_input_usd_per_mtok: 0.01,
        output_usd_per_mtok: 0.50,
    },
)];

impl PricingTable {
    /// The model ids the pinned snapshot lists (read-only; `doctor` uses it to
    /// report which Codex models are covered without an override).
    pub fn pinned_model_ids() -> impl Iterator<Item = &'static str> {
        PINNED.iter().map(|(model, _)| *model)
    }

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

    /// Looks up `model`: an exact match, otherwise the longest entry `model`
    /// extends by a dated-snapshot suffix only.
    ///
    /// Provider model ids carry a dated release suffix
    /// (`claude-sonnet-4-5-20260929`) that changes without the price
    /// changing, so exact-match-only would refuse real traffic as
    /// "unpriced". But a *plain* string prefix is not safe (HORO-2366): a
    /// newer sibling such as `claude-opus-5-6` or `gpt-6.1-sol-pro` shares
    /// the prefix of a listed id and is not guaranteed to share its price,
    /// so it used to be silently priced at the older entry. The remainder
    /// after a listed id must therefore be empty or exactly `-` followed by
    /// eight ASCII digits (a `YYYYMMDD` date); anything else is unpriced.
    /// The longest such entry still wins (`claude-opus-4-1` over
    /// `claude-opus-4`), and overrides follow the same rule.
    ///
    /// Returns `None` when nothing matches — the caller must then refuse,
    /// never guess. See [`crate::cost::worst_case_reservation`].
    pub fn lookup(&self, model: &str) -> Option<ModelPricing> {
        self.resolve(model).map(|(_, pricing)| pricing)
    }

    /// [`Self::lookup`] plus the table key the id resolved to.
    fn resolve(&self, model: &str) -> Option<(&str, ModelPricing)> {
        if let Some((key, exact)) = self.entries.get_key_value(model) {
            return Some((key.as_str(), *exact));
        }
        self.entries
            .iter()
            .filter(|(key, _)| {
                model
                    .strip_prefix(key.as_str())
                    .is_some_and(is_dated_snapshot_suffix)
            })
            .max_by_key(|(key, _)| key.len())
            .map(|(key, pricing)| (key.as_str(), *pricing))
    }

    /// The exact cost of one request in pico-dollars (10^-12 USD), from the
    /// provider's reported usage, shared by the gateway's settlement and the
    /// daemon's transcript pricing so the two cannot drift.
    ///
    /// Official list-price rules (retrieved 2026-11-06, see
    /// [`PRICING_VERSION`]): the 1-hour cache write is 2x base input; Haiku
    /// 5.5 bills a lower row when the request's prompt (input, cache writes
    /// and cache reads together) is at most [`LONG_PROMPT_TOKENS`]; fast mode (Opus 5.5,
    /// 5, 4.8 only) is 2x every tier; `inference_geo: "us"` (Claude 4.6 and
    /// later only) is 1.1x every tier; both stack. Integer arithmetic only,
    /// the 1.1x rounded up.
    pub fn request_cost_pico(
        &self,
        model: &str,
        usage: &crate::usage::ObservedUsage,
    ) -> Result<u128, RequestPriceError> {
        let (key, mut price) = self.resolve(model).ok_or(RequestPriceError::Unpriced)?;
        let (write_5m, write_1h) = usage
            .cache_creation_split
            .resolve(usage.cache_creation_input_tokens)
            .ok_or(RequestPriceError::Unmeasurable)?;

        let fast = match usage.speed {
            SpeedReport::Absent | SpeedReport::Standard => false,
            SpeedReport::Fast if FAST_MODE_MODELS.contains(&key) => true,
            SpeedReport::Fast | SpeedReport::Unrecognised => {
                return Err(RequestPriceError::Unmeasurable)
            }
        };
        let us_geo = match usage.inference_geo {
            GeoReport::Absent | GeoReport::Global => false,
            GeoReport::Us if US_GEO_MODELS.contains(&key) => true,
            GeoReport::Us => return Err(RequestPriceError::Unmeasurable),
            // A model without the feature has nothing to report; on one that
            // has it an unrecognised value cannot be priced.
            GeoReport::Other if US_GEO_MODELS.contains(&key) => {
                return Err(RequestPriceError::Unmeasurable)
            }
            GeoReport::Other => false,
        };

        let prompt = usage
            .input_tokens
            .saturating_add(usage.cache_creation_input_tokens)
            .saturating_add(usage.cache_read_input_tokens);
        if prompt <= LONG_PROMPT_TOKENS {
            let pinned_entry = PINNED.iter().any(|(m, p)| *m == key && *p == price);
            if let Some((_, short)) = SHORT_PROMPT_TIERS.iter().find(|(m, _)| *m == key) {
                if pinned_entry {
                    price = *short;
                }
            }
        }

        let per_token = |usd_per_mtok: f64| (usd_per_mtok * PRICE_SCALE).round() as u128;
        let mut pico = u128::from(usage.input_tokens) * per_token(price.input_usd_per_mtok)
            + u128::from(write_5m) * per_token(price.cache_creation_input_usd_per_mtok)
            + u128::from(write_1h) * per_token(price.one_hour_cache_creation_usd_per_mtok())
            + u128::from(usage.cache_read_input_tokens)
                * per_token(price.cache_read_input_usd_per_mtok)
            + u128::from(usage.output_tokens) * per_token(price.output_usd_per_mtok);
        if fast {
            pico = pico.saturating_mul(2);
        }
        if us_geo {
            pico = pico.saturating_mul(11).div_ceil(10);
        }
        Ok(pico)
    }

    /// The factor a pre-flight reservation multiplies its worst case by so
    /// that settlement can never exceed it: 1.1 on a model that accepts
    /// `inference_geo` (a workspace default can make any request US-only),
    /// and 2 more when the request itself asks for fast mode on a model that
    /// has it. 1.0 for an unlisted model (the caller refuses it anyway).
    pub fn worst_case_multiplier(&self, model: &str, fast_requested: bool) -> f64 {
        let Some((key, _)) = self.resolve(model) else {
            return 1.0;
        };
        let mut factor = 1.0;
        if fast_requested && FAST_MODE_MODELS.contains(&key) {
            factor *= 2.0;
        }
        if US_GEO_MODELS.contains(&key) {
            factor *= 1.1;
        }
        factor
    }
}

/// Pico-dollars per whole USD-per-million-tokens unit: prices are dollars
/// per million tokens, i.e. micro-dollars per token; scaled by 10^6 that is
/// pico-dollars per token, exact for any price with up to six decimals.
const PRICE_SCALE: f64 = 1_000_000.0;

/// `-YYYYMMDD`: a dash then exactly eight ASCII digits, nothing else.
fn is_dated_snapshot_suffix(rest: &str) -> bool {
    rest.strip_prefix('-')
        .is_some_and(|d| d.len() == 8 && d.bytes().all(|b| b.is_ascii_digit()))
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
            pricing.one_hour_cache_creation_usd_per_mtok(),
            "the 1-hour cache write is the priciest input tier — a reservation must assume it"
        );
        assert_eq!(pricing.one_hour_cache_creation_usd_per_mtok(), 6.0);
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
        assert_eq!(PRICING_VERSION, "pricing-2026-11-static-v4");
    }

    // ---- HORO-2366: a variant is never priced at the older entry ----

    #[test]
    fn a_newer_variant_sharing_a_listed_prefix_is_unpriced_not_priced_at_the_older_entry() {
        let table = PricingTable::pinned();
        // `claude-opus-5` is a listed id and a string prefix of both.
        assert_eq!(table.lookup("claude-opus-5-6"), None);
        assert_eq!(table.lookup("claude-opus-5-5-pro"), None);
        assert_eq!(table.lookup("claude-sonnet-4-5-latest"), None);
        // Nor does a date-shaped but wrong-length suffix count.
        assert_eq!(table.lookup("claude-opus-5-5-2026"), None);
        assert_eq!(table.lookup("claude-opus-5-5-202610245"), None);
        assert_eq!(table.lookup("claude-opus-5-5-2026102x"), None);
    }

    #[test]
    fn a_dated_snapshot_still_resolves_to_the_longest_exact_entry() {
        let table = PricingTable::pinned();
        let dated = table.lookup("claude-opus-5-5-20261024").unwrap();
        assert_eq!(dated.input_usd_per_mtok, 4.0);
        assert_eq!(dated, table.lookup("claude-opus-5-5").unwrap());
        assert_eq!(
            table
                .lookup("claude-opus-4-1-20260805")
                .unwrap()
                .output_usd_per_mtok,
            75.0
        );
    }

    #[test]
    fn an_override_matches_exactly_or_by_dated_snapshot_only() {
        let price = ModelPricing {
            input_usd_per_mtok: 9.0,
            cache_creation_input_usd_per_mtok: 9.0,
            cache_read_input_usd_per_mtok: 0.9,
            output_usd_per_mtok: 90.0,
        };
        let table =
            PricingTable::with_overrides(BTreeMap::from([("custom-model".to_string(), price)]));
        assert_eq!(table.lookup("custom-model"), Some(price));
        assert_eq!(table.lookup("custom-model-20261101"), Some(price));
        assert_eq!(table.lookup("custom-model-mini"), None);
    }

    // ---- C9: OpenAI list prices ----

    #[test]
    fn openai_models_are_priced_at_the_cited_short_context_list_prices() {
        // (model id, input, output, cached input) USD per 1M tokens, standard
        // tier, <=272K input tokens.
        let expected = [
            ("gpt-6.1-sol", 2.00, 10.00, 0.10),
            ("gpt-6.1-terra", 1.00, 5.00, 0.05),
            ("gpt-6-sol", 2.00, 10.00, 0.20),
            ("gpt-6-astra", 10.00, 50.00, 1.00),
            ("gpt-6-luna", 0.10, 0.50, 0.01),
            ("gpt-5.3-codex", 1.75, 14.00, 0.175),
        ];
        let table = PricingTable::pinned();
        for (model, input, output, cached) in expected {
            for id in [model.to_string(), format!("{model}-20261024")] {
                let p = table.lookup(&id).unwrap_or_else(|| panic!("{id} unpriced"));
                assert_eq!(
                    (
                        p.input_usd_per_mtok,
                        p.cache_creation_input_usd_per_mtok,
                        p.cache_read_input_usd_per_mtok,
                        p.output_usd_per_mtok
                    ),
                    // OpenAI has no cache-write surcharge: cache creation is input.
                    (input, input, cached, output),
                    "{id}"
                );
            }
        }
    }

    #[test]
    fn openai_variants_and_promotional_models_stay_unpriced() {
        let table = PricingTable::pinned();
        assert_eq!(table.lookup("gpt-6.1-sol-pro"), None);
        assert_eq!(table.lookup("gpt-6-sol-mini"), None);
        // gpt-5.6-* has promotional list pricing until 2026-11-30: not pinned.
        assert_eq!(table.lookup("gpt-5.6-sol"), None);
        assert_eq!(table.lookup("gpt-5.6-codex"), None);
        assert_ne!(
            table.lookup("gpt-6-sol").unwrap(),
            table.lookup("gpt-6.1-sol").unwrap()
        );
    }

    #[test]
    fn an_override_beats_a_pinned_openai_price() {
        let price = ModelPricing {
            input_usd_per_mtok: 3.0,
            cache_creation_input_usd_per_mtok: 3.0,
            cache_read_input_usd_per_mtok: 0.3,
            output_usd_per_mtok: 12.0,
        };
        let table =
            PricingTable::with_overrides(BTreeMap::from([("gpt-6.1-sol".to_string(), price)]));
        assert_eq!(table.lookup("gpt-6.1-sol"), Some(price));
        assert_eq!(table.lookup("gpt-6.1-sol-20261024"), Some(price));
        assert_eq!(table.override_count(), 1);
    }

    #[test]
    fn pinned_model_ids_lists_every_pin() {
        let ids: Vec<&str> = PricingTable::pinned_model_ids().collect();
        assert!(ids.contains(&"gpt-6.1-sol") && ids.contains(&"claude-opus-5-5"));
        assert_eq!(ids.len(), PINNED.len());
    }

    #[test]
    fn unpriced_models_stay_unpriced_after_the_refresh() {
        let table = PricingTable::pinned();
        assert_eq!(table.lookup("claude-unreleased-9"), None);
        assert_eq!(table.lookup("claude-mythos-5-1"), None);
    }

    // ---- HORO-2443: tiers and modifiers ----

    #[test]
    fn one_hour_write_is_twice_input_for_every_anthropic_row_and_input_for_openai() {
        for (model, pricing) in PINNED {
            let expected = if model.starts_with("claude-") {
                pricing.input_usd_per_mtok * 2.0
            } else {
                pricing.input_usd_per_mtok
            };
            assert_eq!(
                pricing.one_hour_cache_creation_usd_per_mtok(),
                expected,
                "{model}"
            );
        }
    }

    #[test]
    fn an_override_of_the_haiku_id_replaces_both_prompt_length_rows() {
        let price = ModelPricing {
            input_usd_per_mtok: 1.0,
            cache_creation_input_usd_per_mtok: 1.25,
            cache_read_input_usd_per_mtok: 0.1,
            output_usd_per_mtok: 5.0,
        };
        let table =
            PricingTable::with_overrides(BTreeMap::from([("claude-haiku-5-5".to_string(), price)]));
        let usage = crate::usage::ObservedUsage {
            input_tokens: 1_000_000,
            cache_creation_split: CacheWriteSplit::Split {
                five_minute: 0,
                one_hour: 0,
            },
            ..Default::default()
        };
        // Short prompt, but the override has no lower row: $1.00 = 1e12 pico.
        assert_eq!(
            table.request_cost_pico("claude-haiku-5-5", &usage),
            Ok(1_000_000_000_000 / 10 * 10)
        );
    }

    #[test]
    fn the_cache_write_split_never_prices_unknown_tokens_cheaper() {
        let split = |five_minute, one_hour| CacheWriteSplit::Split {
            five_minute,
            one_hour,
        };
        assert_eq!(CacheWriteSplit::Absent.resolve(10), Some((0, 10)));
        assert_eq!(split(4, 6).resolve(10), Some((4, 6)));
        assert_eq!(split(4, 1).resolve(10), Some((4, 6)));
        assert_eq!(split(8, 8).resolve(10), None);
        assert_eq!(CacheWriteSplit::Malformed.resolve(0), None);
    }

    #[test]
    fn worst_case_multiplier_covers_geo_and_requested_fast_mode() {
        let table = PricingTable::pinned();
        assert_eq!(table.worst_case_multiplier("claude-sonnet-4-5", true), 1.0);
        assert_eq!(table.worst_case_multiplier("claude-sonnet-4-6", true), 1.1);
        assert_eq!(table.worst_case_multiplier("claude-opus-5", false), 1.1);
        assert_eq!(table.worst_case_multiplier("claude-opus-5", true), 2.2);
        assert_eq!(table.worst_case_multiplier("no-such-model", true), 1.0);
    }
}
