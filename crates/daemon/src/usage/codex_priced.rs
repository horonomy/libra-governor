//! Pricing one measured Codex turn for the local monthly shared budget
//! (HORO-2282, C2).
//!
//! [`super::observe_codex_turn`] already returns the four disjoint tiers: fresh
//! input, cache-creation (Codex's cache-write), cache-read and output. Codex's
//! `output_tokens` **already includes** `reasoning_output_tokens`, so reasoning
//! is never priced separately. The sum is exact in pico-dollars and rounded up
//! once per turn, by the same function the Claude windows use.

use std::collections::BTreeMap;

use libra_governor_gateway::pricing::PricingTable;

use super::priced::{window_cost_cents, Tiers};
use super::TurnUsage;

/// Libra-accounted USD cents for one Codex turn, rounded up once, or `None`
/// when `model` is not in `table` (the pinned table plus the operator's
/// overrides).
///
/// The model is looked up first on purpose: an all-zero usage would otherwise
/// price as `Some(0)` for any model, and an unpriced turn must never read as a
/// free one.
pub fn price_codex_turn(usage: &TurnUsage, model: &str, table: &PricingTable) -> Option<u64> {
    table.lookup(model)?;
    let tiers = Tiers {
        input: usage.input_tokens,
        cache_creation: usage.cache_creation_input_tokens,
        cache_read: usage.cache_read_input_tokens,
        output: usage.output_tokens,
    };
    window_cost_cents(&BTreeMap::from([(model.to_string(), tiers)]), table).ok()
}
