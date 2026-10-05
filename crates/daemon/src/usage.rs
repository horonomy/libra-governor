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
//!   rather than work performed.
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
//! # Privacy
//!
//! The transcript is the single most sensitive local artifact this
//! product can see: it holds prompts, file contents and tool output. This
//! module reads it and extracts **only** the four integer counts above.
//! Parsing goes through the narrow [`TranscriptRecord`] struct rather
//! than a `serde_json::Value`, so message content is skipped during
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
use std::path::Path;

use serde::Deserialize;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

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
/// assistant record inside the measured window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TurnUsage {
    pub input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub output_tokens: u64,
    /// How many assistant records contributed. Zero means the window
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
    /// The host's hook payload carried no `transcript_path`. True of
    /// Codex today — see `docs/adr/0004-agent-adapter-contract.md`.
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
    /// assistant record.
    ///
    /// Distinct from `Measured(0)`, which this module never produces: a
    /// real zero and "nothing was observed" must not be the same value
    /// downstream, because the first is a fact about spend and the second
    /// is the absence of one.
    NoUsageInWindow,
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

/// Sums host-reported usage over every assistant record in `path` whose
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

fn scan(
    path: &Path,
    window_start: OffsetDateTime,
    scan_cap_bytes: u64,
) -> Result<TurnUsage, UsageUnavailable> {
    let mut file = File::open(path).map_err(|_| UsageUnavailable::Unreadable)?;
    let len = file
        .seek(SeekFrom::End(0))
        .map_err(|_| UsageUnavailable::Unreadable)?;

    let mut usage = TurnUsage::default();
    // Bytes belonging to a line whose beginning lies in an
    // earlier (not-yet-read) chunk.
    let mut partial_head: Vec<u8> = Vec::new();
    let mut pos = len;
    let mut scanned: u64 = 0;

    while pos > 0 {
        // Clamped to the remaining budget, not just checked between
        // chunks: a bound that permits one more whole chunk before it
        // trips is not the bound it claims to be.
        let remaining_budget = scan_cap_bytes.saturating_sub(scanned);
        if remaining_budget == 0 {
            return Err(UsageUnavailable::ScanCapReached);
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
            match consider_line(line, window_start) {
                LineVerdict::Older => return Ok(usage),
                LineVerdict::Skip => {}
                LineVerdict::Counted(raw) => {
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
            }
        }
    }

    // Reached the start of the file without finding anything older than
    // the window: the window covers the whole transcript, so the total is
    // complete.
    Ok(usage)
}

enum LineVerdict {
    /// Not an assistant usage record, or unparseable — ignore it.
    Skip,
    /// Older than the window: the backwards scan is done.
    Older,
    Counted(RawUsage),
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
    if record.kind.as_deref() != Some("assistant") {
        return LineVerdict::Skip;
    }
    match record.message.and_then(|m| m.usage) {
        Some(raw) => LineVerdict::Counted(raw),
        None => LineVerdict::Skip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

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
        ] {
            let label = observation.reason_label();
            assert!(
                label.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "a reason code is a fixed vocabulary, never a channel for file content: {label:?}"
            );
        }
    }
}
