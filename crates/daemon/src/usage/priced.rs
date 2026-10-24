//! Priced, session-bound transcript reads for the local monthly shared
//! budget (HORO-2196, HORO-2190, ADR-0026).
//!
//! [`crate::usage`]'s token reader answers "how many fresh tokens", which
//! is the *token*-budget rule. The shared budget is denominated in
//! Libra-accounted, list-price-equivalent USD cents, so this module reads
//! the same transcript but keeps what pricing needs: the `model` of every
//! assistant record and **all four** token tiers (input, output,
//! cache-creation, cache-read), and it adds the checks a spend charged to a
//! shared cap needs: the transcript must belong to the session being
//! charged, and a subagent's records must not be charged twice.
//!
//! # Evidence status of the per-line field names (read this)
//!
//! [`TRANSCRIPT_SESSION_FIELD`] and [`TRANSCRIPT_SIDECHAIN_FIELD`] are
//! **UNCONFIRMED against a real Claude Code host**. This repository
//! contains no transcript fixture, characterization document or test that
//! names a per-line session field or a sidechain flag (the Claude Code
//! hook payload's own `session_id` is documented in
//! `docs/native-agent-lifecycle-characterization.md`, but that is the hook
//! payload, not a transcript line). The names below are the ones the
//! public Claude Code JSONL format is understood to use; the synthetic
//! fixtures in this crate's tests *mirror* that understanding, so they
//! prove the logic, not the host. Real-host confirmation is a later step.
//!
//! The failure mode of a wrong name is safe by construction: if real
//! transcripts do not carry [`TRANSCRIPT_SESSION_FIELD`], every window is
//! "unverifiable" and settles at the hold amount with `usage_known=false`
//! (see [`PricedObservation::SessionUnverified`]); nothing is ever charged
//! on an unverified binding. If real transcripts do not carry
//! [`TRANSCRIPT_SIDECHAIN_FIELD`], sidechain records are not excluded from
//! a parent window, which would double-count with the subagent charge --
//! so that name needs the same real-host confirmation before the subagent
//! path is relied on.
//!
//! # Session binding (never inferred)
//!
//! A window is charged only if every usage record in it carries
//! [`TRANSCRIPT_SESSION_FIELD`] equal to the expected session id. Path,
//! working directory and timestamp are never used to guess ownership.
//!
//! - every record present and equal: [`PricedObservation::Priced`];
//! - any record present and **different**:
//!   [`PricedObservation::SessionMismatch`] (the whole window, never
//!   partially);
//! - otherwise any record missing the field or carrying a non-string:
//!   [`PricedObservation::SessionUnverified`].
//!
//! A mismatch outranks a missing field: one foreign record proves the file
//! is not this session's even if others cannot be checked.
//!
//! # What is excluded
//!
//! - Records flagged [`TRANSCRIPT_SIDECHAIN_FIELD`] `true` are subagent
//!   lines inside a parent transcript; they belong to the subagent charge
//!   and are excluded from a parent window. (A subagent's own file is read
//!   with [`ReadMode::WholeFile`], which counts every record.)
//! - Only `type: "assistant"` lines carrying `message.usage` are summed.
//!   A `usage` object nested in a *tool result* (the Agent tool's
//!   `tool_response.usage` / `toolUseResult.usage`) lives on a `user`
//!   line and is never read: it re-reports what the subagent's own
//!   transcript already records.
//! - Repeated content-block lines of one `message.id` count once, taking
//!   the per-field maximum (the HORO-1882 rule, unchanged).
//!
//! # Pricing and rounding
//!
//! Each record is priced at its own model's four-tier rate from the
//! gateway's [`PricingTable`] (plus the shared-budget file's
//! `pricing_overrides`), summed **exactly** in integer pico-dollars, and
//! rounded **up** to a whole cent **once per window**. Rounding per record
//! would add up to a cent per record; rounding once keeps the error to
//! under one cent per Stop window while never rounding spend down. The
//! pinned prices are list-price-equivalent and not exact (see ADR-0026,
//! *Known pricing limitations*).
//!
//! A record with non-zero tokens whose model the table cannot price makes
//! the whole window [`PricedObservation::UnpricedModels`]: nothing is
//! guessed. A record with all-zero usage is free regardless of its model
//! (a host-generated placeholder message), so it neither prices nor
//! flags.
//!
//! # Privacy
//!
//! Same discipline as [`crate::usage`]: parsing goes through a narrow
//! struct, only integers, the model id and the two identity fields are
//! read, and no line, path excerpt or parse message is retained or
//! returned. The session id from a *different* session is compared and
//! dropped, never returned.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::ControlFlow;
use std::path::Path;

use serde::de::IgnoredAny;
use serde::Deserialize;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use libra_governor_gateway::pricing::PricingTable;

use super::{scan_lines_backward, ScanEnd, UsageUnavailable};

/// The per-line field of a Claude Code JSONL transcript that names the
/// session a record belongs to.
///
/// **UNCONFIRMED against a real host** -- see this module's header. No
/// fixture, characterization document or test in this repository
/// evidences it; the value is the name the public transcript format is
/// understood to use. A wrong value fails safe (every window becomes
/// unverifiable and settles at the hold amount).
pub const TRANSCRIPT_SESSION_FIELD: &str = "sessionId";

/// The per-line boolean marking a subagent ("sidechain") record that sits
/// inside a transcript. **UNCONFIRMED against a real host**, same status
/// as [`TRANSCRIPT_SESSION_FIELD`].
pub const TRANSCRIPT_SIDECHAIN_FIELD: &str = "isSidechain";

/// Bytes a budget read may scan: a parent window or a whole subagent file.
/// A subagent transcript past this is unmeasured, not partially charged,
/// because a partial sum would under-charge real spend.
pub const BUDGET_SCAN_CAP_BYTES: u64 = 32 * 1024 * 1024;

/// Substituted for a model id the record does not carry, so the report
/// names the gap instead of hiding it.
const MISSING_MODEL: &str = "<missing>";

/// One cent in pico-dollars (10^-12 USD).
const PICO_PER_CENT: u128 = 10_000_000_000;
/// Prices are dollars per million tokens, i.e. micro-dollars per token;
/// scaled by 10^6 that is pico-dollars per token, exact for any price with
/// up to six decimals.
const PRICE_SCALE: f64 = 1_000_000.0;

/// What a read covers.
#[derive(Debug, Clone, Copy)]
pub enum ReadMode {
    /// A parent transcript: only records strictly after `after` (the
    /// watermark of the last settled window), sidechain records excluded.
    ParentWindow { after: OffsetDateTime },
    /// A subagent's own file: every record, cumulative. The file is the
    /// subagent's, so its records are counted whatever they are flagged.
    WholeFile,
}

/// Token counts by tier for one model, summed over the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tiers {
    pub input: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
    pub output: u64,
}

impl Tiers {
    fn is_zero(&self) -> bool {
        self.input == 0 && self.cache_creation == 0 && self.cache_read == 0 && self.output == 0
    }

    fn add(&mut self, other: Tiers) {
        self.input = self.input.saturating_add(other.input);
        self.cache_creation = self.cache_creation.saturating_add(other.cache_creation);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.output = self.output.saturating_add(other.output);
    }

    fn max(self, other: Tiers) -> Tiers {
        Tiers {
            input: self.input.max(other.input),
            cache_creation: self.cache_creation.max(other.cache_creation),
            cache_read: self.cache_read.max(other.cache_read),
            output: self.output.max(other.output),
        }
    }
}

/// A fully priced window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PricedWindow {
    /// Libra-accounted USD cents, rounded up once for the window.
    pub cents: u64,
    /// Distinct turns that contributed (a content-block-repeated turn
    /// counts once).
    pub records: u64,
    /// The newest record timestamp seen, if any record carried one.
    pub last_event_time: Option<OffsetDateTime>,
}

/// The result of a priced, session-bound read. Each non-`Priced` variant
/// is a reason code; none carries transcript content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PricedObservation {
    Priced(PricedWindow),
    /// At least one record with usage had a model the table cannot price.
    /// Sorted, de-duplicated model ids (raw, unsanitised -- the caller
    /// bounds and sanitises before showing them).
    UnpricedModels(Vec<String>),
    /// A usage record named a different session.
    SessionMismatch,
    /// No record named a different session, but at least one could not be
    /// verified (field absent or not a string).
    SessionUnverified,
    /// Nothing could be measured: no path, unreadable, over the cap, no
    /// usage in the window, or a usage record that cannot be placed in
    /// time ([`UsageUnavailable::Inconsistent`]).
    Unavailable(UsageUnavailable),
}

/// A field that may legitimately be absent, `null`, or of an unexpected
/// type without failing the whole line: losing a line's usage because one
/// identity field had an odd shape would be an undercount.
#[derive(Deserialize)]
#[serde(untagged)]
enum Lenient<T> {
    Value(T),
    Other(IgnoredAny),
}

impl<T> Lenient<T> {
    fn into_value(self) -> Option<T> {
        match self {
            Lenient::Value(v) => Some(v),
            Lenient::Other(_) => None,
        }
    }
}

/// The only fields read. The `rename` strings must equal
/// [`TRANSCRIPT_SESSION_FIELD`] / [`TRANSCRIPT_SIDECHAIN_FIELD`]; serde
/// attributes cannot take a constant, so a unit test pins them together.
#[derive(Deserialize)]
struct PricedLine {
    #[serde(rename = "type")]
    kind: Option<Lenient<String>>,
    timestamp: Option<Lenient<String>>,
    #[serde(rename = "sessionId", default)]
    session: Option<Lenient<String>>,
    #[serde(rename = "isSidechain", default)]
    sidechain: Option<Lenient<bool>>,
    message: Option<PricedMessage>,
}

#[derive(Deserialize)]
struct PricedMessage {
    #[serde(default)]
    id: Option<Lenient<String>>,
    #[serde(default)]
    model: Option<Lenient<String>>,
    usage: Option<PricedUsage>,
}

#[derive(Deserialize)]
struct PricedUsage {
    input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

impl PricedUsage {
    fn tiers(&self) -> Tiers {
        Tiers {
            input: self.input_tokens.unwrap_or(0),
            cache_creation: self.cache_creation_input_tokens.unwrap_or(0),
            cache_read: self.cache_read_input_tokens.unwrap_or(0),
            output: self.output_tokens.unwrap_or(0),
        }
    }
}

/// Everything a scan accumulates.
#[derive(Default)]
struct Accumulator {
    /// Id-keyed turns: per-field maximum of the repeated lines, and the
    /// model they reported.
    by_id: HashMap<String, (Tiers, String)>,
    /// Lines with no `message.id`, summed as they are.
    per_model: BTreeMap<String, Tiers>,
    records: u64,
    id_less_records: u64,
    mismatch: bool,
    unverified: bool,
    untimed: bool,
    last_event_time: Option<OffsetDateTime>,
}

/// Reads `path` and prices what it records for `expected_session`.
///
/// `table` is the pricing table to price against (the pinned table plus
/// the file's overrides). See the module header for every rule.
pub fn observe_priced(
    path: &Path,
    mode: ReadMode,
    expected_session: &str,
    table: &PricingTable,
    cap_bytes: u64,
) -> PricedObservation {
    let mut acc = Accumulator::default();
    let end = scan_lines_backward(path, cap_bytes, |line| {
        visit_line(&mut acc, line, mode, expected_session)
    });
    match end {
        Err(unavailable) => PricedObservation::Unavailable(unavailable),
        Ok(ScanEnd::CapReached) => PricedObservation::Unavailable(UsageUnavailable::ScanCapReached),
        Ok(ScanEnd::Visitor(()) | ScanEnd::StartOfFile) => finish(acc, table),
    }
}

fn visit_line(
    acc: &mut Accumulator,
    line: &[u8],
    mode: ReadMode,
    expected_session: &str,
) -> ControlFlow<()> {
    if line.is_empty() {
        return ControlFlow::Continue(());
    }
    let Ok(record) = serde_json::from_slice::<PricedLine>(line) else {
        // A truncated or non-JSON line (the host appends concurrently) is
        // ignored, never fatal.
        return ControlFlow::Continue(());
    };
    let timestamp = record
        .timestamp
        .and_then(Lenient::into_value)
        .and_then(|ts| OffsetDateTime::parse(&ts, &Rfc3339).ok());
    if let (ReadMode::ParentWindow { after }, Some(ts)) = (mode, timestamp) {
        // Strictly after the watermark: a record stamped exactly at the
        // end of the previous window was already settled with it.
        if ts <= after {
            return ControlFlow::Break(());
        }
    }
    if record.kind.and_then(Lenient::into_value).as_deref() != Some("assistant") {
        return ControlFlow::Continue(());
    }
    let sidechain = record
        .sidechain
        .and_then(Lenient::into_value)
        .unwrap_or(false);
    if sidechain && matches!(mode, ReadMode::ParentWindow { .. }) {
        return ControlFlow::Continue(());
    }
    let Some(message) = record.message else {
        return ControlFlow::Continue(());
    };
    let Some(usage) = message.usage else {
        return ControlFlow::Continue(());
    };

    // Binding is judged on usage records only: they are what would be
    // charged.
    match record.session.and_then(Lenient::into_value) {
        Some(s) if s == expected_session => {}
        Some(_) => acc.mismatch = true,
        None => acc.unverified = true,
    }
    if matches!(mode, ReadMode::ParentWindow { .. }) && timestamp.is_none() {
        // Cannot be placed relative to the watermark, so counting it could
        // double-charge it in a later window.
        acc.untimed = true;
    }
    if let Some(ts) = timestamp {
        acc.last_event_time = Some(acc.last_event_time.map_or(ts, |t| t.max(ts)));
    }

    let tiers = usage.tiers();
    let model = message
        .model
        .and_then(Lenient::into_value)
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| MISSING_MODEL.to_string());
    match message.id.and_then(Lenient::into_value) {
        Some(id) => {
            acc.by_id
                .entry(id)
                .and_modify(|(t, m)| {
                    *t = t.max(tiers);
                    *m = model.clone();
                })
                .or_insert((tiers, model));
        }
        None => {
            acc.per_model.entry(model).or_default().add(tiers);
            acc.id_less_records += 1;
        }
    }
    acc.records += 1;
    ControlFlow::Continue(())
}

fn finish(mut acc: Accumulator, table: &PricingTable) -> PricedObservation {
    if acc.mismatch {
        return PricedObservation::SessionMismatch;
    }
    if acc.unverified {
        return PricedObservation::SessionUnverified;
    }
    if acc.untimed {
        return PricedObservation::Unavailable(UsageUnavailable::Inconsistent);
    }
    let distinct_turns = acc.by_id.len() as u64 + acc.id_less_records;
    for (_, (tiers, model)) in acc.by_id.drain() {
        acc.per_model.entry(model).or_default().add(tiers);
    }
    if distinct_turns == 0 {
        return PricedObservation::Unavailable(UsageUnavailable::NoUsageInWindow);
    }
    match window_cost_cents(&acc.per_model, table) {
        Ok(cents) => PricedObservation::Priced(PricedWindow {
            cents,
            records: distinct_turns,
            last_event_time: acc.last_event_time,
        }),
        Err(models) => PricedObservation::UnpricedModels(models),
    }
}

/// Prices `per_model` and rounds up to whole cents once.
///
/// `Err` carries the sorted models that have non-zero usage and no price.
/// Exposed for tests so the arithmetic can be pinned against the
/// gateway's `settled_cost` without writing a transcript.
pub fn window_cost_cents(
    per_model: &BTreeMap<String, Tiers>,
    table: &PricingTable,
) -> Result<u64, Vec<String>> {
    let mut unpriced = BTreeSet::new();
    let mut pico: u128 = 0;
    for (model, tiers) in per_model {
        if tiers.is_zero() {
            continue;
        }
        let Some(p) = table.lookup(model) else {
            unpriced.insert(model.clone());
            continue;
        };
        let per_token = |usd_per_mtok: f64| (usd_per_mtok * PRICE_SCALE).round() as u128;
        pico = pico.saturating_add(
            u128::from(tiers.input) * per_token(p.input_usd_per_mtok)
                + u128::from(tiers.cache_creation) * per_token(p.cache_creation_input_usd_per_mtok)
                + u128::from(tiers.cache_read) * per_token(p.cache_read_input_usd_per_mtok)
                + u128::from(tiers.output) * per_token(p.output_usd_per_mtok),
        );
    }
    if !unpriced.is_empty() {
        return Err(unpriced.into_iter().collect());
    }
    Ok(u64::try_from(pico.div_ceil(PICO_PER_CENT)).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use libra_governor_domain::{ResourceAmount, ResourceKind};
    use libra_governor_gateway::cost::settled_cost;
    use libra_governor_gateway::usage::ObservedUsage;
    use std::io::Write;

    const SESSION: &str = "sess-A";

    fn epoch_plus(secs: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(secs)
    }

    fn rfc(secs: i64) -> String {
        epoch_plus(secs).format(&Rfc3339).unwrap()
    }

    /// A synthetic assistant line mirroring the documented transcript
    /// shape. `extra` is spliced in as additional top-level fields.
    fn line(secs: i64, id: &str, model: &str, t: [u64; 4], extra: &str) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{}",{extra}"message":{{"id":"{id}","model":"{model}","usage":{{"input_tokens":{},"cache_creation_input_tokens":{},"cache_read_input_tokens":{},"output_tokens":{}}}}}}}"#,
            rfc(secs),
            t[0],
            t[1],
            t[2],
            t[3]
        )
    }

    fn sid(session: &str) -> String {
        format!(r#""{TRANSCRIPT_SESSION_FIELD}":"{session}","#)
    }

    fn side() -> String {
        format!(r#""{TRANSCRIPT_SIDECHAIN_FIELD}":true,"#)
    }

    fn file(lines: &[String]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        f.flush().unwrap();
        f
    }

    fn observe(f: &tempfile::NamedTempFile, after: i64) -> PricedObservation {
        observe_priced(
            f.path(),
            ReadMode::ParentWindow {
                after: epoch_plus(after),
            },
            SESSION,
            &PricingTable::pinned(),
            BUDGET_SCAN_CAP_BYTES,
        )
    }

    fn cents(o: PricedObservation) -> u64 {
        match o {
            PricedObservation::Priced(w) => w.cents,
            other => panic!("expected Priced, got {other:?}"),
        }
    }

    #[test]
    fn field_name_constants_match_the_serde_renames() {
        // serde attributes cannot take a constant, so this is the single
        // place the two spellings are tied together. A line written with
        // the constants must bind and must be treated as a sidechain.
        let f = file(&[line(
            10,
            "m",
            "claude-sonnet-4-5",
            [1, 0, 0, 1],
            &format!("{}{}", sid(SESSION), side()),
        )]);
        // Sidechain-excluded from a parent window => nothing to measure.
        assert_eq!(
            observe(&f, 0),
            PricedObservation::Unavailable(UsageUnavailable::NoUsageInWindow)
        );
        // Counted (and bound) in a whole-file read.
        let whole = observe_priced(
            f.path(),
            ReadMode::WholeFile,
            SESSION,
            &PricingTable::pinned(),
            BUDGET_SCAN_CAP_BYTES,
        );
        assert!(matches!(whole, PricedObservation::Priced(_)));
    }

    #[test]
    fn prices_all_four_tiers_at_the_models_rates() {
        // claude-sonnet-4-5: 3 / 3.75 / 0.30 / 15 USD per Mtok. One million
        // tokens in each tier = 3 + 3.75 + 0.30 + 15 = $22.05 = 2205 cents.
        let f = file(&[line(
            10,
            "m1",
            "claude-sonnet-4-5",
            [1_000_000, 1_000_000, 1_000_000, 1_000_000],
            &sid(SESSION),
        )]);
        assert_eq!(cents(observe(&f, 0)), 2205);
    }

    #[test]
    fn each_tier_is_priced_separately_not_folded_into_fresh_tokens() {
        let one = |t: [u64; 4]| {
            cents(observe(
                &file(&[line(10, "m", "claude-sonnet-4-5", t, &sid(SESSION))]),
                0,
            ))
        };
        assert_eq!(one([1_000_000, 0, 0, 0]), 300);
        assert_eq!(one([0, 1_000_000, 0, 0]), 375);
        // Cache READS are priced (the token-budget rule excludes them; the
        // USD rule must not).
        assert_eq!(one([0, 0, 1_000_000, 0]), 30);
        assert_eq!(one([0, 0, 0, 1_000_000]), 1500);
    }

    #[test]
    fn rounds_up_once_per_window_not_per_record() {
        // 1 input token at $3/Mtok is $0.000003: 1 cent rounded up. Two
        // such records are 0.0006 cents in total, which still rounds to ONE
        // cent for the window; per-record rounding would give two.
        let f = file(&[
            line(10, "a", "claude-sonnet-4-5", [1, 0, 0, 0], &sid(SESSION)),
            line(11, "b", "claude-sonnet-4-5", [1, 0, 0, 0], &sid(SESSION)),
        ]);
        assert_eq!(cents(observe(&f, 0)), 1);
    }

    #[test]
    fn matches_the_gateway_settled_cost_for_a_single_record() {
        let table = PricingTable::pinned();
        for (model, t) in [
            ("claude-sonnet-4-5", [200_000u64, 50_000, 3_000_000, 70_000]),
            (
                "claude-opus-5",
                [1_000_000, 1_000_000, 1_000_000, 1_000_000],
            ),
            ("claude-haiku-4-5", [123_456, 7_890, 1_234_567, 89_012]),
        ] {
            let f = file(&[line(10, "m", model, t, &sid(SESSION))]);
            let ours = cents(observe(&f, 0));
            let theirs = settled_cost(
                ResourceKind::Usd,
                &ObservedUsage {
                    input_tokens: t[0],
                    cache_creation_input_tokens: t[1],
                    cache_read_input_tokens: t[2],
                    output_tokens: t[3],
                },
                model,
                &table,
            )
            .unwrap();
            assert_eq!(
                ResourceAmount::UsdCents(ours as i64),
                theirs,
                "{model} must price exactly like the gateway"
            );
        }
    }

    #[test]
    fn mixed_models_are_each_priced_at_their_own_rate() {
        let f = file(&[
            line(
                10,
                "a",
                "claude-sonnet-4-5",
                [1_000_000, 0, 0, 0],
                &sid(SESSION),
            ),
            line(
                11,
                "b",
                "claude-opus-5",
                [0, 0, 0, 1_000_000],
                &sid(SESSION),
            ),
        ]);
        // $3 + $25 = 2800 cents.
        assert_eq!(cents(observe(&f, 0)), 2800);
    }

    #[test]
    fn an_unpriced_model_makes_the_window_unpriced_and_names_it() {
        let f = file(&[
            line(
                10,
                "a",
                "claude-sonnet-4-5",
                [1_000, 0, 0, 0],
                &sid(SESSION),
            ),
            line(11, "b", "mystery-model-9", [1_000, 0, 0, 0], &sid(SESSION)),
        ]);
        assert_eq!(
            observe(&f, 0),
            PricedObservation::UnpricedModels(vec!["mystery-model-9".to_string()])
        );
    }

    #[test]
    fn a_zero_usage_record_is_free_whatever_its_model() {
        let f = file(&[
            line(10, "a", "<synthetic>", [0, 0, 0, 0], &sid(SESSION)),
            line(
                11,
                "b",
                "claude-sonnet-4-5",
                [1_000_000, 0, 0, 0],
                &sid(SESSION),
            ),
        ]);
        assert_eq!(cents(observe(&f, 0)), 300);
    }

    #[test]
    fn a_record_with_no_model_and_tokens_is_unpriced_not_free() {
        let f = file(&[format!(
            r#"{{"type":"assistant","timestamp":"{}",{}"message":{{"id":"x","usage":{{"input_tokens":5,"output_tokens":5}}}}}}"#,
            rfc(10),
            sid(SESSION)
        )]);
        assert_eq!(
            observe(&f, 0),
            PricedObservation::UnpricedModels(vec![MISSING_MODEL.to_string()])
        );
    }

    #[test]
    fn overrides_price_a_new_model_and_replace_a_pinned_one() {
        use libra_governor_gateway::pricing::ModelPricing;
        let mut overrides = BTreeMap::new();
        let flat = |p: f64| ModelPricing {
            input_usd_per_mtok: p,
            cache_creation_input_usd_per_mtok: p,
            cache_read_input_usd_per_mtok: p,
            output_usd_per_mtok: p,
        };
        overrides.insert("mystery-model-9".to_string(), flat(2.0));
        overrides.insert("claude-sonnet-4-5".to_string(), flat(1.0));
        let table = PricingTable::with_overrides(overrides);
        let f = file(&[
            line(
                10,
                "a",
                "mystery-model-9",
                [1_000_000, 0, 0, 0],
                &sid(SESSION),
            ),
            line(
                11,
                "b",
                "claude-sonnet-4-5",
                [0, 0, 1_000_000, 0],
                &sid(SESSION),
            ),
        ]);
        let o = observe_priced(
            f.path(),
            ReadMode::ParentWindow {
                after: epoch_plus(0),
            },
            SESSION,
            &table,
            BUDGET_SCAN_CAP_BYTES,
        );
        // $2 + $1 (overridden cache-read price) = 300 cents.
        assert_eq!(cents(o), 300);
    }

    #[test]
    fn repeated_content_block_lines_of_one_turn_count_once() {
        let l = line(
            10,
            "same",
            "claude-sonnet-4-5",
            [1_000_000, 0, 0, 0],
            &sid(SESSION),
        );
        let f = file(&[l.clone(), l.clone(), l]);
        assert_eq!(cents(observe(&f, 0)), 300);
    }

    #[test]
    fn a_window_excludes_records_at_or_before_the_watermark() {
        let f = file(&[
            line(
                10,
                "old",
                "claude-sonnet-4-5",
                [1_000_000, 0, 0, 0],
                &sid(SESSION),
            ),
            line(
                20,
                "edge",
                "claude-sonnet-4-5",
                [1_000_000, 0, 0, 0],
                &sid(SESSION),
            ),
            line(
                30,
                "new",
                "claude-sonnet-4-5",
                [1_000_000, 0, 0, 0],
                &sid(SESSION),
            ),
        ]);
        // Strictly after 20: only "new". The record stamped exactly at the
        // watermark was already settled with the previous window.
        assert_eq!(cents(observe(&f, 20)), 300);
    }

    #[test]
    fn sidechain_records_are_excluded_from_a_parent_window() {
        let f = file(&[
            line(
                10,
                "main",
                "claude-sonnet-4-5",
                [1_000_000, 0, 0, 0],
                &sid(SESSION),
            ),
            line(
                11,
                "sub",
                "claude-sonnet-4-5",
                [9_000_000, 0, 0, 0],
                &format!("{}{}", sid(SESSION), side()),
            ),
        ]);
        assert_eq!(cents(observe(&f, 0)), 300);
    }

    #[test]
    fn tool_result_usage_on_user_lines_is_never_summed() {
        let f = file(&[
            line(
                10,
                "main",
                "claude-sonnet-4-5",
                [1_000_000, 0, 0, 0],
                &sid(SESSION),
            ),
            // The Agent tool's result re-reports the subagent's usage.
            format!(
                r#"{{"type":"user","timestamp":"{}",{}"toolUseResult":{{"usage":{{"input_tokens":9000000,"output_tokens":9000000}}}},"message":{{"role":"user","usage":{{"input_tokens":9000000,"output_tokens":9000000}}}}}}"#,
                rfc(11),
                sid(SESSION)
            ),
        ]);
        assert_eq!(cents(observe(&f, 0)), 300);
    }

    #[test]
    fn binding_equal_is_priced() {
        let f = file(&[line(
            10,
            "m",
            "claude-sonnet-4-5",
            [1_000_000, 0, 0, 0],
            &sid(SESSION),
        )]);
        assert!(matches!(observe(&f, 0), PricedObservation::Priced(_)));
    }

    #[test]
    fn binding_different_session_is_a_mismatch_and_charges_nothing() {
        let f = file(&[line(
            10,
            "m",
            "claude-sonnet-4-5",
            [1_000_000, 0, 0, 0],
            &sid("sess-B"),
        )]);
        assert_eq!(observe(&f, 0), PricedObservation::SessionMismatch);
    }

    #[test]
    fn binding_absent_is_unverified_never_guessed() {
        let f = file(&[line(10, "m", "claude-sonnet-4-5", [1_000_000, 0, 0, 0], "")]);
        assert_eq!(observe(&f, 0), PricedObservation::SessionUnverified);
    }

    #[test]
    fn binding_non_string_is_unverified() {
        let f = file(&[line(
            10,
            "m",
            "claude-sonnet-4-5",
            [1_000_000, 0, 0, 0],
            &format!(r#""{TRANSCRIPT_SESSION_FIELD}":12345,"#),
        )]);
        assert_eq!(observe(&f, 0), PricedObservation::SessionUnverified);
    }

    #[test]
    fn a_mixed_window_is_judged_as_a_whole_mismatch_outranks_unverified() {
        let ok = line(
            10,
            "a",
            "claude-sonnet-4-5",
            [1_000_000, 0, 0, 0],
            &sid(SESSION),
        );
        let none = line(11, "b", "claude-sonnet-4-5", [1_000_000, 0, 0, 0], "");
        let other = line(
            12,
            "c",
            "claude-sonnet-4-5",
            [1_000_000, 0, 0, 0],
            &sid("sess-B"),
        );
        assert_eq!(
            observe(&file(&[ok.clone(), none.clone()]), 0),
            PricedObservation::SessionUnverified
        );
        assert_eq!(
            observe(&file(&[ok.clone(), other.clone()]), 0),
            PricedObservation::SessionMismatch
        );
        assert_eq!(
            observe(&file(&[ok, none, other]), 0),
            PricedObservation::SessionMismatch
        );
    }

    #[test]
    fn a_foreign_session_id_on_a_sidechain_line_does_not_poison_the_parent() {
        let f = file(&[
            line(
                10,
                "main",
                "claude-sonnet-4-5",
                [1_000_000, 0, 0, 0],
                &sid(SESSION),
            ),
            line(
                11,
                "sub",
                "claude-sonnet-4-5",
                [1_000_000, 0, 0, 0],
                &format!("{}{}", sid("other"), side()),
            ),
        ]);
        assert_eq!(cents(observe(&f, 0)), 300);
    }

    #[test]
    fn a_usage_record_without_a_timestamp_cannot_be_placed_in_a_window() {
        let f = file(&[format!(
            r#"{{"type":"assistant",{}"message":{{"id":"x","model":"claude-sonnet-4-5","usage":{{"input_tokens":5,"output_tokens":5}}}}}}"#,
            sid(SESSION)
        )]);
        assert_eq!(
            observe(&f, 0),
            PricedObservation::Unavailable(UsageUnavailable::Inconsistent)
        );
    }

    #[test]
    fn a_missing_or_empty_transcript_is_unavailable() {
        let missing = observe_priced(
            Path::new("/nonexistent/definitely/not/here.jsonl"),
            ReadMode::WholeFile,
            SESSION,
            &PricingTable::pinned(),
            BUDGET_SCAN_CAP_BYTES,
        );
        assert_eq!(
            missing,
            PricedObservation::Unavailable(UsageUnavailable::Unreadable)
        );
        assert_eq!(
            observe(&file(&[]), 0),
            PricedObservation::Unavailable(UsageUnavailable::NoUsageInWindow)
        );
    }

    #[test]
    fn a_file_over_the_cap_is_unavailable_not_partially_priced() {
        let f = file(&[
            line(
                10,
                "a",
                "claude-sonnet-4-5",
                [1_000_000, 0, 0, 0],
                &sid(SESSION),
            ),
            line(
                11,
                "b",
                "claude-sonnet-4-5",
                [1_000_000, 0, 0, 0],
                &sid(SESSION),
            ),
        ]);
        let o = observe_priced(
            f.path(),
            ReadMode::WholeFile,
            SESSION,
            &PricingTable::pinned(),
            64,
        );
        assert_eq!(
            o,
            PricedObservation::Unavailable(UsageUnavailable::ScanCapReached)
        );
    }

    #[test]
    fn a_torn_final_line_is_ignored() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            f,
            "{}",
            line(
                10,
                "a",
                "claude-sonnet-4-5",
                [1_000_000, 0, 0, 0],
                &sid(SESSION)
            )
        )
        .unwrap();
        write!(f, r#"{{"type":"assistant","timestamp":"{}","mess"#, rfc(11)).unwrap();
        f.flush().unwrap();
        assert_eq!(cents(observe(&f, 0)), 300);
    }
}
