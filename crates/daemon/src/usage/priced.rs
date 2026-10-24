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
//! # Evidence status of the per-line field names
//!
//! [`TRANSCRIPT_SESSION_FIELD`] and [`TRANSCRIPT_SIDECHAIN_FIELD`] are
//! **CONFIRMED on a real Claude Code 2.1.306 host (2026-10-24)**: a real
//! binary with an isolated HOME, a fake loopback upstream and a
//! network-deny sandbox, synthetic prompts only. Observed:
//!
//! - every main-transcript conversation line carries `sessionId` equal to
//!   the hook payload's `session_id`, and `isSidechain: false`;
//! - a subagent's lines are NOT inline: they live in a separate file
//!   `<session-dir>/subagents/agent-<agent_id>.jsonl` (session-dir = the
//!   hook's `transcript_path` without `.jsonl`), carry `isSidechain: true`,
//!   an `agentId`, and the **parent's** `sessionId`; `SubagentStop` carries
//!   `agent_id`, `agent_type` and `agent_transcript_path`, with `session_id`
//!   equal to the parent's; each subagent file holds only its own agent's
//!   lines (not cumulative across agents);
//! - `--resume` keeps the same `sessionId` and appends to the same file;
//! - one API message with several content blocks is written as several
//!   `assistant` lines with the same `message.id`, identical
//!   `message.usage` and `apiBlockIndex` 0, 1, ...;
//! - timestamps are ISO-8601 millisecond `Z` but not strictly monotonic in
//!   file order, and some lines (`atis-latch`, `last-prompt`, `cost-state`,
//!   `mode`) have no timestamp at all;
//! - flush lag is real and systematic: at `Stop` the turn's final
//!   `end_turn` assistant line is often not yet in the file, and at
//!   `SubagentStop` a subagent's file may have no assistant line yet.
//!
//! Still unverified on a real host: the post-`/compact` transcript. The
//! fixtures in this crate reproduce the *shape* above with synthetic ids.
//! A mismatch of these names would fail safe for the session field (every
//! window unverifiable) but not for the sidechain flag, so a host upgrade
//! that renames either must be re-checked.
//!
//! # What "fails safe" does and does not mean
//!
//! Every window that cannot be measured (unverified session, unpriced
//! model, malformed counts, scan cap, mismatch) settles at the flat hold
//! amount. That is conservative **only if the hold is at least the real
//! cost of the turn**. An expensive turn settled at a small hold is
//! under-counted: the cap is an estimate, not a guarantee, and a larger
//! `turn_hold_usd_cents` is the only lever.
//!
//! # Watermark, ordering and flush lag
//!
//! The host writes the transcript asynchronously, so records stamped before
//! a Stop are routinely flushed after it. The window therefore starts after
//! the newest record timestamp **already counted** (persisted on the
//! settled hold), not after the wall-clock time of the Stop. Residuals: a
//! record stamped at or before an already-counted record but flushed later
//! is not recovered; a `message.id` whose content blocks straddle the
//! watermark may have its tail blocks counted again by the next window (an
//! over-count, the safe direction); and the last turn of a session is never
//! recovered after its final Stop.
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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ops::ControlFlow;
use std::path::Path;

use serde::de::IgnoredAny;
use serde::Deserialize;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use libra_governor_gateway::pricing::PricingTable;

use super::{scan_lines_backward, ScanEnd, UsageUnavailable};

/// The per-line field of a Claude Code JSONL transcript that names the
/// session a record belongs to. **Confirmed on Claude Code 2.1.306,
/// 2026-10-24** (see this module's header for what was observed).
pub const TRANSCRIPT_SESSION_FIELD: &str = "sessionId";

/// The per-line boolean marking a subagent ("sidechain") record. **Confirmed
/// on Claude Code 2.1.306, 2026-10-24**: subagent lines carry `true` and live
/// in a separate `subagents/agent-<id>.jsonl` file; main-transcript lines
/// carry `false`.
pub const TRANSCRIPT_SIDECHAIN_FIELD: &str = "isSidechain";

/// Bytes a budget read may scan: a parent window or a whole subagent file.
/// A subagent transcript past this is unmeasured, not partially charged,
/// because a partial sum would under-charge real spend.
pub const BUDGET_SCAN_CAP_BYTES: u64 = 32 * 1024 * 1024;

/// Substituted for a model id the record does not carry, so the report
/// names the gap instead of hiding it.
const MISSING_MODEL: &str = "<missing>";

/// How far older than the watermark a record must be before the reverse
/// scan stops. Hosts do not write strictly monotonic timestamps (a
/// tool-result or sidechain line can carry an earlier stamp than the
/// assistant line above it), so the first old line is not the end of the
/// window. Ten minutes is a judgement, not a measured host property
/// (a real 2.1.306 host showed millisecond-scale inversions; see the
/// real-host checklist in ADR-0026): out-of-order stamps further apart than
/// this can still hide newer records.
pub const REORDER_SLACK: time::Duration = time::Duration::minutes(10);

/// How far ahead of the daemon's clock a record may be stamped before it is
/// treated as inconsistent. Covers ordinary skew; anything later never
/// becomes a watermark.
pub const FUTURE_SKEW: time::Duration = time::Duration::minutes(5);

/// One cent in pico-dollars (10^-12 USD).
const PICO_PER_CENT: u128 = 10_000_000_000;
/// Prices are dollars per million tokens, i.e. micro-dollars per token;
/// scaled by 10^6 that is pico-dollars per token, exact for any price with
/// up to six decimals.
const PRICE_SCALE: f64 = 1_000_000.0;

/// What a read covers.
#[derive(Debug, Clone, Copy)]
pub enum ReadMode<'a> {
    /// A parent transcript: only records strictly after `after` (the
    /// newest record timestamp already counted), sidechain records
    /// excluded, and lines of a `message.id` in `counted_ids` skipped (the
    /// tail content blocks of a message an earlier window already counted).
    ParentWindow {
        after: OffsetDateTime,
        counted_ids: &'a HashSet<String>,
        /// The latest instant a record may carry: `now` plus
        /// [`FUTURE_SKEW`]. A record stamped later is inconsistent (clock
        /// skew, a bad line, a hostile file); counting it would push the
        /// watermark ahead of real time and hide every later record.
        horizon: OffsetDateTime,
    },
    /// A subagent's own file: every record, cumulative within that file.
    /// The file is the subagent's, so its records are counted whatever they
    /// are flagged. When `agent_id` is given, a usage record carrying a
    /// different `agentId` makes the file a session mismatch.
    WholeFile { agent_id: Option<&'a str> },
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PricedWindow {
    /// Libra-accounted USD cents, rounded up once for the window.
    pub cents: u64,
    /// Distinct turns that contributed (a content-block-repeated turn
    /// counts once).
    pub records: u64,
    /// The newest timestamp among the records this window counted.
    pub last_event_time: Option<OffsetDateTime>,
    /// The `message.id`s counted (sorted); becomes part of the next
    /// window's watermark.
    pub message_ids: Vec<String>,
    /// Lines of another session (or, for a subagent file, another agent)
    /// that were ignored: they belong to someone else's accounting.
    pub foreign_lines: u64,
    /// Counted records that carried no `message.id`. Each is its own
    /// message (it cannot be deduplicated), so a host that omits ids would
    /// double count content blocks; surfaced so it is not silent.
    pub id_less_records: u64,
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
    /// Usage records exist but **none** is this session's: every one named
    /// a different session. (Foreign lines mixed with own lines are simply
    /// ignored; see [`PricedWindow::foreign_lines`].)
    SessionMismatch,
    /// A record is stamped later than `now + FUTURE_SKEW`. Not counted; the
    /// window cannot be trusted. `newest_in_bound` is the newest in-bound
    /// own record seen, and `message_ids` the ids to remember (in-bound and
    /// future) so the same lines do not poison every later window.
    FutureTimestamp {
        newest_in_bound: Option<OffsetDateTime>,
        message_ids: Vec<String>,
    },
    /// No record named a different session, but at least one could not be
    /// verified (field absent or not a string).
    SessionUnverified,
    /// At least one usage record carried a token count that is not a
    /// non-negative integer, so the window's cost cannot be known.
    MalformedUsage,
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
    #[serde(rename = "agentId", default)]
    agent: Option<Lenient<String>>,
    message: Option<PricedMessage>,
}

#[derive(Deserialize)]
struct PricedMessage {
    #[serde(default)]
    id: Option<Lenient<String>>,
    #[serde(default)]
    model: Option<Lenient<String>>,
    usage: Option<Lenient<PricedUsage>>,
}

/// Every count is lenient: a token field that is present but not a
/// non-negative integer (a float, a string, a negative) must not make serde
/// fail the whole line and drop the record silently (that would be an
/// invisible under-count). It is detected and the window settles
/// conservatively instead (see [`PricedObservation::MalformedUsage`]).
/// An explicit `null` or an absent count is zero, as in the token reader.
#[derive(Deserialize)]
struct PricedUsage {
    #[serde(default)]
    input_tokens: Option<Lenient<u64>>,
    #[serde(default)]
    cache_creation_input_tokens: Option<Lenient<u64>>,
    #[serde(default)]
    cache_read_input_tokens: Option<Lenient<u64>>,
    #[serde(default)]
    output_tokens: Option<Lenient<u64>>,
}

impl PricedUsage {
    /// `None` when any present count is not a non-negative integer.
    fn tiers(self) -> Option<Tiers> {
        let count = |v: Option<Lenient<u64>>| match v {
            None => Some(0),
            Some(l) => l.into_value(),
        };
        Some(Tiers {
            input: count(self.input_tokens)?,
            cache_creation: count(self.cache_creation_input_tokens)?,
            cache_read: count(self.cache_read_input_tokens)?,
            output: count(self.output_tokens)?,
        })
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
    foreign: u64,
    unverified: bool,
    untimed: bool,
    malformed: bool,
    future: bool,
    future_ids: Vec<String>,
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
    if let (ReadMode::ParentWindow { after, .. }, Some(ts)) = (mode, timestamp) {
        if ts <= after {
            // Not counted: it is at or before the watermark. But the scan
            // does NOT stop here. A host may stamp a user / tool-result /
            // sidechain line out of order, and breaking at the first old
            // line of any type would hide every newer assistant record
            // above it (an invisible under-count). The scan stops only
            // once records are older than the watermark by more than
            // [`REORDER_SLACK`]; failing that it ends at the byte bound
            // ([`BUDGET_SCAN_CAP_BYTES`]), which is reported as
            // unmeasured, never as a partial sum.
            if ts < after - REORDER_SLACK {
                return ControlFlow::Break(());
            }
            return ControlFlow::Continue(());
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
    let usage = usage.into_value().and_then(PricedUsage::tiers);
    let message_id = message.id.and_then(Lenient::into_value);
    if let (ReadMode::ParentWindow { counted_ids, .. }, Some(id)) = (mode, message_id.as_ref()) {
        if counted_ids.contains(id) {
            // A later content block of a message an earlier window already
            // counted (the host flushed it after that Stop).
            return ControlFlow::Continue(());
        }
    }

    // Binding is judged on usage records only: they are what would be
    // charged. A line of another session is not this lane's spend and is
    // ignored (counted, not priced); a line whose field cannot be read
    // could be ours, so the window is unverifiable.
    match record.session.and_then(Lenient::into_value) {
        Some(s) if s == expected_session => {}
        Some(_) => {
            acc.foreign += 1;
            return ControlFlow::Continue(());
        }
        None => acc.unverified = true,
    }
    if let ReadMode::WholeFile {
        agent_id: Some(expected),
    } = mode
    {
        if record
            .agent
            .and_then(Lenient::into_value)
            .is_some_and(|a| a != expected)
        {
            acc.foreign += 1;
            return ControlFlow::Continue(());
        }
    }
    if let (ReadMode::ParentWindow { horizon, .. }, Some(ts)) = (mode, timestamp) {
        if ts > horizon {
            acc.future = true;
            if let Some(id) = message_id {
                acc.future_ids.push(id);
            }
            return ControlFlow::Continue(());
        }
    }
    if matches!(mode, ReadMode::ParentWindow { .. }) && timestamp.is_none() {
        // Cannot be placed relative to the watermark, so counting it could
        // double-charge it in a later window.
        acc.untimed = true;
    }
    if let Some(ts) = timestamp {
        acc.last_event_time = Some(acc.last_event_time.map_or(ts, |t| t.max(ts)));
    }

    let Some(tiers) = usage else {
        // Present but not integer counts: counted as a record whose cost
        // is unknowable, flagged, never dropped.
        acc.malformed = true;
        return ControlFlow::Continue(());
    };
    let model = message
        .model
        .and_then(Lenient::into_value)
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| MISSING_MODEL.to_string());
    match message_id {
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
    if acc.unverified {
        return PricedObservation::SessionUnverified;
    }
    if acc.malformed {
        return PricedObservation::MalformedUsage;
    }
    if acc.untimed {
        return PricedObservation::Unavailable(UsageUnavailable::Inconsistent);
    }
    let distinct_turns = acc.by_id.len() as u64 + acc.id_less_records;
    let mut message_ids: Vec<String> = acc.by_id.keys().cloned().collect();
    message_ids.sort_unstable();
    if acc.future {
        message_ids.append(&mut acc.future_ids);
        message_ids.sort_unstable();
        message_ids.dedup();
        return PricedObservation::FutureTimestamp {
            newest_in_bound: acc.last_event_time,
            message_ids,
        };
    }
    for (_, (tiers, model)) in acc.by_id.drain() {
        acc.per_model.entry(model).or_default().add(tiers);
    }
    if distinct_turns == 0 {
        return if acc.foreign > 0 {
            PricedObservation::SessionMismatch
        } else {
            PricedObservation::Unavailable(UsageUnavailable::NoUsageInWindow)
        };
    }
    match window_cost_cents(&acc.per_model, table) {
        Ok(cents) => PricedObservation::Priced(PricedWindow {
            cents,
            records: distinct_turns,
            last_event_time: acc.last_event_time,
            message_ids,
            foreign_lines: acc.foreign,
            id_less_records: acc.id_less_records,
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
                counted_ids: &HashSet::new(),
                horizon: epoch_plus(4_000_000_000),
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
            ReadMode::WholeFile { agent_id: None },
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
                counted_ids: &HashSet::new(),
                horizon: epoch_plus(4_000_000_000),
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
    fn foreign_lines_are_ignored_and_own_lines_are_priced() {
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
            [9_000_000, 0, 0, 0],
            &sid("sess-B"),
        );
        // Own + foreign: only the own line is priced; the foreign one is
        // counted, not charged.
        let PricedObservation::Priced(w) = observe(&file(&[ok.clone(), other.clone()]), 0) else {
            panic!("expected Priced");
        };
        assert_eq!((w.cents, w.foreign_lines), (300, 1));
        // A line whose field cannot be read could be ours: unverifiable.
        assert_eq!(
            observe(&file(&[ok.clone(), none.clone()]), 0),
            PricedObservation::SessionUnverified
        );
        assert_eq!(
            observe(&file(&[ok, none, other]), 0),
            PricedObservation::SessionUnverified
        );
    }

    #[test]
    fn a_future_stamped_record_is_not_counted_and_flags_the_window() {
        let f = file(&[
            line(
                10,
                "a",
                "claude-sonnet-4-5",
                [1_000_000, 0, 0, 0],
                &sid(SESSION),
            ),
            line(
                9_000_000_000,
                "far",
                "claude-sonnet-4-5",
                [1_000_000, 0, 0, 0],
                &sid(SESSION),
            ),
        ]);
        let PricedObservation::FutureTimestamp {
            newest_in_bound,
            message_ids,
        } = observe_priced(
            f.path(),
            ReadMode::ParentWindow {
                after: epoch_plus(0),
                counted_ids: &HashSet::new(),
                horizon: epoch_plus(1_000),
            },
            SESSION,
            &PricingTable::pinned(),
            BUDGET_SCAN_CAP_BYTES,
        )
        else {
            panic!("expected FutureTimestamp");
        };
        assert_eq!(newest_in_bound, Some(epoch_plus(10)));
        assert_eq!(message_ids, vec!["a".to_string(), "far".to_string()]);
        // Once its id is remembered, the same line no longer flags a window.
        let ids: HashSet<String> = message_ids.into_iter().collect();
        let again = observe_priced(
            f.path(),
            ReadMode::ParentWindow {
                after: epoch_plus(10),
                counted_ids: &ids,
                horizon: epoch_plus(1_000),
            },
            SESSION,
            &PricingTable::pinned(),
            BUDGET_SCAN_CAP_BYTES,
        );
        assert_eq!(
            again,
            PricedObservation::Unavailable(UsageUnavailable::NoUsageInWindow)
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
            ReadMode::WholeFile { agent_id: None },
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
            ReadMode::WholeFile { agent_id: None },
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
    fn observe_skipping(
        f: &tempfile::NamedTempFile,
        after: i64,
        counted: &[&str],
    ) -> PricedObservation {
        let ids: HashSet<String> = counted.iter().map(|s| s.to_string()).collect();
        observe_priced(
            f.path(),
            ReadMode::ParentWindow {
                after: epoch_plus(after),
                counted_ids: &ids,
                horizon: epoch_plus(4_000_000_000),
            },
            SESSION,
            &PricingTable::pinned(),
            BUDGET_SCAN_CAP_BYTES,
        )
    }

    #[test]
    fn lines_of_an_already_counted_message_are_skipped_and_do_not_move_the_watermark() {
        let f = file(&[
            line(
                20,
                "done",
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
        let PricedObservation::Priced(w) = observe_skipping(&f, 10, &["done"]) else {
            panic!("expected Priced");
        };
        assert_eq!(w.cents, 300);
        assert_eq!(w.message_ids, vec!["new".to_string()]);
        assert_eq!(w.last_event_time, Some(epoch_plus(30)));
        // Only skipped lines in the window: nothing to measure.
        let only = file(&[line(
            20,
            "done",
            "claude-sonnet-4-5",
            [1, 0, 0, 0],
            &sid(SESSION),
        )]);
        assert_eq!(
            observe_skipping(&only, 10, &["done"]),
            PricedObservation::Unavailable(UsageUnavailable::NoUsageInWindow)
        );
    }

    #[test]
    fn an_old_line_within_the_slack_does_not_truncate_the_scan() {
        let user = |secs: i64| {
            format!(
                r#"{{"type":"user","timestamp":"{}",{}"message":{{"role":"user"}}}}"#,
                rfc(secs),
                sid(SESSION)
            )
        };
        // Order: A (new), an old user line inside the slack, B (new).
        let f = file(&[
            line(
                2_000,
                "a",
                "claude-sonnet-4-5",
                [1_000_000, 0, 0, 0],
                &sid(SESSION),
            ),
            user(1_950),
            line(
                2_010,
                "b",
                "claude-sonnet-4-5",
                [1_000_000, 0, 0, 0],
                &sid(SESSION),
            ),
        ]);
        assert_eq!(cents(observe(&f, 1_990)), 600);
        // Beyond the slack the scan stops: the documented limit.
        let far = file(&[
            line(
                5_000,
                "a",
                "claude-sonnet-4-5",
                [1_000_000, 0, 0, 0],
                &sid(SESSION),
            ),
            user(100),
            line(
                5_010,
                "b",
                "claude-sonnet-4-5",
                [1_000_000, 0, 0, 0],
                &sid(SESSION),
            ),
        ]);
        assert_eq!(cents(observe(&far, 4_990)), 300);
    }

    #[test]
    fn timestamp_less_lines_are_ignored_and_never_break_the_scan() {
        let f = file(&[
            line(
                10,
                "a",
                "claude-sonnet-4-5",
                [1_000_000, 0, 0, 0],
                &sid(SESSION),
            ),
            r#"{"type":"last-prompt","sessionId":"sess-A"}"#.to_string(),
            r#"{"type":"atis-latch"}"#.to_string(),
            line(
                20,
                "b",
                "claude-sonnet-4-5",
                [1_000_000, 0, 0, 0],
                &sid(SESSION),
            ),
        ]);
        assert_eq!(cents(observe(&f, 0)), 600);
    }

    #[test]
    fn non_integer_token_counts_are_malformed_not_dropped() {
        for bad in ["1.5", "\"7\"", "-3", "true"] {
            let l = line(
                10,
                "m",
                "claude-sonnet-4-5",
                [1_000_000, 0, 0, 0],
                &sid(SESSION),
            )
            .replace(
                "\"input_tokens\":1000000",
                &format!("\"input_tokens\":{bad}"),
            );
            let f = file(&[
                l,
                line(
                    11,
                    "ok",
                    "claude-sonnet-4-5",
                    [1_000_000, 0, 0, 0],
                    &sid(SESSION),
                ),
            ]);
            assert_eq!(observe(&f, 0), PricedObservation::MalformedUsage, "{bad}");
        }
        // null and absent counts are zero, as in the token reader.
        let l = line(10, "m", "claude-sonnet-4-5", [0, 0, 0, 0], &sid(SESSION))
            .replace("\"input_tokens\":0", "\"input_tokens\":null");
        assert!(matches!(
            observe(&file(&[l]), 0),
            PricedObservation::Priced(_)
        ));
    }

    #[test]
    fn lines_without_a_message_id_are_each_their_own_message_and_counted_in_the_report() {
        let l = |secs: i64| {
            format!(
                r#"{{"type":"assistant","timestamp":"{}",{}"message":{{"model":"claude-sonnet-4-5","usage":{{"input_tokens":1000000}}}}}}"#,
                rfc(secs),
                sid(SESSION)
            )
        };
        let PricedObservation::Priced(w) = observe(&file(&[l(10), l(11)]), 0) else {
            panic!("expected Priced");
        };
        assert_eq!((w.cents, w.records, w.id_less_records), (600, 2, 2));
        assert!(w.message_ids.is_empty());
    }

    #[test]
    fn a_subagent_line_naming_another_agent_is_a_mismatch() {
        let l = line(
            10,
            "m",
            "claude-sonnet-4-5",
            [1_000_000, 0, 0, 0],
            &format!("{}\"agentId\":\"zzz\",", sid(SESSION)),
        );
        let f = file(&[l]);
        let read = |agent| {
            observe_priced(
                f.path(),
                ReadMode::WholeFile { agent_id: agent },
                SESSION,
                &PricingTable::pinned(),
                BUDGET_SCAN_CAP_BYTES,
            )
        };
        assert_eq!(read(Some("aaa")), PricedObservation::SessionMismatch);
        assert!(matches!(read(Some("zzz")), PricedObservation::Priced(_)));
        assert!(matches!(read(None), PricedObservation::Priced(_)));
    }
}
