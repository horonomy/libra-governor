//! [`NormalizedEvent`] — the host-agnostic result of translating one raw
//! hook payload (HORO-1157).
//!
//! Both Claude Code and Codex feed the same shape of raw JSON on stdin
//! for a given hook entry point (see `agent::payload`);
//! [`crate::agent::normalize::normalize`] turns it into exactly one of
//! the variants below, and `crate::agent::run` is the only place that
//! acts on it.

use std::path::PathBuf;

/// One normalized hook event, payload-carrying where the daemon needs
/// data from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NormalizedEvent {
    /// `UserPromptSubmit`.
    PromptSubmitted {
        session_id: String,
        cwd: PathBuf,
        prompt: String,
    },
    /// `PostToolUse`.
    ToolCompleted {
        session_id: String,
        tool_name: String,
    },
    /// `Stop`.
    TurnCompleted {
        session_id: String,
        model: Option<String>,
    },
    /// A hook event name this integration recognizes as a genuine agent
    /// lifecycle event (`SessionStart`, `SessionEnd`, `SubagentStart`,
    /// `SubagentStop`, `Interrupt`, `PreCompact`, `PostCompact`), but
    /// deliberately does not wire to any daemon action in MVP 1 — see
    /// the HORO-1157 non-goals. Never causes an error; the daemon simply
    /// is not called.
    RecognizedUnwired { hook_event_name: String },
    /// A hook event name that is neither a wired entry point nor a
    /// recognized-but-unwired lifecycle event — forward compatibility:
    /// an unknown event name is a value this function returns, never a
    /// panic or an `Err`.
    Unrecognized { hook_event_name: String },
}
