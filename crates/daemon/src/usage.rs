//! Measured host resource usage, read from the agent host's own local
//! transcript (HORO-1725).
//!
//! # Why this exists
//!
//! Every receipt written before this module recorded `actual_usage` as an
//! empty `Vec`, on the stated premise that "Claude Code's hook payloads
//! expose no token/cost data". That premise was wrong, and the cost of it
//! was total: with no receipt ever carrying a resource amount,
//! `libra_governor_estimator`'s `resource_quantiles` had nothing to take
//! quantiles over, so `Estimate::resource_p80` was unconditionally `None`,
//! so admission fell back to `policy.resource.target` for *every* task,
//! so every reservation on a live store held the identical constant and
//! the rendered budget share was the same number forever. A governor that
//! reserves the same amount for a one-line typo fix and a cross-crate
//! refactor is not measuring anything.
//!
//! The payload does not carry token counts directly, but it carries
//! `transcript_path`, and the host writes per-assistant-turn usage into
//! that transcript: `input_tokens`, `cache_creation_input_tokens`,
//! `cache_read_input_tokens`, `output_tokens`. Those are the same four
//! counts `libra_governor_ledger::GatewayRequestClose` already persists,
//! so this is not a new economic vocabulary — it is the existing one,
//! sourced without requiring a gateway.
//!
//! # What a `Tokens` amount means here
//!
//! [`TurnUsage::fresh_tokens`] is `input_tokens +
//! cache_creation_input_tokens + output_tokens`. `ResourceAmount::Tokens`
//! documents itself as "a raw token count (input + output, or as defined
//! by the caller)", so the definition is this caller's to make and to
//! justify:
//!
//! - `cache_creation_input_tokens` **is** included. Those tokens were
//!   genuinely ingested and are billed above base input rate; they are
//!   fresh work by any reading.
//! - `cache_read_input_tokens` is **excluded**. A cache read re-presents
//!   a prefix that was already ingested and already paid for on an
//!   earlier turn. Summing it would count the same context once per turn
//!   for the life of a session. This is not a rounding difference:
//!   measured over two real local transcripts, per-turn totals were p50
//!   ~370k-510k including cache creation but ~7.7M-11.3M once cache reads
//!   were added — a ~25x inflation that describes caching mechanics
//!   rather than work performed. (These absolute figures predate the
//!   per-content-block dedup added for HORO-1882 below and are very
//!   likely inflated by that separate, now-fixed double-count; the ~25x
//!   *ratio* between the two totals is unaffected, since both sides of
//!   it carried the same double-count.)
//!
//! All four raw counts are read and summed into [`TurnUsage`] regardless,
//! so the choice above is auditable, and changing it later needs no
//! second pass over the transcript.
//!
//! This is deliberately a token count and not a price. Cache creation and
//! cache reads bill at different multiples of base input, so converting
//! to `ResourceAmount::UsdCents` needs a priced rate card and a
//! `pricing_version` to attribute it to — which only the gateway path
//! has. Inventing per-model prices in the daemon is exactly the "fake,
//! potentially misleading USD conversion" that
//! `libra_governor_domain::resource_amount`'s own module header exists to
//! prevent.
//!
//! # Codex rollouts (HORO-1714)
//!
//! Codex's `Stop` payload also carries `transcript_path`, pointing at its
//! rollout file (`$CODEX_HOME/sessions/.../rollout-*.jsonl`). Usage there
//! is one `{"type":"token_usage_record","payload":{...}}` line per
//! completed model response, timestamped in RFC 3339 like Claude Code's
//! records, so the window, backwards scan and scan cap are unchanged.
//! Only the extraction differs, and only in one respect that matters:
//! the counts are **nested**, not disjoint.
//!
//! Codex copies the Responses API's usage object, where
//! `cached_input_tokens` and `cache_write_input_tokens` are breakdowns
//! *of* `input_tokens`, and `reasoning_output_tokens` is a breakdown *of*
//! `output_tokens` (`total_tokens == input_tokens + output_tokens`). In
//! Claude Code's shape `input_tokens` already excludes both cache
//! classes. Copying `input_tokens` across by name would therefore put
//! every cached re-read back into [`TurnUsage::fresh_tokens`] — the ~25x
//! inflation the section above exists to prevent, arriving under a field
//! name that looks correct.
//!
//! So a Codex record is partitioned into the same disjoint counts before
//! it is summed:
//!
//! - `cached_input_tokens` -> `cache_read_input_tokens`. A prefix served
//!   from cache, excluded for the same reason as Claude's cache reads.
//! - `cache_write_input_tokens` -> `cache_creation_input_tokens`. Input
//!   ingested and written to cache on this response: fresh work.
//! - the remainder of `input_tokens` -> `input_tokens`.
//! - `output_tokens` -> `output_tokens` as is. Reasoning tokens are
//!   already inside it; adding `reasoning_output_tokens` again would
//!   count them twice. They are real billed output and they are counted
//!   — once.
//!
//! The result is that `fresh_tokens()` equals Codex's own
//! `TokenUsage::blended_total()` (non-cached input + output), the figure
//! Codex itself prints as "tokens used". That is an upstream anchor, not
//! a definition invented here.
//!
//! Only `payload.usage` is read. The same line carries
//! `turn_token_usage` and `thread_token_usage`, which are running totals;
//! summing those per record would grow quadratically with the session.
//! The `event_msg`/`token_count` lines are a second view of the same
//! responses (and can repeat a `last_token_usage` without a new response,
//! or carry a synthetic total-only figure after a context-window fill),
//! so they are never counted alongside the records.
//!
//! # Claude Code duplicate lines per turn (HORO-1882)
//!
//! Claude Code writes one `assistant` transcript line per *content
//! block* of a turn (text, `tool_use`, …), not one line per turn. Every
//! line belonging to the same logical turn repeats the identical
//! `message.usage` object and shares the same `message.id`. Summing every
//! line therefore summed the same usage once per content block, not once
//! per turn. Checked across every local Claude Code transcript (18
//! files, 959,855 lines): 86,281 `message.id`s appeared on more than one
//! line, and every one of them carried byte-identical usage on each
//! repeat — zero divergence observed. An earlier, smaller sample (4
//! transcripts) estimated the resulting inflation of `fresh_tokens()` at
//! roughly 2.14x-2.39x; that figure predates this fix and most likely
//! carries the same double-count itself, so treat it as an order of
//! magnitude, not a precise before/after ratio.
//!
//! This is fixed by deduplicating on `message.id`: each id is counted
//! once, taking the per-field **maximum** across its repeated lines
//! rather than assuming they are always byte-identical (a safety margin,
//! not an expectation of divergence). A line with no `message.id` is
//! summed exactly as before — this preserves behavior for any
//! transcript shape (e.g. Codex's) that does not carry one. The id is
//! used only as an in-memory dedup key for the duration of one scan; it
//! is never retained, logged, or returned.
//!
//! Dedup is scoped to a single [`observe_transcript_window`] call, not
//! across calls: if a turn's content-block lines straddle a window
//! boundary (an earlier line settled by a previous receipt, a later line
//! falling in the next window), each window counts that id once, so the
//! turn can still be counted twice in total. This residual is strictly
//! smaller than the prior bug, which double- or triple-counted it inside
//! a single window too, and closing it needs a boundary concept this
//! module doesn't have today — out of scope here.
//!
//! # Privacy
//!
//! The transcript is the single most sensitive local artifact this
//! product can see: it holds prompts, file contents and tool output. This
//! module reads it and extracts **only** the four integer counts above,
//! plus — for a Claude Code `assistant` line — `message.id`, an opaque
//! dedup key (see the duplicate-lines section above) that is held only
//! in a local scan-scoped map and dropped when the scan returns. Parsing
//! goes through the narrow [`TranscriptRecord`] and [`CodexUsageRecord`]
//! structs rather than a `serde_json::Value`, so message content is skipped during
//! deserialization instead of being materialized into an owned tree. No
//! text is retained, returned, logged, or included in any error: the
//! error type carries a reason code, never a line, a path excerpt, or a
//! parse message quoting the input. Nothing here transmits anything
//! anywhere — it is a local read, consistent with the repository's
//! privacy invariant that full prompt/source/tool output stays local.
//!
//! # Why the daemon reads the file rather than the CLI
//!
//! The CLI could parse the transcript and send four integers, which would
//! be a smaller wire change. It would also make a spend figure
//! caller-supplied, and `libra_governor_domain::Reservation`'s own docs
//! are emphatic that no caller-supplied value may "forge a spend or
//! credit by itself". Measurement authority stays with the daemon; the
//! CLI forwards only the path the host gave it.
//!
//! A caller-supplied path is still caller-controlled, so the read is
//! bounded ([`DEFAULT_SCAN_CAP_BYTES`]), never follows the file anywhere
//! but open-and-read, and ignores anything it cannot parse as a usage
//! record.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::ops::ControlFlow;
use std::path::Path;

use serde::Deserialize;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

pub mod codex_priced;
mod codex_turn;
pub use codex_turn::observe_codex_turn;
pub mod priced;

/// How much of the transcript tail this is willing to read, in bytes.
///
/// The scan window is "since the previous receipt on this task", which is
/// normally one assistant turn — about 700 KiB in the two local
/// transcripts measured for HORO-1725 (91 MB over 130 turns). 8 MiB
/// therefore covers roughly a dozen turns of history, which is slack for
/// a task that spanned several turns without a receipt, while keeping a
/// `Stop` hook's added latency in the low tens of milliseconds even in
/// the worst case. A transcript can be 90 MB; reading all of it on every
/// turn would not be acceptable in a hook.
pub const DEFAULT_SCAN_CAP_BYTES: u64 = 8 * 1024 * 1024;

/// Read granularity for the backwards scan.
const CHUNK_BYTES: usize = 256 * 1024;

/// The four raw per-turn counts the host reports, summed over every
/// usage record inside the measured window. Disjoint by construction:
/// Codex's nested counts are partitioned into this shape before summing
/// (see the module header).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TurnUsage {
    pub input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub output_tokens: u64,
    /// How many *distinct* turns contributed: one per unique
    /// `message.id` (HORO-1882 dedup) plus one per id-less line. Not a
    /// raw transcript-line count — a turn written as several
    /// content-block lines still counts once. Zero means the window
    /// contained no usage-bearing record at all, which is reported as
    /// [`UsageUnavailable::NoUsageInWindow`] rather than as a measured
    /// zero — see that variant's docs.
    pub records: u64,
}

impl TurnUsage {
    /// Tokens newly ingested or produced: `input + cache_creation +
    /// output`, deliberately excluding cached-prefix re-reads. See this
    /// module's header for why, and for why this is a count rather than a
    /// price.
    pub fn fresh_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_creation_input_tokens)
            .saturating_add(self.output_tokens)
    }
}

/// Why no measurement is available. Each variant is a reason code with no
/// payload: an error here must not become a channel for transcript
/// content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageUnavailable {
    /// The host's hook payload carried no `transcript_path`. Before
    /// HORO-1863 this also covered the execution-owner route
    /// deliberately withholding the path for every identity, because this
    /// session-window reader cannot tell one lane's tokens from a
    /// sibling's. The owner route now forwards a Codex identity's path to
    /// [`codex_turn::observe_codex_turn`] instead of to this reader; every
    /// other identity still reaches this variant exactly as before.
    NoTranscriptPath,
    /// The path could not be opened, read, or seeked.
    Unreadable,
    /// [`DEFAULT_SCAN_CAP_BYTES`] was exhausted before the scan reached
    /// the start of the window, so the total would be an undercount.
    ///
    /// Reported as unavailable rather than as a partial measurement on
    /// purpose. A short total would settle a reservation below what was
    /// actually spent — refunding capacity that was really consumed — and
    /// would bias the estimator's quantiles low for every future task. A
    /// measurement either covers its window or it is not a measurement.
    ScanCapReached,
    /// The window was read in full and contained no usage-bearing
    /// record.
    ///
    /// Distinct from `Measured(0)`, which this module never produces: a
    /// real zero and "nothing was observed" must not be the same value
    /// downstream, because the first is a fact about spend and the second
    /// is the absence of one.
    NoUsageInWindow,
    /// A Codex turn-scoped read (HORO-1863) never found the turn's own
    /// start marker (`event_msg`/`task_started` with a matching
    /// `turn_id`) before exhausting the scan cap or reaching the start of
    /// the file. Unlike the session-window reader above, a turn-scoped
    /// read has an exact lower bound to find; not finding it means the
    /// read cannot be trusted to cover the whole turn.
    TurnIncomplete,
    /// A Codex turn-scoped read (HORO-1863) found a record that
    /// contradicts the turn's own internal accounting invariants (see
    /// `codex_turn` module docs) — a wrong `session_id`, a second
    /// `thread_id`, or a sum that disagrees with the turn's own reported
    /// running total. Never partially trusted: the whole read is
    /// discarded rather than reporting a total built from inconsistent
    /// inputs.
    Inconsistent,
}

/// The result of trying to measure the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageObservation {
    Measured(TurnUsage),
    Unavailable(UsageUnavailable),
}

impl UsageObservation {
    /// The measured [`TurnUsage`], if this is a measurement.
    pub fn measured(&self) -> Option<TurnUsage> {
        match self {
            UsageObservation::Measured(u) => Some(*u),
            UsageObservation::Unavailable(_) => None,
        }
    }

    /// A short, content-free label for the daemon log and for receipt
    /// provenance.
    pub fn reason_label(&self) -> &'static str {
        match self {
            UsageObservation::Measured(_) => "measured",
            UsageObservation::Unavailable(UsageUnavailable::NoTranscriptPath) => {
                "no_transcript_path"
            }
            UsageObservation::Unavailable(UsageUnavailable::Unreadable) => "transcript_unreadable",
            UsageObservation::Unavailable(UsageUnavailable::ScanCapReached) => "scan_cap_reached",
            UsageObservation::Unavailable(UsageUnavailable::NoUsageInWindow) => {
                "no_usage_in_window"
            }
            UsageObservation::Unavailable(UsageUnavailable::TurnIncomplete) => "turn_incomplete",
            UsageObservation::Unavailable(UsageUnavailable::Inconsistent) => "usage_inconsistent",
        }
    }
}

/// The only fields this module deserializes. Everything else in a
/// transcript line — prompts, file contents, tool output — is skipped by
/// serde rather than materialized. See the module header's privacy note.
#[derive(Deserialize)]
struct TranscriptRecord {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
    message: Option<TranscriptMessage>,
}

#[derive(Deserialize)]
struct TranscriptMessage {
    /// Shared by every content-block line of the same logical turn
    /// (HORO-1882) — the dedup key `scan` groups repeated lines by.
    /// Absent or explicit `null` gives `None`, and such a line is summed
    /// rather than deduplicated, exactly as every line was before this
    /// fix. A `message.id` that is present but not a string would fail
    /// this whole record's parse (losing its usage and its timestamp),
    /// not fall back to `None` — verified against every local Claude
    /// Code transcript (959,855 lines across 18 files) that `message.id`
    /// is always absent or a string, never another JSON type.
    #[serde(default)]
    id: Option<String>,
    usage: Option<RawUsage>,
}

/// Every count is `Option`, not `#[serde(default)]`: the host writes an
/// explicit `null` for counts it has no value for, and `default` does not
/// cover an explicit null.
#[derive(Deserialize)]
struct RawUsage {
    input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

/// The `type` tag of a Codex rollout line carrying one response's usage.
const CODEX_USAGE_RECORD: &str = "token_usage_record";

/// A Codex `token_usage_record` line, narrowed to `payload.usage`. The
/// running totals beside it (`turn_token_usage`, `thread_token_usage`)
/// are skipped by serde — see the module header for why they must never
/// be summed.
#[derive(Deserialize)]
struct CodexUsageRecord {
    payload: CodexUsagePayload,
}

#[derive(Deserialize)]
struct CodexUsagePayload {
    usage: CodexRawUsage,
}

/// Codex writes these as `i64` and clamps negatives to zero wherever it
/// consumes them. Reading `u64` would make one negative count fail the
/// whole record, silently dropping its other counts — an undercount.
/// `reasoning_output_tokens` is deliberately not read: it is a subset of
/// `output_tokens`, so it carries no count that is not already summed.
#[derive(Deserialize)]
struct CodexRawUsage {
    input_tokens: Option<i64>,
    cached_input_tokens: Option<i64>,
    cache_write_input_tokens: Option<i64>,
    output_tokens: Option<i64>,
}

impl CodexRawUsage {
    /// Partitions Codex's nested input counts into the disjoint shape
    /// [`TurnUsage`] sums. Each cache class is capped at what is left of
    /// `input_tokens`, so the three input parts always add back up to it
    /// and `fresh_tokens()` stays equal to Codex's `blended_total()` even
    /// if a record's breakdowns are inconsistent.
    fn into_disjoint(self) -> RawUsage {
        let count = |v: Option<i64>| u64::try_from(v.unwrap_or(0)).unwrap_or(0);
        let input = count(self.input_tokens);
        let cache_read = count(self.cached_input_tokens).min(input);
        let cache_creation = count(self.cache_write_input_tokens).min(input - cache_read);
        RawUsage {
            input_tokens: Some(input - cache_read - cache_creation),
            cache_creation_input_tokens: Some(cache_creation),
            cache_read_input_tokens: Some(cache_read),
            output_tokens: Some(count(self.output_tokens)),
        }
    }
}

/// Sums host-reported usage over every usage record in `path` — a Claude
/// Code `assistant` record or a Codex `token_usage_record` — whose
/// timestamp is at or after `window_start`.
///
/// `window_start` must be the end of the last already-accounted window —
/// the previous receipt's `recorded_at` for this task, falling back to the
/// task's plan-lineage start when there is no previous receipt. Passing
/// the lineage start unconditionally would re-count every earlier turn
/// that an earlier receipt already settled, which is the
/// double-counting the one-snapshot accounting invariant forbids.
///
/// Scans backwards from the end of the file, because the window is at the
/// end and the file may be ~90 MB. Stops at the first record older than
/// `window_start`, at the start of the file, or at `scan_cap_bytes`,
/// whichever comes first.
pub fn observe_transcript_window(
    path: &Path,
    window_start: OffsetDateTime,
    scan_cap_bytes: u64,
) -> UsageObservation {
    match scan(path, window_start, scan_cap_bytes) {
        Err(unavailable) => UsageObservation::Unavailable(unavailable),
        Ok(usage) if usage.records == 0 => {
            UsageObservation::Unavailable(UsageUnavailable::NoUsageInWindow)
        }
        Ok(usage) => UsageObservation::Measured(usage),
    }
}

/// How a backward scan over a JSONL file ended, for a caller that needs to
/// distinguish "the visitor asked to stop" from the two cases it cannot
/// control: the scan ran off the start of the file, or it exhausted its
/// byte budget first.
pub(crate) enum ScanEnd<B> {
    /// `visit` returned [`ControlFlow::Break`], carrying its value.
    Visitor(B),
    /// The scan reached the start of the file without the visitor
    /// breaking.
    StartOfFile,
    /// [`CHUNK_BYTES`]-bounded reading exhausted `cap_bytes` before either
    /// of the above.
    CapReached,
}

/// Refuses a `path` that does not resolve to a regular file, without
/// opening it. `transcript_path` is attacker-reachable input (it comes
/// from a hook payload / Codex rollout path), and `File::open` on a FIFO
/// with no writer on the other end blocks the calling thread indefinitely
/// — this daemon only serves one connection at a time, so that would hang
/// every other request. `std::fs::metadata` is a plain `stat` (following
/// symlinks, unlike `lstat`), which never blocks on a FIFO's open
/// semantics, so this check is safe to run before any open and still
/// allows a path that is legitimately a symlink to a regular file.
pub(crate) fn reject_non_regular_file(path: &Path) -> Result<(), UsageUnavailable> {
    let metadata = std::fs::metadata(path).map_err(|_| UsageUnavailable::Unreadable)?;
    if metadata.is_file() {
        Ok(())
    } else {
        Err(UsageUnavailable::Unreadable)
    }
}

/// Opens `path` for reading and refuses anything that is not a regular file,
/// judged on the **opened handle**. [`reject_non_regular_file`] is a stat
/// before the open, so a regular file replaced by a FIFO in between would
/// still block a plain `File::open` forever; `O_NONBLOCK` makes the open of a
/// FIFO return at once and the `fstat` then refuses it. The flag is harmless
/// on a regular file.
pub(crate) fn open_regular_file(path: &Path) -> Result<File, UsageUnavailable> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .map_err(|_| UsageUnavailable::Unreadable)?;
    match file.metadata() {
        Ok(m) if m.is_file() => Ok(file),
        _ => Err(UsageUnavailable::Unreadable),
    }
}

/// Scans `path` backwards from EOF in [`CHUNK_BYTES`] chunks, calling
/// `visit` once per complete line (newest first), stopping at the first
/// [`ControlFlow::Break`], the start of the file, or `cap_bytes`,
/// whichever comes first.
///
/// This is the one place this module seeks and re-assembles chunk-spanning
/// lines; [`scan`] and [`codex_turn::observe_codex_turn`] differ only in
/// what `visit` does with each line and in how they interpret where the
/// scan stopped.
pub(crate) fn scan_lines_backward<B>(
    path: &Path,
    cap_bytes: u64,
    mut visit: impl FnMut(&[u8]) -> ControlFlow<B>,
) -> Result<ScanEnd<B>, UsageUnavailable> {
    reject_non_regular_file(path)?;
    let mut file = open_regular_file(path)?;
    let len = file
        .seek(SeekFrom::End(0))
        .map_err(|_| UsageUnavailable::Unreadable)?;

    // Bytes belonging to a line whose beginning lies in an
    // earlier (not-yet-read) chunk.
    let mut partial_head: Vec<u8> = Vec::new();
    let mut pos = len;
    let mut scanned: u64 = 0;

    while pos > 0 {
        // Clamped to the remaining budget, not just checked between
        // chunks: a bound that permits one more whole chunk before it
        // trips is not the bound it claims to be.
        let remaining_budget = cap_bytes.saturating_sub(scanned);
        if remaining_budget == 0 {
            return Ok(ScanEnd::CapReached);
        }
        let want = CHUNK_BYTES.min(pos as usize).min(remaining_budget as usize);
        let start = pos - want as u64;
        let mut block = vec![0u8; want];
        file.seek(SeekFrom::Start(start))
            .map_err(|_| UsageUnavailable::Unreadable)?;
        file.read_exact(&mut block)
            .map_err(|_| UsageUnavailable::Unreadable)?;
        scanned += want as u64;
        pos = start;

        block.extend_from_slice(&partial_head);
        partial_head.clear();

        let mut lines: Vec<&[u8]> = block.split(|b| *b == b'\n').collect();
        // Unless we just read the very start of the file, the first
        // element is the tail of a line that continues backwards into the
        // next chunk and must not be parsed yet.
        if pos > 0 {
            let head = lines.remove(0);
            partial_head = head.to_vec();
        }

        for line in lines.iter().rev() {
            if let ControlFlow::Break(value) = visit(line) {
                return Ok(ScanEnd::Visitor(value));
            }
        }
    }

    Ok(ScanEnd::StartOfFile)
}

fn scan(
    path: &Path,
    window_start: OffsetDateTime,
    scan_cap_bytes: u64,
) -> Result<TurnUsage, UsageUnavailable> {
    let mut usage = TurnUsage::default();
    // Claude Code writes one line per content block of a turn, repeating
    // the same `message.id` and identical usage on each (HORO-1882). Keyed
    // lines are grouped here and folded into `usage` — once per id, by
    // per-field maximum — only after the scan finishes; id-less lines
    // (any other transcript shape, e.g. Codex's) go straight into `usage`
    // exactly as before. The map lives only for this call and is dropped
    // when it returns — the id itself never leaves this function.
    let mut by_id: std::collections::HashMap<String, RawUsage> = std::collections::HashMap::new();
    let end = scan_lines_backward(path, scan_cap_bytes, |line| {
        match consider_line(line, window_start) {
            LineVerdict::Older => ControlFlow::Break(()),
            LineVerdict::Skip => ControlFlow::Continue(()),
            LineVerdict::Counted(None, raw) => {
                usage.input_tokens = usage
                    .input_tokens
                    .saturating_add(raw.input_tokens.unwrap_or(0));
                usage.cache_creation_input_tokens = usage
                    .cache_creation_input_tokens
                    .saturating_add(raw.cache_creation_input_tokens.unwrap_or(0));
                usage.cache_read_input_tokens = usage
                    .cache_read_input_tokens
                    .saturating_add(raw.cache_read_input_tokens.unwrap_or(0));
                usage.output_tokens = usage
                    .output_tokens
                    .saturating_add(raw.output_tokens.unwrap_or(0));
                usage.records += 1;
                ControlFlow::Continue(())
            }
            LineVerdict::Counted(Some(id), raw) => {
                let entry = by_id.entry(id).or_insert(RawUsage {
                    input_tokens: Some(0),
                    cache_creation_input_tokens: Some(0),
                    cache_read_input_tokens: Some(0),
                    output_tokens: Some(0),
                });
                entry.input_tokens = Some(
                    entry
                        .input_tokens
                        .unwrap_or(0)
                        .max(raw.input_tokens.unwrap_or(0)),
                );
                entry.cache_creation_input_tokens = Some(
                    entry
                        .cache_creation_input_tokens
                        .unwrap_or(0)
                        .max(raw.cache_creation_input_tokens.unwrap_or(0)),
                );
                entry.cache_read_input_tokens = Some(
                    entry
                        .cache_read_input_tokens
                        .unwrap_or(0)
                        .max(raw.cache_read_input_tokens.unwrap_or(0)),
                );
                entry.output_tokens = Some(
                    entry
                        .output_tokens
                        .unwrap_or(0)
                        .max(raw.output_tokens.unwrap_or(0)),
                );
                ControlFlow::Continue(())
            }
        }
    })?;
    for (_, raw) in by_id {
        usage.input_tokens = usage
            .input_tokens
            .saturating_add(raw.input_tokens.unwrap_or(0));
        usage.cache_creation_input_tokens = usage
            .cache_creation_input_tokens
            .saturating_add(raw.cache_creation_input_tokens.unwrap_or(0));
        usage.cache_read_input_tokens = usage
            .cache_read_input_tokens
            .saturating_add(raw.cache_read_input_tokens.unwrap_or(0));
        usage.output_tokens = usage
            .output_tokens
            .saturating_add(raw.output_tokens.unwrap_or(0));
        usage.records += 1;
    }
    match end {
        // A record older than the window, or the start of the file: either
        // way the window has been read in full.
        ScanEnd::Visitor(()) | ScanEnd::StartOfFile => Ok(usage),
        ScanEnd::CapReached => Err(UsageUnavailable::ScanCapReached),
    }
}

enum LineVerdict {
    /// Not a usage record, or unparseable — ignore it.
    Skip,
    /// Older than the window: the backwards scan is done.
    Older,
    /// A usage-bearing record. `Some(id)` is a Claude Code `message.id`
    /// used to deduplicate repeated content-block lines of the same turn
    /// (HORO-1882); `None` means this record carries no such key and is
    /// summed unconditionally, as every record was before this fix.
    Counted(Option<String>, RawUsage),
}

fn consider_line(line: &[u8], window_start: OffsetDateTime) -> LineVerdict {
    if line.is_empty() {
        return LineVerdict::Skip;
    }
    let Ok(record) = serde_json::from_slice::<TranscriptRecord>(line) else {
        // A truncated or non-JSON line is ignored rather than fatal: the
        // host appends to this file concurrently, so the final line can
        // legitimately be mid-write.
        return LineVerdict::Skip;
    };
    // The timestamp decides the window boundary for every record type,
    // not just usage-bearing ones, so a long run of user/tool records
    // older than the window terminates the scan promptly.
    if let Some(ts) = record.timestamp.as_deref() {
        if let Ok(parsed) = OffsetDateTime::parse(ts, &Rfc3339) {
            if parsed < window_start {
                return LineVerdict::Older;
            }
        }
    }
    match record.kind.as_deref() {
        Some("assistant") => match record.message {
            Some(message) => match message.usage {
                Some(raw) => LineVerdict::Counted(message.id, raw),
                None => LineVerdict::Skip,
            },
            None => LineVerdict::Skip,
        },
        // Parsed a second time, narrowly, only once the tag matches: a
        // provider-specific `payload` field on the shared record would
        // make every line whose payload has another shape fail to parse,
        // losing its timestamp and with it the window boundary.
        Some(CODEX_USAGE_RECORD) => match serde_json::from_slice::<CodexUsageRecord>(line) {
            Ok(record) => LineVerdict::Counted(None, record.payload.usage.into_disjoint()),
            Err(_) => LineVerdict::Skip,
        },
        _ => LineVerdict::Skip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A FIFO swapped in after the stat pre-check must be refused on the
    /// opened handle without blocking the daemon: a plain `File::open` of a
    /// FIFO with no writer blocks forever. The open runs on a thread with a
    /// timeout so a regression fails instead of hanging the suite.
    #[cfg(unix)]
    #[test]
    fn a_fifo_is_refused_on_the_opened_handle_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("rollout.fifo");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let (tx, rx) = std::sync::mpsc::channel();
        let path = fifo.clone();
        std::thread::spawn(move || {
            let _ = tx.send(open_regular_file(&path).map(|_| ()));
        });
        let got = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("opening a FIFO must not block");
        assert_eq!(got, Err(UsageUnavailable::Unreadable));
    }

    #[test]
    fn a_regular_file_opens() {
        let f = tempfile::NamedTempFile::new().unwrap();
        assert!(open_regular_file(f.path()).is_ok());
    }

    fn epoch_plus(secs: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(secs)
    }

    fn rfc(secs: i64) -> String {
        epoch_plus(secs).format(&Rfc3339).unwrap()
    }

    fn assistant(secs: i64, input: u64, cc: u64, cr: u64, out: u64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{}","message":{{"usage":{{"input_tokens":{},"cache_creation_input_tokens":{},"cache_read_input_tokens":{},"output_tokens":{}}}}}}}"#,
            rfc(secs),
            input,
            cc,
            cr,
            out
        )
    }

    /// Mirrors a real Claude Code `assistant` transcript line, which
    /// carries `message.id` alongside `message.usage` (HORO-1882). Claude
    /// Code writes one such line per content block of a turn, repeating
    /// the same id and usage on each.
    fn assistant_with_id(secs: i64, id: &str, input: u64, cc: u64, cr: u64, out: u64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{}","message":{{"id":"{}","usage":{{"input_tokens":{},"cache_creation_input_tokens":{},"cache_read_input_tokens":{},"output_tokens":{}}}}}}}"#,
            rfc(secs),
            id,
            input,
            cc,
            cr,
            out
        )
    }

    fn write_transcript(lines: &[String]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
        f.flush().unwrap();
        f
    }

    #[test]
    fn sums_every_assistant_record_inside_the_window() {
        let f = write_transcript(&[assistant(100, 10, 20, 30, 40), assistant(200, 1, 2, 3, 4)]);
        let observed = observe_transcript_window(f.path(), epoch_plus(50), DEFAULT_SCAN_CAP_BYTES);
        let usage = observed.measured().expect("both records are in the window");
        assert_eq!(usage.records, 2);
        assert_eq!(usage.input_tokens, 11);
        assert_eq!(usage.cache_creation_input_tokens, 22);
        assert_eq!(usage.cache_read_input_tokens, 33);
        assert_eq!(usage.output_tokens, 44);
    }

    #[test]
    fn repeated_message_id_with_identical_usage_counts_once() {
        // Three content-block lines of the same turn: text, tool_use,
        // tool_use — same `message.id`, identical usage on each, the
        // exact shape Claude Code writes (HORO-1882).
        let f = write_transcript(&[
            assistant_with_id(100, "msg_dup", 10, 20, 30, 40),
            assistant_with_id(100, "msg_dup", 10, 20, 30, 40),
            assistant_with_id(100, "msg_dup", 10, 20, 30, 40),
        ]);
        let observed = observe_transcript_window(f.path(), epoch_plus(50), DEFAULT_SCAN_CAP_BYTES);
        let usage = observed.measured().expect("the turn is in the window");
        assert_eq!(
            usage.records, 1,
            "three lines of the same turn must count as one measurement, not three"
        );
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.cache_creation_input_tokens, 20);
        assert_eq!(usage.cache_read_input_tokens, 30);
        assert_eq!(usage.output_tokens, 40);
    }

    #[test]
    fn distinct_message_ids_each_count_independently() {
        let f = write_transcript(&[
            assistant_with_id(100, "msg_a", 10, 0, 0, 10),
            assistant_with_id(200, "msg_b", 5, 0, 0, 5),
        ]);
        let observed = observe_transcript_window(f.path(), epoch_plus(50), DEFAULT_SCAN_CAP_BYTES);
        let usage = observed.measured().expect("both turns are in the window");
        assert_eq!(usage.records, 2);
        assert_eq!(usage.input_tokens, 15);
        assert_eq!(usage.output_tokens, 15);
    }

    #[test]
    fn a_line_with_no_message_id_is_summed_exactly_as_before() {
        // No regression for a transcript shape (or fixture) that carries
        // no `message.id` at all: every line is still summed.
        let f = write_transcript(&[assistant(100, 10, 0, 0, 10), assistant(100, 10, 0, 0, 10)]);
        let observed = observe_transcript_window(f.path(), epoch_plus(50), DEFAULT_SCAN_CAP_BYTES);
        let usage = observed
            .measured()
            .expect("both id-less lines are in the window");
        assert_eq!(
            usage.records, 2,
            "id-less lines have no dedup key available and must be summed, same as before HORO-1882"
        );
        assert_eq!(usage.input_tokens, 20);
        assert_eq!(usage.output_tokens, 20);
    }

    #[test]
    fn repeated_message_id_with_divergent_usage_takes_the_per_field_maximum() {
        // Not the normal case (repeated lines are identical in practice),
        // but the rule must be pinned so a future change cannot quietly
        // become "first wins" or "last wins" for a genuinely divergent
        // report.
        let f = write_transcript(&[
            assistant_with_id(100, "msg_dup", 10, 5, 100, 1),
            assistant_with_id(100, "msg_dup", 3, 20, 2, 40),
        ]);
        let observed = observe_transcript_window(f.path(), epoch_plus(50), DEFAULT_SCAN_CAP_BYTES);
        let usage = observed.measured().expect("the turn is in the window");
        assert_eq!(usage.records, 1);
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.cache_creation_input_tokens, 20);
        assert_eq!(usage.cache_read_input_tokens, 100);
        assert_eq!(usage.output_tokens, 40);
    }

    #[test]
    fn a_deduped_turn_and_an_id_less_line_both_contribute() {
        let f = write_transcript(&[
            assistant_with_id(100, "msg_dup", 10, 0, 0, 10),
            assistant_with_id(100, "msg_dup", 10, 0, 0, 10),
            assistant(200, 1, 0, 0, 1),
        ]);
        let observed = observe_transcript_window(f.path(), epoch_plus(50), DEFAULT_SCAN_CAP_BYTES);
        let usage = observed.measured().expect("both are in the window");
        assert_eq!(usage.records, 2, "one deduped turn plus one id-less line");
        assert_eq!(usage.input_tokens, 11);
        assert_eq!(usage.output_tokens, 11);
    }

    /// A realistic multi-turn fixture: each turn has 2-4 content-block
    /// lines sharing one `message.id` and identical usage, interleaved
    /// with `user`/tool-result lines that carry no usage at all — the
    /// shape a real Claude Code transcript has (HORO-1882 AC4).
    #[test]
    fn realistic_multi_turn_fixture_shows_deduped_not_summed_total() {
        let lines = vec![
            r#"{"type":"user","timestamp":"1970-01-01T00:01:40Z","message":{"content":"do the thing"}}"#.to_string(),
            assistant_with_id(101, "msg_test_1", 1000, 200, 0, 50),
            assistant_with_id(102, "msg_test_1", 1000, 200, 0, 50),
            r#"{"type":"user","timestamp":"1970-01-01T00:01:43Z","message":{"content":"tool_result"}}"#.to_string(),
            assistant_with_id(104, "msg_test_1", 1000, 200, 0, 50),
            assistant_with_id(105, "msg_test_2", 500, 1300, 0, 30),
            assistant_with_id(106, "msg_test_2", 500, 1300, 0, 30),
            assistant_with_id(107, "msg_test_2", 500, 1300, 0, 30),
            assistant_with_id(108, "msg_test_2", 500, 1300, 0, 30),
        ];
        let f = write_transcript(&lines);
        let observed = observe_transcript_window(f.path(), epoch_plus(50), DEFAULT_SCAN_CAP_BYTES);
        let usage = observed.measured().expect("both turns are in the window");

        assert_eq!(
            usage.records, 2,
            "two logical turns, however many content-block lines each wrote"
        );
        let deduped_fresh = usage.fresh_tokens();
        assert_eq!(deduped_fresh, (1000 + 200 + 50) + (500 + 1300 + 30));

        // Naive per-line sum: 3 lines for turn 1, 4 lines for turn 2, each
        // repeating its turn's identical fresh-token total.
        let naive_summed_fresh: u64 = 3 * (1000 + 200 + 50) + 4 * (500 + 1300 + 30);
        assert!(
            deduped_fresh < naive_summed_fresh,
            "deduplicated total must be smaller than the naive per-line sum"
        );
        // This fixture's own inflation factor (~3.59x, driven by its 3-
        // and 4-line turns) is a separate figure from the 2.14x-2.39x
        // measured directly on real transcripts for HORO-1882 — the two
        // are not expected to match, since real turns vary in how many
        // content blocks they write. What this fixture pins down is only
        // that summing instead of deduplicating inflates the total at
        // all, and by more than a rounding amount.
        let inflation_factor = naive_summed_fresh as f64 / deduped_fresh as f64;
        assert!(
            inflation_factor > 1.5,
            "this fixture's repeated-line shape should produce a clearly inflated naive sum; \
             got {inflation_factor}x"
        );
    }

    #[test]
    fn fresh_tokens_excludes_cached_prefix_rereads() {
        let usage = TurnUsage {
            input_tokens: 10,
            cache_creation_input_tokens: 20,
            cache_read_input_tokens: 1_000_000,
            output_tokens: 40,
            records: 1,
        };
        assert_eq!(
            usage.fresh_tokens(),
            70,
            "a cache read re-presents a prefix already ingested and already paid for on an \
             earlier turn — counting it would charge the same context once per turn"
        );
    }

    #[test]
    fn records_before_the_window_are_not_counted() {
        let f = write_transcript(&[
            assistant(100, 999, 999, 999, 999),
            assistant(300, 1, 2, 3, 4),
        ]);
        let observed = observe_transcript_window(f.path(), epoch_plus(200), DEFAULT_SCAN_CAP_BYTES);
        let usage = observed
            .measured()
            .expect("the newer record is in the window");
        assert_eq!(
            usage.records, 1,
            "the pre-window record was already accounted for by an earlier receipt — counting it \
             again is the double-count the one-snapshot invariant forbids"
        );
        assert_eq!(usage.input_tokens, 1);
    }

    #[test]
    fn an_empty_window_is_unavailable_rather_than_a_measured_zero() {
        let f = write_transcript(&[assistant(100, 10, 10, 10, 10)]);
        let observed = observe_transcript_window(f.path(), epoch_plus(500), DEFAULT_SCAN_CAP_BYTES);
        assert_eq!(
            observed,
            UsageObservation::Unavailable(UsageUnavailable::NoUsageInWindow),
            "`Measured(0)` would claim the task spent nothing, which is a different assertion \
             from having observed nothing"
        );
        assert!(observed.measured().is_none());
    }

    #[test]
    fn a_truncated_scan_is_unavailable_rather_than_an_undercount() {
        let f = write_transcript(&vec![assistant(100, 10, 10, 10, 10); 200]);
        let observed = observe_transcript_window(f.path(), epoch_plus(50), 64);
        assert_eq!(
            observed,
            UsageObservation::Unavailable(UsageUnavailable::ScanCapReached),
            "settling at a short total would refund capacity that was really spent"
        );
    }

    #[test]
    fn a_missing_transcript_is_unreadable_not_a_panic() {
        let observed = observe_transcript_window(
            Path::new("/nonexistent/horo-1725/transcript.jsonl"),
            epoch_plus(0),
            DEFAULT_SCAN_CAP_BYTES,
        );
        assert_eq!(
            observed,
            UsageObservation::Unavailable(UsageUnavailable::Unreadable)
        );
    }

    #[test]
    fn a_mid_write_final_line_does_not_discard_the_complete_records() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        writeln!(f, "{}", assistant(100, 7, 0, 0, 3)).unwrap();
        // The host appends concurrently, so the last line can be a
        // partial JSON fragment with no trailing newline.
        write!(f, r#"{{"type":"assistant","timestamp":"#).unwrap();
        f.flush().unwrap();
        let observed = observe_transcript_window(f.path(), epoch_plus(50), DEFAULT_SCAN_CAP_BYTES);
        let usage = observed
            .measured()
            .expect("the complete record still counts");
        assert_eq!(usage.records, 1);
        assert_eq!(usage.fresh_tokens(), 10);
    }

    #[test]
    fn records_spanning_a_chunk_boundary_are_parsed_once_and_whole() {
        // Enough records to cross several 256 KiB chunk boundaries, with
        // a filler field so individual lines are large.
        let filler = "x".repeat(4_000);
        let mut lines = Vec::new();
        for i in 0..200 {
            lines.push(format!(
                r#"{{"type":"assistant","timestamp":"{}","pad":"{}","message":{{"usage":{{"input_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":1}}}}}}"#,
                rfc(1_000 + i),
                filler
            ));
        }
        let f = write_transcript(&lines);
        assert!(
            std::fs::metadata(f.path()).unwrap().len() > CHUNK_BYTES as u64,
            "the fixture must actually span more than one chunk or it tests nothing"
        );
        let observed = observe_transcript_window(f.path(), epoch_plus(500), DEFAULT_SCAN_CAP_BYTES);
        let usage = observed.measured().unwrap();
        assert_eq!(
            usage.records, 200,
            "a line split across the backwards-read boundary must be reassembled, not dropped \
             and not counted twice"
        );
        assert_eq!(usage.fresh_tokens(), 400);
    }

    #[test]
    fn non_assistant_and_usageless_records_are_ignored() {
        let f = write_transcript(&[
            format!(
                r#"{{"type":"user","timestamp":"{}","message":{{"content":"secret prompt"}}}}"#,
                rfc(100)
            ),
            format!(r#"{{"type":"assistant","timestamp":"{}"}}"#, rfc(110)),
            assistant(120, 5, 0, 0, 5),
        ]);
        let observed = observe_transcript_window(f.path(), epoch_plus(50), DEFAULT_SCAN_CAP_BYTES);
        let usage = observed.measured().unwrap();
        assert_eq!(usage.records, 1);
        assert_eq!(usage.fresh_tokens(), 10);
    }

    #[test]
    fn an_explicit_null_count_is_treated_as_zero_not_a_parse_failure() {
        let f = write_transcript(&[format!(
            r#"{{"type":"assistant","timestamp":"{}","message":{{"usage":{{"input_tokens":5,"cache_creation_input_tokens":null,"cache_read_input_tokens":null,"output_tokens":5}}}}}}"#,
            rfc(100)
        )]);
        let observed = observe_transcript_window(f.path(), epoch_plus(50), DEFAULT_SCAN_CAP_BYTES);
        let usage = observed
            .measured()
            .expect("null counts must not fail the record");
        assert_eq!(usage.records, 1);
        assert_eq!(usage.fresh_tokens(), 10);
    }

    #[test]
    fn no_reason_label_can_carry_transcript_content() {
        for observation in [
            UsageObservation::Measured(TurnUsage::default()),
            UsageObservation::Unavailable(UsageUnavailable::NoTranscriptPath),
            UsageObservation::Unavailable(UsageUnavailable::Unreadable),
            UsageObservation::Unavailable(UsageUnavailable::ScanCapReached),
            UsageObservation::Unavailable(UsageUnavailable::NoUsageInWindow),
            UsageObservation::Unavailable(UsageUnavailable::TurnIncomplete),
            UsageObservation::Unavailable(UsageUnavailable::Inconsistent),
        ] {
            let label = observation.reason_label();
            assert!(
                label.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "a reason code is a fixed vocabulary, never a channel for file content: {label:?}"
            );
        }
    }

    /// Eight consecutive lines from a real rollout written by `codex-cli
    /// 0.160.0` (`codex exec`, then `codex exec resume --last`) under an
    /// isolated `$HOME`/`$CODEX_HOME`, against a local mock Responses
    /// endpoint. Every key path is Codex's own serializer output; only the
    /// count values were chosen by the mock, so that every breakdown is
    /// non-zero. For each turn: the assistant `response_item`, its
    /// `token_usage_record`, the `event_msg`/`token_count` mirror, and
    /// `task_complete`.
    ///
    /// Turn 1: input 1000 (600 cached, 300 cache-write), output 50 (20
    /// reasoning). Turn 2: input 2000 (1500 cached, 400 cache-write),
    /// output 80 (30 reasoning). Codex itself printed "tokens used 450"
    /// after turn 1 and "1,030" after turn 2.
    const CODEX_ROLLOUT: &str = include_str!("../tests/fixtures/codex-rollout-token-usage.jsonl");

    fn at(rfc3339: &str) -> OffsetDateTime {
        OffsetDateTime::parse(rfc3339, &Rfc3339).unwrap()
    }

    fn codex_lines() -> Vec<String> {
        CODEX_ROLLOUT.lines().map(str::to_owned).collect()
    }

    /// The real turn-2 `token_usage_record`, as a value to derive variants
    /// from without hand-writing its shape.
    fn codex_record_turn_2() -> serde_json::Value {
        let line = codex_lines()
            .into_iter()
            .filter(|l| l.contains(r#""type":"token_usage_record""#))
            .nth(1)
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }

    #[test]
    fn codex_rollout_usage_is_partitioned_into_disjoint_counts() {
        let f = write_transcript(&codex_lines());
        let observed =
            observe_transcript_window(f.path(), at("2026-10-12T03:17:00Z"), DEFAULT_SCAN_CAP_BYTES);
        let usage = observed
            .measured()
            .expect("both token_usage_records are in the window");
        assert_eq!(
            usage.records, 2,
            "one record per completed response; the token_count mirrors must not add a second sample"
        );
        assert_eq!(usage.cache_read_input_tokens, 600 + 1_500);
        assert_eq!(usage.cache_creation_input_tokens, 300 + 400);
        assert_eq!(
            usage.input_tokens,
            (1_000 - 600 - 300) + (2_000 - 1_500 - 400),
            "Codex's input_tokens includes both cache classes; copying it across by name would \
             re-count every cached prefix"
        );
        assert_eq!(
            usage.output_tokens,
            50 + 80,
            "reasoning tokens are already inside output_tokens and must not be added again"
        );
        assert_eq!(
            usage.fresh_tokens(),
            1_030,
            "must equal Codex's own blended_total — what it printed as `tokens used` after turn 2"
        );
    }

    #[test]
    fn codex_window_excludes_responses_already_accounted_for() {
        let f = write_transcript(&codex_lines());
        let observed =
            observe_transcript_window(f.path(), at("2026-10-12T03:17:02Z"), DEFAULT_SCAN_CAP_BYTES);
        let usage = observed.measured().expect("turn 2 is in the window");
        assert_eq!(usage.records, 1);
        assert_eq!(usage.fresh_tokens(), (2_000 - 1_500) + 80);
    }

    #[test]
    fn codex_running_totals_beside_the_usage_are_never_summed() {
        let mut record = codex_record_turn_2();
        for total in ["turn_token_usage", "thread_token_usage"] {
            record["payload"][total]["input_tokens"] = serde_json::json!(1_000_000_000);
            record["payload"][total]["output_tokens"] = serde_json::json!(1_000_000_000);
        }
        let f = write_transcript(&[record.to_string()]);
        let observed = observe_transcript_window(f.path(), epoch_plus(0), DEFAULT_SCAN_CAP_BYTES);
        assert_eq!(
            observed.measured().unwrap().fresh_tokens(),
            (2_000 - 1_500) + 80,
            "turn/thread totals are cumulative; summing them per record grows quadratically"
        );
    }

    #[test]
    fn a_codex_compaction_copy_of_a_usage_record_is_not_counted() {
        let record = codex_record_turn_2();
        let compacted = serde_json::json!({
            "timestamp": record["timestamp"],
            "type": "compacted",
            "payload": {
                "message": "summary",
                "latest_token_usage_record": record["payload"],
            },
        });
        let f = write_transcript(&[compacted.to_string()]);
        let observed = observe_transcript_window(f.path(), epoch_plus(0), DEFAULT_SCAN_CAP_BYTES);
        assert_eq!(
            observed,
            UsageObservation::Unavailable(UsageUnavailable::NoUsageInWindow),
            "a compaction checkpoint repeats a response already counted where it happened"
        );
    }

    #[test]
    fn malformed_or_partial_codex_usage_lines_are_skipped_without_panicking() {
        let real = codex_record_turn_2();
        let mut no_usage = real.clone();
        no_usage["payload"].as_object_mut().unwrap().remove("usage");
        let mut non_numeric = real.clone();
        non_numeric["payload"]["usage"]["input_tokens"] = serde_json::json!("lots");
        let mut payload_not_an_object = real.clone();
        payload_not_an_object["payload"] = serde_json::json!([1, 2, 3]);
        let whole = real.to_string();
        let truncated = &whole[..whole.len() / 2];

        let mut f = tempfile::NamedTempFile::new().unwrap();
        for line in [
            no_usage.to_string(),
            non_numeric.to_string(),
            payload_not_an_object.to_string(),
            whole.clone(),
        ] {
            writeln!(f, "{line}").unwrap();
        }
        // The host appends concurrently, so the last line can be cut off.
        write!(f, "{truncated}").unwrap();
        f.flush().unwrap();

        let observed = observe_transcript_window(f.path(), epoch_plus(0), DEFAULT_SCAN_CAP_BYTES);
        let usage = observed
            .measured()
            .expect("the one well-formed record still counts");
        assert_eq!(usage.records, 1);
        assert_eq!(usage.fresh_tokens(), (2_000 - 1_500) + 80);
    }

    #[test]
    fn a_negative_or_missing_codex_count_is_zero_not_a_dropped_record() {
        let mut negative = codex_record_turn_2();
        negative["payload"]["usage"]["cached_input_tokens"] = serde_json::json!(-5);
        // Older rollouts predate the field; Codex itself defaults it to 0.
        negative["payload"]["usage"]
            .as_object_mut()
            .unwrap()
            .remove("cache_write_input_tokens");
        let f = write_transcript(&[negative.to_string()]);
        let observed = observe_transcript_window(f.path(), epoch_plus(0), DEFAULT_SCAN_CAP_BYTES);
        let usage = observed
            .measured()
            .expect("one bad count must not discard the record's other counts");
        assert_eq!(usage.records, 1);
        assert_eq!(usage.cache_read_input_tokens, 0);
        assert_eq!(usage.cache_creation_input_tokens, 0);
        assert_eq!(usage.fresh_tokens(), 2_000 + 80);
    }

    #[test]
    fn inconsistent_codex_breakdowns_never_exceed_the_reported_input() {
        let mut record = codex_record_turn_2();
        record["payload"]["usage"] = serde_json::json!({
            "input_tokens": 100,
            "cached_input_tokens": 80,
            "cache_write_input_tokens": 50,
            "output_tokens": 10,
            "reasoning_output_tokens": 0,
            "total_tokens": 110,
        });
        let f = write_transcript(&[record.to_string()]);
        let usage = observe_transcript_window(f.path(), epoch_plus(0), DEFAULT_SCAN_CAP_BYTES)
            .measured()
            .unwrap();
        assert_eq!(
            usage.input_tokens + usage.cache_creation_input_tokens + usage.cache_read_input_tokens,
            100,
            "the three input parts are a partition of input_tokens"
        );
        assert_eq!(usage.fresh_tokens(), 100 - 80 + 10);
    }
}
