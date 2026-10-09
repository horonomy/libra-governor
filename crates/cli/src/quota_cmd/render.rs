//! Text/JSON rendering for `libra-governor quota explain` (HORO-1767).
//!
//! Every pacing-derived text line is prefixed `[SIM] `; the output never
//! contains an ANSI escape byte (`\x1b`); and a status token (`STL`,
//! `UNK`, `BLOCK`, ...) is never truncated or dropped. Every line is
//! built from a short *required* portion (scope + short id + fixed
//! status code — always within a few characters of [`MIN_WIDTH`], never
//! longer) plus an *optional* free-text/detail suffix (a full id, a
//! note, a value) that [`with_optional_suffix`] appends only when the
//! combined line still fits `width`, ellipsizing it by characters (never
//! bytes) if it needs to shrink, and dropping it entirely rather than
//! ever slicing into the required portion. This is what makes "below 60
//! columns, compact; above it, fuller" an emergent property of the width
//! budget, not a second hand-written code path to keep in sync.

use std::fmt::Write as _;

use super::view::{
    BlockingView, Explanation, Figure, FigureKind, Measured, ModeView, NextView, ReliefView,
    TaskView, WindowView,
};

/// No line is ever built narrower than this — below it, even the short
/// required codes could not reliably fit, so the caller-chosen width is
/// clamped up rather than risking an unreadable or inconsistent render.
pub const MIN_WIDTH: usize = 24;

pub fn clamp_width(width: usize) -> usize {
    width.max(MIN_WIDTH)
}

pub fn render_json(explanation: &Explanation) -> String {
    serde_json::to_string_pretty(explanation).unwrap_or_else(|e| {
        format!(
            "{{\"schema_version\":\"{}\",\"error\":\"serialization_failed: {e}\"}}",
            explanation.schema_version
        )
    })
}

/// Shortens `s` to at most `max_chars` *characters* (never bytes — a
/// byte-offset slice can panic mid-UTF-8-codepoint) by keeping a prefix
/// and appending a single ellipsis character.
fn ellipsize(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    let keep = max_chars.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

/// Appends an optional suffix to `required` only if the combined line
/// (`"[SIM] " + required + " " + suffix`) still fits within `width`;
/// otherwise the suffix is ellipsized to fit, or dropped entirely if
/// there is no room at all — `required` itself is never touched, so a
/// status code inside it is never truncated.
fn with_optional_suffix(required: &str, suffix: &str, width: usize) -> String {
    if suffix.is_empty() {
        return required.to_string();
    }
    let prefix_len = "[SIM] ".chars().count();
    let required_len = required.chars().count();
    let used = prefix_len + required_len + 1; // +1 for the separating space
    if used >= width {
        return required.to_string();
    }
    let budget = width - used;
    if budget == 0 {
        return required.to_string();
    }
    format!("{required} {}", ellipsize(suffix, budget))
}

/// Strips every C0 control byte (0x00-0x1F) and DEL (0x7F) from `s` —
/// applied to every line's fully composed text immediately before it is
/// written, as a single choke point rather than at each individual
/// field. This surface renders caller-supplied free text straight from
/// a `--replay` fixture (a task's principal, a policy's name, ...) with
/// no character restriction of its own; without this, a crafted fixture
/// could inject a terminal escape sequence (ESC, CSI, OSC — cursor
/// movement, a screen clear, an OSC title spoof) or a bare `\n`/`\r`
/// that forges a fake additional `[SIM]`-prefixed line into this
/// process's stdout. Stripping (never just escaping) is deliberate: a
/// partial sequence surviving width-based ellipsizing (e.g. the `2J` in
/// a clear-screen sequence with its leading ESC cut off) is still
/// rendered harmless once every control byte, including an ESC
/// anywhere else on the line, is gone — the dangerous part of any such
/// sequence is always a control byte, never the printable bytes around
/// it. JSON output is unaffected by this function (and does not need
/// it): `serde_json` already escapes every control byte, including ESC,
/// as a textual `\u00XX` sequence, so a raw control byte never reaches
/// JSON-mode stdout either.
fn sanitize_for_terminal(s: &str) -> String {
    s.chars()
        .filter(|c| {
            let code = *c as u32;
            !(code < 0x20 || code == 0x7F)
        })
        .collect()
}

fn push_line(out: &mut String, width: usize, required: &str, suffix: &str) {
    let body = with_optional_suffix(required, suffix, width);
    let safe_body = sanitize_for_terminal(&body);
    let _ = writeln!(out, "[SIM] {safe_body}");
}

/// First 8 characters of an id/label — short enough that `scope id`
/// alone always fits comfortably within [`MIN_WIDTH`].
fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn blocking_code(b: &BlockingView) -> &'static str {
    match b {
        BlockingView::Blocking => "BLOCK",
        BlockingView::NotBlocking => "OK",
        BlockingView::Indeterminate { .. } => "UNK",
    }
}

fn kind_code(kind: FigureKind) -> &'static str {
    match kind {
        FigureKind::QuotaSnapshot => "QS",
        FigureKind::Actual => "ACT",
        FigureKind::Hold => "HLD",
        FigureKind::Forecast => "FCT",
    }
}

fn measured_code<T>(m: &Measured<T>) -> &'static str {
    match m {
        Measured::Exact { .. } => "EX",
        Measured::UpperBound { .. } => "UB",
        Measured::Unknown { .. } => "UNK",
        Measured::Stale { .. } => "STL",
    }
}

fn measured_detail<T: std::fmt::Debug>(m: &Measured<T>) -> String {
    match m {
        Measured::Exact { value } => format!("{value:?}"),
        Measured::UpperBound { value, note } => format!("<={value:?} ({note})"),
        Measured::Unknown { reason } => reason.clone(),
        Measured::Stale { age_secs, .. } => {
            age_secs.map(|a| format!("age={a}s")).unwrap_or_default()
        }
    }
}

fn render_figure_line<T: std::fmt::Debug>(
    out: &mut String,
    width: usize,
    scope: &str,
    figure: &Figure<T>,
) {
    let required = format!(
        "{scope} {}={}",
        kind_code(figure.kind),
        measured_code(&figure.measured)
    );
    push_line(out, width, &required, &measured_detail(&figure.measured));
}

fn relief_required_and_detail(relief: &ReliefView) -> (&'static str, String) {
    match relief {
        ReliefView::NotBlocking => ("REL=NONE", String::new()),
        ReliefView::Computed { at } => ("REL=COMPUTED", at.clone()),
        ReliefView::ProviderClaim { at } => ("REL=PROVIDER_CLAIM", at.clone()),
        ReliefView::AfterHoldsSettle => ("REL=AFTER_HOLDS", String::new()),
        ReliefView::Unknown => ("REL=UNK", String::new()),
    }
}

fn render_window(out: &mut String, width: usize, scope: &str, w: &WindowView) {
    let required = format!(
        "{scope} w={} b={}",
        short_id(&w.window_id),
        blocking_code(&w.blocking)
    );
    let detail = match &w.blocking {
        BlockingView::Indeterminate { reason } => format!("unit={} {reason}", w.unit),
        _ => format!("unit={}", w.unit),
    };
    push_line(out, width, &required, &detail);

    render_figure_line(out, width, scope, &w.snapshot);
    render_figure_line(out, width, scope, &w.actual);
    render_figure_line(out, width, scope, &w.held);
    render_figure_line(out, width, scope, &w.remaining);

    let (relief_required, relief_detail) = relief_required_and_detail(&w.relief);
    let required = format!("{scope} {relief_required}");
    push_line(out, width, &required, &relief_detail);
}

fn render_task(out: &mut String, width: usize, t: &TaskView) {
    let required = format!("T id={} scope=TASK", t.task);
    push_line(
        out,
        width,
        &required,
        &format!("princ={}", short_id(&t.principal)),
    );

    render_figure_line(out, width, "T", &t.hold);
    render_figure_line(out, width, "T", &t.actual);
    render_figure_line(out, width, "T", &t.need_range);

    let required = "T CEIL=KNOWN".to_string();
    push_line(
        out,
        width,
        &required,
        &format!("{:?} policy={}", t.ceiling.value, t.ceiling.policy_name),
    );
}

/// Renders the full human-readable explain report, clamped to `width`
/// columns (see [`clamp_width`]) and never containing an ANSI escape
/// byte.
pub fn render_human(explanation: &Explanation, requested_width: usize) -> String {
    let width = clamp_width(requested_width);
    let mut out = String::new();

    push_line(&mut out, width, "quota-explain", &explanation.as_of);

    let mode_required = match &explanation.mode.value {
        ModeView::Sustain { .. } => "MODE=SUSTAIN".to_string(),
        ModeView::Burst { max_fanout, .. } => format!("MODE=BURST fanout={max_fanout}"),
    };
    push_line(&mut out, width, &mode_required, "");

    let next_code = match &explanation.next_safe_action.value {
        NextView::Now => "NOW",
        NextView::At { .. } => "AT",
        NextView::Unknown { .. } => "UNK",
    };
    let next_required = format!("NEXT={next_code}");
    let next_detail = match &explanation.next_safe_action.value {
        NextView::At { at } => at.clone(),
        NextView::Unknown { reason } => reason.clone(),
        NextView::Now => String::new(),
    };
    push_line(&mut out, width, &next_required, &next_detail);

    let binding_required = match &explanation.binding_window.value {
        Some(id) => format!("BIND={}", short_id(id)),
        None => "BIND=NONE".to_string(),
    };
    push_line(&mut out, width, &binding_required, "");

    let counts = format!(
        "ACTIVE={} CAP={}",
        explanation.active_tasks, explanation.configured_cap
    );
    push_line(&mut out, width, &counts, "");

    for w in &explanation.host {
        render_window(&mut out, width, "H", w);
    }
    for w in &explanation.principal {
        render_window(&mut out, width, "P", w);
    }
    for t in &explanation.tasks {
        render_task(&mut out, width, t);
    }

    out
}
