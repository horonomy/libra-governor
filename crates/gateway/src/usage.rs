//! [`ObservedUsage`] and [`UsageAccumulator`] — reading the provider's own
//! token accounting out of a response the gateway is relaying byte for
//! byte (HORO-1144).
//!
//! # Observe, never interfere
//!
//! The accumulator is a *tap*. Bytes are forwarded to the client exactly
//! as they arrive — including SSE `ping` events, comment lines, and
//! whatever framing the provider chooses — and a copy is fed through
//! here. Nothing in this module can alter, delay, or drop a byte on its
//! way to the agent. Claude Code's own watchdog counts raw bytes during
//! extended thinking pauses, so buffering a stream to parse it would
//! break the client for a benefit (tidier parsing) that is worth nothing.
//!
//! # Cumulative, not incremental
//!
//! Anthropic's `message_delta` events carry a `usage` object whose
//! `output_tokens` is the running **cumulative** total, not that event's
//! increment. Summing them would multiply the reported spend by roughly
//! the number of deltas in the stream. [`UsageAccumulator`] therefore
//! *replaces* rather than adds: the last value seen wins. This is the
//! single easiest way to get this wrong and the reason the type exists at
//! all instead of an inline `+=`.
//!
//! # Bounded memory on a hostile or broken stream
//!
//! The line buffer is capped at [`MAX_LINE_BYTES`]. A stream that never
//! emits a newline cannot grow it without bound: past the cap the buffer
//! is dropped and the accumulator resynchronises at the next newline.
//! Losing one oversized line costs, at worst, precision in the settled
//! figure — which then falls back to the conservative reserved amount.
//! Growing without bound would cost the daemon.

use serde::Deserialize;

/// The largest single SSE line the accumulator will buffer before
//  dropping it and resynchronising. Real `message_start` frames are a few
/// hundred bytes; 64 KiB is generous by orders of magnitude while still
/// being a bound.
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// Exact per-tier token counts as reported by the provider.
///
/// Four separate counters rather than one total because the four tiers
/// are priced differently — see [`crate::pricing::ModelPricing`]. A
/// single "tokens used" number could not be priced correctly and could
/// not be audited afterwards.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ObservedUsage {
    pub input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub output_tokens: u64,
}

impl ObservedUsage {
    /// Every reported token, regardless of tier — the figure a
    /// token-denominated budget settles against.
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_creation_input_tokens)
            .saturating_add(self.cache_read_input_tokens)
            .saturating_add(self.output_tokens)
    }
}

/// The `usage` object as it appears in a `message_start` frame, a
/// `message_delta` frame, or a non-streaming response body. Every field
/// is optional: `message_delta` carries only the counters that changed,
/// and older API revisions omit the cache tiers entirely.
#[derive(Debug, Deserialize)]
struct UsageFields {
    input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct StreamFrame {
    #[serde(rename = "type")]
    frame_type: Option<String>,
    message: Option<FrameMessage>,
    usage: Option<UsageFields>,
}

#[derive(Debug, Deserialize)]
struct FrameMessage {
    usage: Option<UsageFields>,
}

/// Accumulates the provider's reported usage from a response body as it
/// streams past, without buffering the body.
#[derive(Debug, Default)]
pub struct UsageAccumulator {
    line: Vec<u8>,
    /// Set once the buffer overflowed and the rest of the current line is
    /// being discarded, so a truncated fragment is never parsed as a
    /// whole frame.
    resyncing: bool,
    usage: ObservedUsage,
    /// `true` once any frame carrying a usage object has been parsed.
    /// Distinguishes "the provider said zero" from "we never learned
    /// anything", which settle differently — see
    /// [`libra_governor_domain::Reservation::usage_known`].
    saw_usage: bool,
    saw_message_start: bool,
    saw_message_stop: bool,
}

impl UsageAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds a chunk of response bytes. Never returns an error: a
    /// malformed stream degrades the *precision* of settlement, it does
    /// not fail the request the client is already receiving.
    pub fn feed(&mut self, chunk: &[u8]) {
        for &byte in chunk {
            if byte == b'\n' {
                if self.resyncing {
                    // The line this newline terminates was truncated;
                    // drop it and resume cleanly from here.
                    self.resyncing = false;
                    self.line.clear();
                    continue;
                }
                let line = std::mem::take(&mut self.line);
                self.consume_line(&line);
                continue;
            }
            if self.resyncing {
                continue;
            }
            if self.line.len() >= MAX_LINE_BYTES {
                self.line.clear();
                self.resyncing = true;
                continue;
            }
            self.line.push(byte);
        }
    }

    /// Parses a whole (non-streaming) response body.
    pub fn feed_whole_body(&mut self, body: &[u8]) {
        if let Ok(frame) = serde_json::from_slice::<StreamFrame>(body) {
            self.apply_frame(&frame);
        }
    }

    fn consume_line(&mut self, line: &[u8]) {
        // SSE field lines are `field: value`; everything else (blank
        // separators, `:` comments, `event:` names) carries no usage.
        let Some(rest) = line.strip_prefix(b"data:") else {
            return;
        };
        let payload = trim_ascii(rest);
        if payload.is_empty() || payload == b"[DONE]" {
            return;
        }
        let Ok(frame) = serde_json::from_slice::<StreamFrame>(payload) else {
            return;
        };
        self.apply_frame(&frame);
    }

    fn apply_frame(&mut self, frame: &StreamFrame) {
        match frame.frame_type.as_deref() {
            Some("message_start") => self.saw_message_start = true,
            Some("message_stop") => self.saw_message_stop = true,
            _ => {}
        }
        // `message_start` nests usage under `message`; `message_delta`
        // and a non-streaming body carry it at the top level.
        if let Some(usage) = frame
            .message
            .as_ref()
            .and_then(|m| m.usage.as_ref())
            .or(frame.usage.as_ref())
        {
            self.apply_usage(usage);
        }
    }

    /// Applies one reported usage object. Every present counter REPLACES
    /// the stored one — see module docs on why adding would be wrong.
    fn apply_usage(&mut self, fields: &UsageFields) {
        let mut saw_any = false;
        if let Some(v) = fields.input_tokens {
            self.usage.input_tokens = v;
            saw_any = true;
        }
        if let Some(v) = fields.cache_creation_input_tokens {
            self.usage.cache_creation_input_tokens = v;
            saw_any = true;
        }
        if let Some(v) = fields.cache_read_input_tokens {
            self.usage.cache_read_input_tokens = v;
            saw_any = true;
        }
        if let Some(v) = fields.output_tokens {
            self.usage.output_tokens = v;
            saw_any = true;
        }
        self.saw_usage |= saw_any;
    }

    /// The usage observed so far, or `None` if the provider never
    /// reported any.
    ///
    /// `None` is meaningful and must not be flattened to a zeroed
    /// [`ObservedUsage`]: settling at zero would refund a request that
    /// certainly cost something. A caller seeing `None` settles with the
    /// conservative reserved-amount fallback instead.
    pub fn observed(&self) -> Option<ObservedUsage> {
        self.saw_usage.then_some(self.usage)
    }

    /// `true` if a `message_start` frame was parsed — i.e. the provider
    /// actually began a response, as opposed to the connection producing
    /// nothing parseable at all.
    pub fn saw_message_start(&self) -> bool {
        self.saw_message_start
    }

    /// `true` if the stream reached `message_stop`, i.e. it ended the way
    /// the protocol says a complete response ends rather than being cut
    /// short.
    pub fn completed_cleanly(&self) -> bool {
        self.saw_message_stop
    }
}

/// Trims leading/trailing ASCII whitespace. `[u8]::trim_ascii` is stable
/// but writing it out keeps the minimum supported toolchain unpinned by
/// this one call.
fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map(|i| i + 1)
        .unwrap_or(start);
    &bytes[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    const STREAM: &str = concat!(
        "event: message_start\n",
        r#"data: {"type":"message_start","message":{"usage":{"input_tokens":25,"cache_creation_input_tokens":100,"cache_read_input_tokens":200,"output_tokens":1}}}"#,
        "\n\n",
        "event: ping\n",
        "data: {\"type\": \"ping\"}\n\n",
        ": this is an SSE comment line\n\n",
        "event: content_block_delta\n",
        r#"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"hi"}}"#,
        "\n\n",
        "event: message_delta\n",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":12}}"#,
        "\n\n",
        "event: message_delta\n",
        r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":40}}"#,
        "\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );

    #[test]
    fn takes_the_last_cumulative_output_count_and_never_sums_deltas() {
        let mut acc = UsageAccumulator::new();
        acc.feed(STREAM.as_bytes());
        let usage = acc.observed().expect("the stream reported usage");
        assert_eq!(
            usage.output_tokens, 40,
            "message_delta usage is cumulative — summing 1 + 12 + 40 would triple-count"
        );
        assert_eq!(usage.input_tokens, 25);
        assert_eq!(usage.cache_creation_input_tokens, 100);
        assert_eq!(usage.cache_read_input_tokens, 200);
        assert!(acc.saw_message_start());
        assert!(acc.completed_cleanly());
    }

    #[test]
    fn parses_identically_when_chunk_boundaries_split_every_frame() {
        let mut whole = UsageAccumulator::new();
        whole.feed(STREAM.as_bytes());

        let mut split = UsageAccumulator::new();
        for byte in STREAM.as_bytes() {
            split.feed(&[*byte]);
        }
        assert_eq!(split.observed(), whole.observed());
        assert_eq!(split.completed_cleanly(), whole.completed_cleanly());
    }

    #[test]
    fn a_stream_cut_off_mid_flight_keeps_the_last_observed_usage() {
        let truncated = &STREAM[..STREAM.find("message_stop").unwrap()];
        let mut acc = UsageAccumulator::new();
        acc.feed(truncated.as_bytes());
        let usage = acc
            .observed()
            .expect("message_start already reported usage");
        assert_eq!(usage.output_tokens, 40);
        assert!(
            !acc.completed_cleanly(),
            "an aborted stream must not claim it finished normally"
        );
    }

    #[test]
    fn a_stream_with_no_usage_at_all_reports_none_not_zero() {
        let mut acc = UsageAccumulator::new();
        acc.feed(b"event: ping\ndata: {\"type\":\"ping\"}\n\n");
        assert_eq!(
            acc.observed(),
            None,
            "settling a zeroed usage would refund a request that certainly cost something"
        );
        assert!(!acc.saw_message_start());
    }

    #[test]
    fn a_provider_reported_zero_is_distinguishable_from_no_report() {
        let mut acc = UsageAccumulator::new();
        acc.feed(br#"data: {"type":"message_delta","usage":{"output_tokens":0}}"#);
        acc.feed(b"\n");
        assert_eq!(acc.observed(), Some(ObservedUsage::default()));
    }

    #[test]
    fn an_oversized_line_is_dropped_without_growing_the_buffer() {
        let mut acc = UsageAccumulator::new();
        let flood = vec![b'x'; MAX_LINE_BYTES * 3];
        acc.feed(b"data: ");
        acc.feed(&flood);
        assert!(
            acc.line.len() <= MAX_LINE_BYTES,
            "the line buffer grew past its cap on an unterminated line"
        );
        // After the newline that ends the flooded line, parsing resumes
        // cleanly on the next well-formed frame.
        acc.feed(b"\n");
        acc.feed(br#"data: {"type":"message_delta","usage":{"output_tokens":7}}"#);
        acc.feed(b"\n");
        assert_eq!(acc.observed().unwrap().output_tokens, 7);
    }

    #[test]
    fn malformed_json_is_ignored_rather_than_failing_the_stream() {
        let mut acc = UsageAccumulator::new();
        acc.feed(b"data: {not json at all\n");
        acc.feed(b"data: \n");
        acc.feed(b"data: [DONE]\n");
        acc.feed(br#"data: {"type":"message_delta","usage":{"output_tokens":5}}"#);
        acc.feed(b"\n");
        assert_eq!(acc.observed().unwrap().output_tokens, 5);
    }

    #[test]
    fn a_non_streaming_body_reports_its_top_level_usage() {
        let mut acc = UsageAccumulator::new();
        acc.feed_whole_body(
            br#"{"id":"msg_1","type":"message","usage":{"input_tokens":11,"output_tokens":22}}"#,
        );
        let usage = acc.observed().unwrap();
        assert_eq!(usage.input_tokens, 11);
        assert_eq!(usage.output_tokens, 22);
    }

    #[test]
    fn an_unparseable_non_streaming_body_reports_no_usage() {
        let mut acc = UsageAccumulator::new();
        acc.feed_whole_body(b"<html>gateway timeout</html>");
        assert_eq!(acc.observed(), None);
    }

    #[test]
    fn total_tokens_sums_every_tier() {
        let usage = ObservedUsage {
            input_tokens: 1,
            cache_creation_input_tokens: 2,
            cache_read_input_tokens: 4,
            output_tokens: 8,
        };
        assert_eq!(usage.total_tokens(), 15);
    }
}
