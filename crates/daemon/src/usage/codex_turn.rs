//! Turn-scoped, measured Codex usage for the Execution Owner `Stop` route
//! (HORO-1863).
//!
//! # Why this exists, and why it is not [`super::observe_transcript_window`]
//!
//! The owner route's `Stop` arm used to refuse every `transcript_path` it
//! was offered (`AssociationUnavailable::Unsupported`), because the only
//! reader available (`super::scan`, now `super::observe_transcript_window`)
//! measures a *session* window: everything in the rollout since the
//! previous receipt. On a lane that only ever runs one turn at a time that
//! is fine. It is wrong the moment a session has concurrent lanes — several
//! agents or subagents writing into the same rollout file — because the
//! session window has no way to tell one lane's tokens from another's. The
//! owner route exists specifically to give each lane its own accounting, so
//! handing it a reader that re-merges every lane back together would silently
//! reintroduce the cross-attribution bug this route exists to fix.
//!
//! This reader instead scans backward for exactly one turn: it sums only
//! the `token_usage_record` lines whose `payload.turn_id` matches the
//! caller's turn, never `root_turn_id` (matching on `root_turn_id` would
//! pull a subagent's whole tree into its parent's total — the same
//! cross-attribution bug, reintroduced one field over).
//!
//! # Start bound
//!
//! The backward scan stops as soon as it finds `event_msg`/`task_started`
//! for this `turn_id` — the one durable marker of where this turn began.
//! Reaching the start of the file or the scan cap first means the turn's
//! extent was never established, so the read cannot be trusted:
//! [`UsageUnavailable::TurnIncomplete`] (start of file) or
//! [`UsageUnavailable::ScanCapReached`] (cap) respectively — kept distinct
//! because they are different operational signals (a turn whose start truly
//! predates the cap vs. a cap that is simply too small).
//!
//! # End bound — what the public Codex source shows, and the deviation that follows
//!
//! The ticket's own research task asked whether the *last*
//! `token_usage_record` for a turn is always on disk before the `Stop` hook
//! fires. Reading `openai/codex` at `main` (the 0.153–0.162 era these
//! rollouts were captured from is close behind it) settles the ordering,
//! and it settles it against the strict end-bound the ticket proposed as
//! the safe fallback:
//!
//! - `codex-rs/core/src/session/turn.rs`, `run_turn`: once the model's
//!   final response for a turn needs no follow-up, it calls
//!   `run_turn_stop_hooks(&sess, &step_context, ...)` and `.await`s it
//!   in-line, *inside* the per-step loop, before `run_turn` returns.
//! - `codex-rs/core/src/tasks/mod.rs`, `on_task_finished`: only runs once
//!   `run_turn` has returned. It is what builds and sends
//!   `EventMsg::TurnComplete` (the rollout's `task_complete`), and it is
//!   also where the *explicit* terminal `flush_rollout().await` barrier
//!   lives (`"failed to flush rollout after emitting terminal turn
//!   event"`).
//!
//! So `task_complete` is persisted strictly *after* the `Stop` hook — our
//! daemon's handler necessarily returns before that write even happens, not
//! merely before it is flushed. Requiring `task_complete` as specified
//! would make the strict variant fail on every single real Codex turn, not
//! "some genuinely-finished turns" as the ticket anticipated — the ordering
//! isn't a race, it's a guarantee in the wrong direction. **Deviation:**
//! this reader does not gate on `task_complete` at all.
//!
//! The usage record itself is a closer call. `record_observed_response_completed`
//! (`codex-rs/core/src/session/mod.rs`) runs earlier in the same step,
//! before the `Stop` hook, and does `.await` its
//! `persist_rollout_items(&[RolloutItem::TokenUsageRecord(record)])`. But
//! that call only sends a command on a bounded `mpsc` channel
//! (`codex-rs/rollout/src/recorder.rs`, `RolloutRecorder`: `tx.send(..)`
//! into a 256-slot channel to a background writer task); the `.await`
//! resolves once the send succeeds, not once the background writer has
//! performed the actual `write(2)`. The only hard barrier for that is
//! `RolloutRecorder::flush()`, which is not called between this persist and
//! the `Stop` hook dispatch — it is called only later, from the same
//! terminal path as `task_complete` above. So whether the final record is
//! already visible to an independent reader at `Stop` time is genuinely
//! unconfirmed by the source: it depends on the background writer task
//! having been scheduled in the interval between the channel send and our
//! hook's round trip, which in every realistic case (process spawn + Unix
//! socket round trip vs. a tokio task wakeup) is a near-certainty but not a
//! guarantee.
//!
//! Given the strict variant is actively broken rather than merely
//! conservative, this module takes the loose bound: `task_started` for the
//! start, no `task_complete` requirement for the end. The risk this leaves
//! — the newest record for the turn still sitting unflushed in the
//! channel — is **not fully closed** by the invariants below, and this
//! doc previously overstated that it was. There are two distinct
//! sub-cases:
//!
//! - **The last line on disk is a half-written record.** The background
//!   writer's `write(2)` was interrupted mid-line (this reader ran between
//!   two partial writes, or the process was killed mid-write). This is
//!   detected directly: a complete JSONL line always ends in `\n`, so this
//!   reader first checks whether the file's final byte is `\n` before
//!   scanning at all, and treats anything else as
//!   [`UsageUnavailable::Inconsistent`] rather than silently proceeding —
//!   a half-written tail line could otherwise fail [`TaggedLine`]'s parse,
//!   be skipped as "not a line this module models", and leave the previous
//!   (older, lower) record's `turn_token_usage` as the scan's "newest"
//!   total, which then spuriously agrees with the sum of records the scan
//!   actually found. Closing *that* path is the point of the check.
//! - **The newest record for the turn never reached the file at all** —
//!   it is still sitting in the writer's bounded `mpsc` channel,
//!   unconsumed, when this reader runs. Nothing is malformed or missing a
//!   trailing newline; the file is simply, legitimately, one record short
//!   of the truth. **This sub-case cannot be detected from the reader
//!   side.** There is no Codex-side end-of-turn marker this reader can
//!   wait for without reintroducing the `task_complete` ordering problem
//!   documented above, and the running-total check only catches a
//!   *mismatch* — a wholly absent final record simply makes the second-
//!   newest record look like the newest, and the sum of what's on disk
//!   agrees with it by construction. This read therefore sums to a true
//!   but incomplete total and reports it `Measured`. This is a real,
//!   accepted residual risk, not a solved one; the reasoning in the
//!   previous paragraph bounds it in practice (near-certainty, not a
//!   guarantee) but does not eliminate it.
//!
//! Per this ticket's core safety rule, every *detected* failure mode here
//! still finalizes the turn unmeasured (`usage_known = false`) rather than
//! ever falling back to a session- or file-wide count. The undetectable
//! sub-case above is the one gap that rule cannot close, because there is
//! nothing to detect.
//!
//! # Consistency checks
//!
//! Before the scan even starts, the file's final byte is checked: anything
//! other than `\n` (and the file is non-empty) means the last line was
//! still being written, and the whole read is
//! [`UsageUnavailable::Inconsistent`] immediately (see the end-bound
//! section above for why this exists).
//!
//! Every `token_usage_record` whose `turn_id` matches is required to agree
//! with the turn's own self-reported invariants before it is trusted:
//! `cached_input_tokens <= input_tokens`, `reasoning_output_tokens <=
//! output_tokens`, and (when `total_tokens` is present)
//! `total_tokens == input_tokens + output_tokens`. A matching record whose
//! `session_id` differs from the caller's, or whose `thread_id` differs
//! from an earlier match's, is also disqualifying. All of these mark the
//! whole read [`UsageUnavailable::Inconsistent`] immediately — never
//! partially trusted.
//!
//! This also covers a record that is *for this turn* but fails to parse at
//! all: the full [`TokenUsageRecordLine`] struct requires `turn_id`,
//! `thread_id`, `session_id`, `usage`, and `turn_token_usage` to all be
//! present and well-typed, so a single malformed field fails the whole
//! parse. Without a cross-check, that record would simply be skipped —
//! bypassing every invariant above for exactly the record they exist to
//! check, and reproducing the missing-final-record undercount if it was
//! the newest one. So a record whose full parse fails is re-parsed against
//! a narrow struct exposing only `payload.turn_id` (tolerant of everything
//! else); if that narrow `turn_id` matches the caller's, the line is
//! [`UsageUnavailable::Inconsistent`] rather than silently skipped.
//!
//! The strongest check runs once the turn's start is reached: the sum of
//! every matched record's own `usage` fields must equal the *newest*
//! matched record's `turn_token_usage` (a running total Codex itself
//! maintains per turn). Disagreement here is exactly what an unflushed or
//! truncated record looks like, and it is caught rather than undercounted.
//!
//! **Deviation from the ticket's literal spec:** the spec asked for any
//! nonzero `cache_write_input_tokens` to be treated as an unexplained
//! drift signal ("never observed non-zero" in the sampled rollouts) and
//! marked `Inconsistent`. That premise does not hold in this repository:
//! `crates/daemon/tests/fixtures/codex-rollout-token-usage.jsonl` is real
//! `codex-cli 0.160.0` serializer output with `cache_write_input_tokens`
//! 300 and 400, and `super::CodexRawUsage::into_disjoint` (added in
//! HORO-1714, already merged) already treats it as legitimate fresh input
//! mapped to `cache_creation_input_tokens`. Following the spec literally
//! would make the identical record `Measured` on the legacy session-window
//! route and `Inconsistent` on this turn-scoped route for the same
//! underlying spend. This reader reuses `into_disjoint` for exactly that
//! reason: so `fresh_tokens()` agrees between the two routes for the same
//! data. A missing `cache_write_input_tokens` (older rollouts predate the
//! field) defaults to 0, matching the same leniency `into_disjoint` already
//! has.
//!
//! # Privacy
//!
//! Same discipline as the rest of this module: narrow structs only, never
//! `serde_json::Value`. No prompt or tool-output content is deserialized.

use std::ops::ControlFlow;
use std::path::Path;

use serde::Deserialize;

use super::{
    scan_lines_backward, CodexRawUsage, ScanEnd, TurnUsage, UsageObservation, UsageUnavailable,
};

/// The `type` tag of a Codex rollout line carrying one response's usage.
const TOKEN_USAGE_RECORD: &str = "token_usage_record";
/// The `type` tag of a lifecycle line, narrowed further by its own nested
/// `payload.type`.
const EVENT_MSG: &str = "event_msg";
/// `event_msg` payload tag marking where a turn began.
const TASK_STARTED: &str = "task_started";

/// Only the top-level `type` tag — enough to route to a narrower struct
/// without materializing anything else on this line.
#[derive(Deserialize)]
struct TaggedLine {
    #[serde(rename = "type")]
    kind: String,
}

/// The four Responses-API-shaped counts, read wherever they appear
/// (`payload.usage` and `payload.turn_token_usage` share this shape).
/// `i64` because Codex writes these signed and clamps negatives itself;
/// `reasoning_output_tokens`/`total_tokens` are read here (unlike
/// [`super::CodexRawUsage`]) because this module validates them, it does
/// not just partition them.
#[derive(Deserialize, Default, Clone, Copy)]
struct CodexUsageFields {
    input_tokens: Option<i64>,
    cached_input_tokens: Option<i64>,
    cache_write_input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    reasoning_output_tokens: Option<i64>,
    total_tokens: Option<i64>,
}

impl CodexUsageFields {
    fn get(field: Option<i64>) -> i64 {
        field.unwrap_or(0).max(0)
    }

    /// The per-record self-consistency invariants this module's docs claim
    /// always hold for real Codex output. A violation here is a format-
    /// drift signal, not something to silently clamp away.
    fn internally_consistent(&self) -> bool {
        if self.input_tokens.is_none() || self.output_tokens.is_none() {
            // Required counts; an older breakdown field may be absent, but
            // these two never are on a genuine usage record.
            return false;
        }
        let input = Self::get(self.input_tokens);
        let cached = Self::get(self.cached_input_tokens);
        let output = Self::get(self.output_tokens);
        let reasoning = Self::get(self.reasoning_output_tokens);
        if cached > input {
            return false;
        }
        if reasoning > output {
            return false;
        }
        if let Some(total) = self.total_tokens {
            // `checked_add`, not `+`: real data can't approach i64::MAX,
            // but an overflow here must read as a format-drift signal
            // (`false`), never as a wrapped/panicking arithmetic op.
            let Some(sum) = input.checked_add(output) else {
                return false;
            };
            if total.max(0) != sum {
                return false;
            }
        }
        true
    }

    /// Converts to the shared [`CodexRawUsage`] shape so this reader's
    /// per-record partitioning into [`TurnUsage`]'s disjoint counts is
    /// byte-for-byte the same policy the legacy session-window route
    /// already applies (HORO-1714) — see this module's doc header on why
    /// that reuse, rather than a stricter re-validation of
    /// `cache_write_input_tokens`, is the deliberate choice here.
    fn as_codex_raw_usage(&self) -> CodexRawUsage {
        CodexRawUsage {
            input_tokens: self.input_tokens,
            cached_input_tokens: self.cached_input_tokens,
            cache_write_input_tokens: self.cache_write_input_tokens,
            output_tokens: self.output_tokens,
        }
    }
}

#[derive(Deserialize)]
struct TokenUsageRecordPayload {
    turn_id: String,
    thread_id: String,
    session_id: String,
    usage: CodexUsageFields,
    turn_token_usage: CodexUsageFields,
}

#[derive(Deserialize)]
struct TokenUsageRecordLine {
    payload: TokenUsageRecordPayload,
}

/// The narrow shape used to recover `turn_id` from a `token_usage_record`
/// line whose full [`TokenUsageRecordLine`] parse failed — tolerant of
/// every other field being missing or malformed. See this module's header
/// on why a record that fails to parse must still be checked for `turn_id`
/// before it is treated as unrelated.
#[derive(Deserialize)]
struct NarrowTurnIdPayload {
    turn_id: Option<String>,
}

#[derive(Deserialize)]
struct NarrowTurnIdLine {
    payload: NarrowTurnIdPayload,
}

#[derive(Deserialize)]
struct EventMsgPayload {
    #[serde(rename = "type")]
    kind: String,
    turn_id: Option<String>,
}

#[derive(Deserialize)]
struct EventMsgLine {
    payload: EventMsgPayload,
}

/// Why the backward scan over one line stopped, distinct from
/// [`UsageUnavailable`] because it is purely internal control flow for
/// [`observe_codex_turn`] — it never crosses the function boundary.
enum StepOutcome {
    /// `task_started` for this turn was found: the scan has reached this
    /// turn's own start and must stop, successfully, regardless of the
    /// chosen end bound (see this module's doc header).
    FoundStart,
    /// A matched record failed a consistency check. The whole read is
    /// discarded immediately — never partially trusted.
    Inconsistent,
}

/// Running, unclamped sums of every matched record's raw `usage` fields,
/// kept separately from the disjoint [`TurnUsage`] this function also
/// accumulates. Comparing *this* against the newest match's
/// `turn_token_usage` is the strong self-consistency check from this
/// module's docs; comparing the already-clamped [`TurnUsage`] would let an
/// out-of-range `cache_write_input_tokens` silently change the answer.
#[derive(Default, Clone, Copy)]
struct RawTotals {
    input_tokens: i64,
    cached_input_tokens: i64,
    cache_write_input_tokens: i64,
    output_tokens: i64,
}

impl RawTotals {
    /// `false` on overflow (real data can never approach `i64::MAX`, so this
    /// is a hardening backstop, not an expected path) — the caller treats it
    /// exactly like any other consistency failure rather than wrapping or
    /// panicking.
    #[must_use]
    fn add(&mut self, fields: &CodexUsageFields) -> bool {
        let Some(input_tokens) = self
            .input_tokens
            .checked_add(CodexUsageFields::get(fields.input_tokens))
        else {
            return false;
        };
        let Some(cached_input_tokens) = self
            .cached_input_tokens
            .checked_add(CodexUsageFields::get(fields.cached_input_tokens))
        else {
            return false;
        };
        let Some(cache_write_input_tokens) = self
            .cache_write_input_tokens
            .checked_add(CodexUsageFields::get(fields.cache_write_input_tokens))
        else {
            return false;
        };
        let Some(output_tokens) = self
            .output_tokens
            .checked_add(CodexUsageFields::get(fields.output_tokens))
        else {
            return false;
        };
        self.input_tokens = input_tokens;
        self.cached_input_tokens = cached_input_tokens;
        self.cache_write_input_tokens = cache_write_input_tokens;
        self.output_tokens = output_tokens;
        true
    }

    fn matches(&self, newest: &CodexUsageFields) -> bool {
        self.input_tokens == CodexUsageFields::get(newest.input_tokens)
            && self.cached_input_tokens == CodexUsageFields::get(newest.cached_input_tokens)
            && self.cache_write_input_tokens
                == CodexUsageFields::get(newest.cache_write_input_tokens)
            && self.output_tokens == CodexUsageFields::get(newest.output_tokens)
    }
}

/// `true` when `path` is empty or its final byte is `\n`; `false` when the
/// file is non-empty and does not end in a newline, meaning its last line
/// was still being written when this read ran. Checked up front, separate
/// from [`scan_lines_backward`]'s own tolerance for an unparseable tail
/// line: that tolerance is correct for
/// [`super::observe_transcript_window`], whose session window is expected
/// to race a concurrent writer, but here it would let a half-written final
/// record silently drop out of the scan and manufacture a false pass of
/// the turn's own running-total check (see this module's header).
fn ends_with_newline(path: &Path) -> Result<bool, UsageUnavailable> {
    use std::io::{Read, Seek, SeekFrom};

    super::reject_non_regular_file(path)?;
    let mut file = super::open_regular_file(path)?;
    let len = file
        .seek(SeekFrom::End(0))
        .map_err(|_| UsageUnavailable::Unreadable)?;
    if len == 0 {
        return Ok(true);
    }
    file.seek(SeekFrom::End(-1))
        .map_err(|_| UsageUnavailable::Unreadable)?;
    let mut last_byte = [0u8; 1];
    file.read_exact(&mut last_byte)
        .map_err(|_| UsageUnavailable::Unreadable)?;
    Ok(last_byte[0] == b'\n')
}

/// Measures usage for exactly one Codex turn: `turn_id`, cross-checked
/// against `session_id`. See this module's header for the full contract.
/// `cap_bytes` bounds the backward scan exactly as
/// [`super::observe_transcript_window`]'s does.
pub fn observe_codex_turn(
    path: &Path,
    session_id: &str,
    turn_id: &str,
    cap_bytes: u64,
) -> UsageObservation {
    match ends_with_newline(path) {
        Ok(true) => {}
        Ok(false) => return UsageObservation::Unavailable(UsageUnavailable::Inconsistent),
        Err(unavailable) => return UsageObservation::Unavailable(unavailable),
    }

    let mut usage = TurnUsage::default();
    let mut raw_totals = RawTotals::default();
    let mut newest_turn_token_usage: Option<CodexUsageFields> = None;
    let mut seen_thread_id: Option<String> = None;

    let end = match scan_lines_backward(path, cap_bytes, |line| {
        step(
            line,
            session_id,
            turn_id,
            &mut usage,
            &mut raw_totals,
            &mut newest_turn_token_usage,
            &mut seen_thread_id,
        )
    }) {
        Ok(end) => end,
        Err(unavailable) => return UsageObservation::Unavailable(unavailable),
    };

    match end {
        ScanEnd::Visitor(StepOutcome::Inconsistent) => {
            UsageObservation::Unavailable(UsageUnavailable::Inconsistent)
        }
        ScanEnd::Visitor(StepOutcome::FoundStart) => {
            if usage.records == 0 {
                return UsageObservation::Unavailable(UsageUnavailable::NoUsageInWindow);
            }
            match newest_turn_token_usage {
                Some(newest) if raw_totals.matches(&newest) => UsageObservation::Measured(usage),
                _ => UsageObservation::Unavailable(UsageUnavailable::Inconsistent),
            }
        }
        ScanEnd::StartOfFile => UsageObservation::Unavailable(UsageUnavailable::TurnIncomplete),
        ScanEnd::CapReached => UsageObservation::Unavailable(UsageUnavailable::ScanCapReached),
    }
}

#[allow(clippy::too_many_arguments)]
fn step(
    line: &[u8],
    session_id: &str,
    turn_id: &str,
    usage: &mut TurnUsage,
    raw_totals: &mut RawTotals,
    newest_turn_token_usage: &mut Option<CodexUsageFields>,
    seen_thread_id: &mut Option<String>,
) -> ControlFlow<StepOutcome> {
    if line.is_empty() {
        return ControlFlow::Continue(());
    }
    let Ok(tagged) = serde_json::from_slice::<TaggedLine>(line) else {
        // Concurrent writer, truncated tail, or a line shape this module
        // does not model (e.g. `response_item`) — never fatal.
        return ControlFlow::Continue(());
    };
    match tagged.kind.as_str() {
        TOKEN_USAGE_RECORD => {
            let record = match serde_json::from_slice::<TokenUsageRecordLine>(line) {
                Ok(record) => record,
                Err(_) => {
                    // The full parse requires turn_id/thread_id/session_id/
                    // usage/turn_token_usage to all be present and
                    // well-typed; any single malformed field fails it even
                    // when this line is for our turn. Re-check the narrow
                    // `turn_id` shape before treating the line as
                    // unrelated -- a record for the right turn that fails
                    // here must not be silently skipped (see this module's
                    // header).
                    let narrow_turn_id = serde_json::from_slice::<NarrowTurnIdLine>(line)
                        .ok()
                        .and_then(|narrow| narrow.payload.turn_id);
                    if narrow_turn_id.as_deref() == Some(turn_id) {
                        return ControlFlow::Break(StepOutcome::Inconsistent);
                    }
                    return ControlFlow::Continue(());
                }
            };
            let payload = record.payload;
            if payload.turn_id != turn_id {
                return ControlFlow::Continue(());
            }
            if payload.session_id != session_id {
                return ControlFlow::Break(StepOutcome::Inconsistent);
            }
            match seen_thread_id {
                Some(existing) if *existing != payload.thread_id => {
                    return ControlFlow::Break(StepOutcome::Inconsistent);
                }
                Some(_) => {}
                None => *seen_thread_id = Some(payload.thread_id.clone()),
            }
            if !payload.usage.internally_consistent() {
                return ControlFlow::Break(StepOutcome::Inconsistent);
            }
            if !raw_totals.add(&payload.usage) {
                return ControlFlow::Break(StepOutcome::Inconsistent);
            }
            let raw = payload.usage.as_codex_raw_usage().into_disjoint();
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
            // Backward scan visits newest-first: the first match sets the
            // comparison baseline, later (older) matches never overwrite it.
            if newest_turn_token_usage.is_none() {
                *newest_turn_token_usage = Some(payload.turn_token_usage);
            }
            ControlFlow::Continue(())
        }
        EVENT_MSG => {
            let Ok(event) = serde_json::from_slice::<EventMsgLine>(line) else {
                return ControlFlow::Continue(());
            };
            if event.payload.kind == TASK_STARTED
                && event.payload.turn_id.as_deref() == Some(turn_id)
            {
                return ControlFlow::Break(StepOutcome::FoundStart);
            }
            ControlFlow::Continue(())
        }
        _ => ControlFlow::Continue(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::DEFAULT_SCAN_CAP_BYTES;
    use std::io::Write;

    /// A synthetic rollout line set modeled on the real shapes documented
    /// in `super`'s module header and in
    /// `crates/daemon/tests/fixtures/codex-rollout-token-usage.jsonl`
    /// (which itself has no `task_started`, so it cannot serve unmodified
    /// as a positive fixture for a start-bound reader).
    fn task_started(turn_id: &str) -> String {
        format!(
            r#"{{"type":"event_msg","payload":{{"type":"task_started","turn_id":"{turn_id}"}}}}"#
        )
    }

    fn task_complete(turn_id: &str) -> String {
        format!(
            r#"{{"type":"event_msg","payload":{{"type":"task_complete","turn_id":"{turn_id}"}}}}"#
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn usage_record(
        turn_id: &str,
        thread_id: &str,
        session_id: &str,
        input: i64,
        cached: i64,
        write: i64,
        output: i64,
        reasoning: i64,
        turn_input: i64,
        turn_cached: i64,
        turn_write: i64,
        turn_output: i64,
        turn_reasoning: i64,
    ) -> String {
        format!(
            r#"{{"type":"token_usage_record","payload":{{"turn_id":"{turn_id}","thread_id":"{thread_id}","session_id":"{session_id}","root_turn_id":"{turn_id}","response_id":"resp","usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"cache_write_input_tokens":{write},"output_tokens":{output},"reasoning_output_tokens":{reasoning},"total_tokens":{total}}},"turn_token_usage":{{"input_tokens":{turn_input},"cached_input_tokens":{turn_cached},"cache_write_input_tokens":{turn_write},"output_tokens":{turn_output},"reasoning_output_tokens":{turn_reasoning},"total_tokens":{turn_total}}},"thread_token_usage":{{"input_tokens":{turn_input},"cached_input_tokens":{turn_cached},"cache_write_input_tokens":{turn_write},"output_tokens":{turn_output},"reasoning_output_tokens":{turn_reasoning},"total_tokens":{turn_total}}}}}}}"#,
            total = input + output,
            turn_total = turn_input + turn_output,
        )
    }

    fn write_lines(lines: &[String]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(f, "{line}").unwrap();
        }
        f.flush().unwrap();
        f
    }

    const T1: &str = "turn-1";
    const T2: &str = "turn-2";
    const THREAD: &str = "thread-1";
    const SESSION: &str = "session-1";

    #[test]
    fn a_single_record_turn_is_measured_exactly() {
        let f = write_lines(&[
            task_started(T1),
            usage_record(T1, THREAD, SESSION, 100, 20, 10, 5, 2, 100, 20, 10, 5, 2),
            task_complete(T1),
        ]);
        let observed = observe_codex_turn(f.path(), SESSION, T1, DEFAULT_SCAN_CAP_BYTES);
        let usage = observed.measured().expect("single consistent record");
        assert_eq!(usage.records, 1);
        // input(100) - cached(20) - write(10) = 70 fresh input, + write(10)
        // cache_creation, + output(5) = fresh_tokens.
        assert_eq!(usage.fresh_tokens(), 70 + 10 + 5);
    }

    #[test]
    fn cache_semantics_guard_fresh_tokens_excludes_cache_reads() {
        let f = write_lines(&[
            task_started(T1),
            usage_record(
                T1, THREAD, SESSION, 1000, 900, 0, 10, 0, 1000, 900, 0, 10, 0,
            ),
        ]);
        let observed = observe_codex_turn(f.path(), SESSION, T1, DEFAULT_SCAN_CAP_BYTES);
        let usage = observed.measured().unwrap();
        assert_eq!(
            usage.fresh_tokens(),
            110,
            "900 of the 1000 input tokens were a cache read and must not count as fresh"
        );
    }

    #[test]
    fn interleaved_sibling_turns_are_never_cross_attributed() {
        // T1 and T2 interleaved by position (as if two lanes shared one
        // rollout file). Reading T1 must sum only T1's own records.
        let f = write_lines(&[
            task_started(T1),
            task_started(T2),
            usage_record(T1, THREAD, SESSION, 100, 0, 0, 10, 0, 100, 0, 0, 10, 0),
            usage_record(T2, THREAD, SESSION, 500, 0, 0, 50, 0, 500, 0, 0, 50, 0),
            usage_record(T1, THREAD, SESSION, 200, 0, 0, 20, 0, 300, 0, 0, 30, 0),
            task_complete(T1),
            task_complete(T2),
        ]);
        let observed = observe_codex_turn(f.path(), SESSION, T1, DEFAULT_SCAN_CAP_BYTES);
        let usage = observed
            .measured()
            .expect("T1's own two records are self-consistent");
        assert_eq!(usage.records, 2, "only T1's two records, never T2's");
        assert_eq!(
            usage.fresh_tokens(),
            300 + 30,
            "exactly T1's own total, not T1+T2"
        );
    }

    #[test]
    fn wrong_session_record_is_inconsistent() {
        let f = write_lines(&[
            task_started(T1),
            usage_record(T1, THREAD, "other-session", 10, 0, 0, 5, 0, 10, 0, 0, 5, 0),
        ]);
        let observed = observe_codex_turn(f.path(), SESSION, T1, DEFAULT_SCAN_CAP_BYTES);
        assert_eq!(
            observed,
            UsageObservation::Unavailable(UsageUnavailable::Inconsistent)
        );
    }

    #[test]
    fn a_second_distinct_thread_id_is_inconsistent() {
        let f = write_lines(&[
            task_started(T1),
            usage_record(T1, "thread-a", SESSION, 10, 0, 0, 5, 0, 10, 0, 0, 5, 0),
            usage_record(T1, "thread-b", SESSION, 10, 0, 0, 5, 0, 20, 0, 0, 10, 0),
        ]);
        let observed = observe_codex_turn(f.path(), SESSION, T1, DEFAULT_SCAN_CAP_BYTES);
        assert_eq!(
            observed,
            UsageObservation::Unavailable(UsageUnavailable::Inconsistent)
        );
    }

    #[test]
    fn two_sequential_turns_each_measure_only_their_own_records() {
        let f = write_lines(&[
            task_started(T1),
            usage_record(T1, THREAD, SESSION, 100, 0, 0, 10, 0, 100, 0, 0, 10, 0),
            task_complete(T1),
            task_started(T2),
            usage_record(T2, THREAD, SESSION, 50, 0, 0, 5, 0, 50, 0, 0, 5, 0),
            task_complete(T2),
        ]);
        let observed_t2 = observe_codex_turn(f.path(), SESSION, T2, DEFAULT_SCAN_CAP_BYTES);
        assert_eq!(observed_t2.measured().unwrap().fresh_tokens(), 55);
        let observed_t1 = observe_codex_turn(f.path(), SESSION, T1, DEFAULT_SCAN_CAP_BYTES);
        assert_eq!(observed_t1.measured().unwrap().fresh_tokens(), 110);
    }

    #[test]
    fn old_format_token_count_only_is_no_usage_in_window() {
        let f = write_lines(&[
            task_started(T1),
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{}}}"#.to_string(),
            task_complete(T1),
        ]);
        let observed = observe_codex_turn(f.path(), SESSION, T1, DEFAULT_SCAN_CAP_BYTES);
        assert_eq!(
            observed,
            UsageObservation::Unavailable(UsageUnavailable::NoUsageInWindow)
        );
    }

    #[test]
    fn token_count_mirror_alongside_a_real_record_is_not_double_counted() {
        let f = write_lines(&[
            task_started(T1),
            r#"{"type":"event_msg","payload":{"type":"token_count","info":{}}}"#.to_string(),
            usage_record(T1, THREAD, SESSION, 10, 0, 0, 5, 0, 10, 0, 0, 5, 0),
        ]);
        let observed = observe_codex_turn(f.path(), SESSION, T1, DEFAULT_SCAN_CAP_BYTES);
        let usage = observed.measured().unwrap();
        assert_eq!(usage.records, 1);
    }

    #[test]
    fn missing_task_started_before_start_of_file_is_turn_incomplete() {
        let f = write_lines(&[usage_record(
            T1, THREAD, SESSION, 10, 0, 0, 5, 0, 10, 0, 0, 5, 0,
        )]);
        let observed = observe_codex_turn(f.path(), SESSION, T1, DEFAULT_SCAN_CAP_BYTES);
        assert_eq!(
            observed,
            UsageObservation::Unavailable(UsageUnavailable::TurnIncomplete)
        );
    }

    #[test]
    fn exhausting_the_scan_cap_before_task_started_is_scan_cap_reached_not_turn_incomplete() {
        // The backward scan reads from EOF toward the start of the file, so
        // `task_started` must sit at the *start* (far from EOF) with enough
        // bytes after it that a small cap exhausts before the scan ever
        // reaches it -- putting it last would make it the very first line
        // read backward, never exercising the cap at all.
        let filler = "x".repeat(4_000);
        let mut lines = vec![task_started(T1)];
        for i in 0..50 {
            lines.push(format!(
                r#"{{"type":"event_msg","pad":"{filler}","payload":{{"type":"unrelated_{i}"}}}}"#
            ));
        }
        let f = write_lines(&lines);
        let observed = observe_codex_turn(f.path(), SESSION, T1, 256);
        assert_eq!(
            observed,
            UsageObservation::Unavailable(UsageUnavailable::ScanCapReached)
        );
    }

    #[test]
    fn inconsistent_sum_against_the_turns_own_running_total_is_caught() {
        // The record's own `turn_token_usage` claims a running total that
        // disagrees with the sum of the one matched record's `usage` --
        // exactly what an unflushed earlier record for this turn would
        // look like.
        let f = write_lines(&[
            task_started(T1),
            usage_record(T1, THREAD, SESSION, 10, 0, 0, 5, 0, 999, 0, 0, 999, 0),
        ]);
        let observed = observe_codex_turn(f.path(), SESSION, T1, DEFAULT_SCAN_CAP_BYTES);
        assert_eq!(
            observed,
            UsageObservation::Unavailable(UsageUnavailable::Inconsistent)
        );
    }

    #[test]
    fn cached_exceeding_input_is_inconsistent() {
        let f = write_lines(&[
            task_started(T1),
            usage_record(T1, THREAD, SESSION, 10, 20, 0, 5, 0, 10, 20, 0, 5, 0),
        ]);
        let observed = observe_codex_turn(f.path(), SESSION, T1, DEFAULT_SCAN_CAP_BYTES);
        assert_eq!(
            observed,
            UsageObservation::Unavailable(UsageUnavailable::Inconsistent)
        );
    }

    #[test]
    fn reasoning_exceeding_output_is_inconsistent() {
        let f = write_lines(&[
            task_started(T1),
            usage_record(T1, THREAD, SESSION, 10, 0, 0, 5, 20, 10, 0, 0, 5, 20),
        ]);
        let observed = observe_codex_turn(f.path(), SESSION, T1, DEFAULT_SCAN_CAP_BYTES);
        assert_eq!(
            observed,
            UsageObservation::Unavailable(UsageUnavailable::Inconsistent)
        );
    }

    #[test]
    fn real_serializer_nonzero_cache_write_is_measured_not_inconsistent() {
        // The deviation documented in this module's header: real
        // codex-cli 0.160.0 output has nonzero cache_write_input_tokens
        // (see crates/daemon/tests/fixtures/codex-rollout-token-usage.jsonl),
        // contradicting the ticket's premise that it is never observed
        // nonzero. This must measure, not score Inconsistent.
        let f = write_lines(&[
            task_started(T1),
            usage_record(
                T1, THREAD, SESSION, 1000, 600, 300, 50, 20, 1000, 600, 300, 50, 20,
            ),
        ]);
        let observed = observe_codex_turn(f.path(), SESSION, T1, DEFAULT_SCAN_CAP_BYTES);
        let usage = observed
            .measured()
            .expect("nonzero cache_write is legitimate fresh input");
        assert_eq!(
            usage.fresh_tokens(),
            450,
            "matches Codex's own blended_total for this record"
        );
    }

    #[test]
    fn missing_cache_write_field_defaults_to_zero() {
        let line = format!(
            r#"{{"type":"token_usage_record","payload":{{"turn_id":"{T1}","thread_id":"{THREAD}","session_id":"{SESSION}","root_turn_id":"{T1}","response_id":"resp","usage":{{"input_tokens":10,"cached_input_tokens":0,"output_tokens":5,"reasoning_output_tokens":0,"total_tokens":15}},"turn_token_usage":{{"input_tokens":10,"cached_input_tokens":0,"output_tokens":5,"reasoning_output_tokens":0,"total_tokens":15}}}}}}"#
        );
        let f = write_lines(&[task_started(T1), line]);
        let observed = observe_codex_turn(f.path(), SESSION, T1, DEFAULT_SCAN_CAP_BYTES);
        let usage = observed
            .measured()
            .expect("absent field is not a parse failure");
        assert_eq!(usage.fresh_tokens(), 15);
    }

    #[test]
    fn a_record_straddling_a_chunk_boundary_is_counted_once() {
        let filler = "x".repeat(4_000);
        let mut lines = vec![task_started(T1)];
        for i in 0..100 {
            lines.push(format!(
                r#"{{"type":"event_msg","pad":"{filler}","payload":{{"type":"noop_{i}"}}}}"#
            ));
        }
        lines.push(usage_record(
            T1, THREAD, SESSION, 10, 0, 0, 5, 0, 10, 0, 0, 5, 0,
        ));
        let f = write_lines(&lines);
        assert!(
            std::fs::metadata(f.path()).unwrap().len() > 256 * 1024,
            "the fixture must actually span more than one chunk or it tests nothing"
        );
        let observed = observe_codex_turn(f.path(), SESSION, T1, DEFAULT_SCAN_CAP_BYTES);
        let usage = observed.measured().unwrap();
        assert_eq!(usage.records, 1, "parsed once, not zero or twice");
        assert_eq!(usage.fresh_tokens(), 15);
    }

    #[test]
    fn a_half_written_final_line_is_inconsistent_not_silently_undercounted() {
        // Without the trailing-newline pre-check, this would simply fail
        // `TaggedLine`'s parse and be skipped, leaving the single
        // complete record's own `turn_token_usage` as "newest" -- which
        // then spuriously agrees with itself and returns Measured(10..)
        // instead of catching that the turn's real newest record never
        // made it to disk.
        let complete_lines = [
            task_started(T1),
            usage_record(T1, THREAD, SESSION, 10, 0, 0, 5, 0, 10, 0, 0, 5, 0),
        ];
        // The turn's real newest record, truncated mid-write (no closing
        // `}` and, critically, no trailing newline).
        let truncated_tail = r#"{"type":"token_usage_record","payload":{"turn_id":"turn-1""#;
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        for line in &complete_lines {
            writeln!(f, "{line}").unwrap();
        }
        write!(f, "{truncated_tail}").unwrap();
        f.flush().unwrap();

        let observed = observe_codex_turn(f.path(), SESSION, T1, DEFAULT_SCAN_CAP_BYTES);
        assert_eq!(
            observed,
            UsageObservation::Unavailable(UsageUnavailable::Inconsistent)
        );
    }

    #[test]
    fn empty_file_is_treated_as_ending_in_newline() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let observed = observe_codex_turn(f.path(), SESSION, T1, DEFAULT_SCAN_CAP_BYTES);
        // An empty file has no task_started either, so this still reports
        // TurnIncomplete -- the point of this test is only that the
        // trailing-newline check itself does not misfire on an empty file.
        assert_eq!(
            observed,
            UsageObservation::Unavailable(UsageUnavailable::TurnIncomplete)
        );
    }

    #[test]
    fn a_right_turn_record_with_a_malformed_field_is_inconsistent_not_skipped() {
        // thread_id is a number instead of a string: the full
        // TokenUsageRecordLine parse fails, but the narrow turn_id still
        // matches T1, so this must be Inconsistent, not silently skipped
        // (which would let a cross-attributed or truncated record through
        // undetected).
        let malformed = format!(
            r#"{{"type":"token_usage_record","payload":{{"turn_id":"{T1}","thread_id":123,"session_id":"{SESSION}","usage":{{"input_tokens":10,"output_tokens":5}},"turn_token_usage":{{"input_tokens":10,"output_tokens":5}}}}}}"#
        );
        let f = write_lines(&[task_started(T1), malformed]);
        let observed = observe_codex_turn(f.path(), SESSION, T1, DEFAULT_SCAN_CAP_BYTES);
        assert_eq!(
            observed,
            UsageObservation::Unavailable(UsageUnavailable::Inconsistent)
        );
    }

    #[test]
    fn a_malformed_record_for_a_different_turn_is_skipped_not_inconsistent() {
        // Same malformed shape as above, but for T2 -- irrelevant to a
        // T1 read, so it must not affect the outcome at all.
        let malformed = format!(
            r#"{{"type":"token_usage_record","payload":{{"turn_id":"{T2}","thread_id":123,"session_id":"{SESSION}","usage":{{"input_tokens":10,"output_tokens":5}},"turn_token_usage":{{"input_tokens":10,"output_tokens":5}}}}}}"#
        );
        let f = write_lines(&[
            task_started(T1),
            malformed,
            usage_record(T1, THREAD, SESSION, 10, 0, 0, 5, 0, 10, 0, 0, 5, 0),
        ]);
        let observed = observe_codex_turn(f.path(), SESSION, T1, DEFAULT_SCAN_CAP_BYTES);
        let usage = observed
            .measured()
            .expect("the malformed T2 line is irrelevant to T1");
        assert_eq!(usage.records, 1);
    }

    #[test]
    fn root_turn_id_equal_to_parent_turn_id_is_never_used_for_matching() {
        // Every other fixture in this module sets root_turn_id == turn_id,
        // which leaves this reader's "root_turn_id is never read" claim
        // untested: a reader that accidentally matched on root_turn_id
        // would pass every other test here. This fixture gives a subagent
        // record a root_turn_id equal to the *parent's* turn_id while its
        // own turn_id is T2, so a reader that matched on root_turn_id
        // would wrongly pull it into a T1 read.
        let subagent_record = format!(
            r#"{{"type":"token_usage_record","payload":{{"turn_id":"{T2}","thread_id":"{THREAD}","session_id":"{SESSION}","root_turn_id":"{T1}","response_id":"resp","usage":{{"input_tokens":500,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":50,"reasoning_output_tokens":0,"total_tokens":550}},"turn_token_usage":{{"input_tokens":500,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":50,"reasoning_output_tokens":0,"total_tokens":550}},"thread_token_usage":{{"input_tokens":500,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":50,"reasoning_output_tokens":0,"total_tokens":550}}}}}}"#
        );
        let f = write_lines(&[
            task_started(T1),
            usage_record(T1, THREAD, SESSION, 100, 0, 0, 10, 0, 100, 0, 0, 10, 0),
            subagent_record,
        ]);
        let observed = observe_codex_turn(f.path(), SESSION, T1, DEFAULT_SCAN_CAP_BYTES);
        let usage = observed
            .measured()
            .expect("T1's own record is self-consistent regardless of the subagent line");
        assert_eq!(
            usage.records, 1,
            "the subagent's record (turn_id T2, root_turn_id T1) must never be counted toward T1"
        );
        assert_eq!(usage.fresh_tokens(), 110);
    }

    /// Characterization (HORO-2282): Codex writes `usage` per response and
    /// `turn_token_usage` as a running total (codex-rs state/session.rs,
    /// 0.163 to 0.165). The turn total is the sum of `usage`, never the sum
    /// of the cumulative field.
    #[test]
    fn cumulative_turn_totals_are_not_summed_across_records() {
        let f = write_lines(&[
            task_started(T1),
            usage_record(T1, THREAD, SESSION, 100, 0, 0, 10, 0, 100, 0, 0, 10, 0),
            usage_record(T1, THREAD, SESSION, 200, 0, 0, 20, 0, 300, 0, 0, 30, 0),
            usage_record(T1, THREAD, SESSION, 50, 0, 0, 5, 0, 350, 0, 0, 35, 0),
        ]);
        let observed = observe_codex_turn(f.path(), SESSION, T1, DEFAULT_SCAN_CAP_BYTES);
        let usage = observed.measured().expect("three consistent records");
        assert_eq!(usage.records, 3);
        assert_eq!(
            usage.fresh_tokens(),
            350 + 35,
            "the last record's running total, not 100+300+350 input"
        );
    }

    /// Characterization (HORO-2282): codex-rs writes a record for an
    /// incomplete or failed response that carries a `response_id`, and it
    /// rolls into the turn's running total. The fixture cannot mark a record
    /// as incomplete, so this pins only that a low-output first record is
    /// summed with the later ones and read against the running total. It
    /// documents current behavior and changes only deliberately.
    #[test]
    fn a_first_record_with_little_output_is_summed_with_the_turn() {
        let f = write_lines(&[
            task_started(T1),
            usage_record(T1, THREAD, SESSION, 40, 0, 0, 0, 0, 40, 0, 0, 0, 0),
            usage_record(T1, THREAD, SESSION, 100, 0, 0, 10, 0, 140, 0, 0, 10, 0),
        ]);
        let observed = observe_codex_turn(f.path(), SESSION, T1, DEFAULT_SCAN_CAP_BYTES);
        let usage = observed.measured().expect("partial then complete response");
        assert_eq!(usage.records, 2);
        assert_eq!(usage.fresh_tokens(), 140 + 10);
    }

    /// Characterization (HORO-2282): the reader does not de-duplicate by
    /// `response_id`. A replayed or concatenated record makes the summed
    /// usage exceed the turn's running total, so the read is refused (the
    /// caller falls back to the flat hold) instead of double charging.
    #[test]
    fn a_duplicated_record_is_inconsistent_not_double_charged() {
        let record = usage_record(T1, THREAD, SESSION, 100, 0, 0, 10, 0, 100, 0, 0, 10, 0);
        let f = write_lines(&[task_started(T1), record.clone(), record]);
        let observed = observe_codex_turn(f.path(), SESSION, T1, DEFAULT_SCAN_CAP_BYTES);
        assert_eq!(
            observed,
            UsageObservation::Unavailable(UsageUnavailable::Inconsistent)
        );
    }

    /// Characterization (HORO-2282): an aborted or stream-failed request
    /// writes no `token_usage_record` (codex-rs, 0.163 to 0.165), so an
    /// interrupted turn with no completed response has no usage to read.
    #[test]
    fn an_interrupted_turn_without_any_record_has_no_usage_in_window() {
        let aborted = format!(
            r#"{{"type":"event_msg","payload":{{"type":"turn_aborted","turn_id":"{T1}"}}}}"#
        );
        let f = write_lines(&[task_started(T1), aborted]);
        let observed = observe_codex_turn(f.path(), SESSION, T1, DEFAULT_SCAN_CAP_BYTES);
        assert_eq!(
            observed,
            UsageObservation::Unavailable(UsageUnavailable::NoUsageInWindow)
        );
    }

    /// Characterization (HORO-2282), a KNOWN under-count: when a turn is
    /// interrupted after some responses completed, the aborted request left
    /// no record, so the read is `Measured` over the recorded responses
    /// only. The reader cannot see the missing spend. This test pins that
    /// limit so Codex coverage is never claimed complete for interrupts.
    #[test]
    fn an_interrupted_turn_measures_only_the_recorded_responses() {
        let aborted = format!(
            r#"{{"type":"event_msg","payload":{{"type":"turn_aborted","turn_id":"{T1}"}}}}"#
        );
        let f = write_lines(&[
            task_started(T1),
            usage_record(T1, THREAD, SESSION, 100, 0, 0, 10, 0, 100, 0, 0, 10, 0),
            aborted,
        ]);
        let observed = observe_codex_turn(f.path(), SESSION, T1, DEFAULT_SCAN_CAP_BYTES);
        let usage = observed
            .measured()
            .expect("the recorded response is consistent");
        assert_eq!(usage.records, 1);
        assert_eq!(usage.fresh_tokens(), 110);
    }

    /// Characterization (HORO-2282): a Stop-block continuation writes more
    /// records for the same `turn_id` after an earlier `task_complete`. The
    /// reader ignores `task_complete`, so they are counted.
    #[test]
    fn records_written_after_a_task_complete_for_the_same_turn_are_counted() {
        let f = write_lines(&[
            task_started(T1),
            usage_record(T1, THREAD, SESSION, 100, 0, 0, 10, 0, 100, 0, 0, 10, 0),
            task_complete(T1),
            usage_record(T1, THREAD, SESSION, 200, 0, 0, 20, 0, 300, 0, 0, 30, 0),
        ]);
        let observed = observe_codex_turn(f.path(), SESSION, T1, DEFAULT_SCAN_CAP_BYTES);
        let usage = observed.measured().expect("continuation records agree");
        assert_eq!(usage.records, 2);
        assert_eq!(usage.fresh_tokens(), 300 + 30);
    }
}
