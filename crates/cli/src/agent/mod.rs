//! Shared multi-agent hook translation layer (HORO-1157).
//!
//! `crates/protocol` and `crates/daemon`'s `handle_preflight`/
//! `handle_tool_invoked`/`handle_finalize` are already agent-neutral —
//! no Claude-specific field or tool-name vocabulary exists anywhere in
//! those crates. Codex's real hook payloads use the same field names
//! Claude Code's do (see `docs/adr/0004-agent-adapter-contract.md`), so
//! the adapter boundary is entirely inside this crate: one set of
//! payload structs ([`payload`]), one normalized event vocabulary
//! ([`event`]), one total parsing function ([`normalize`]), one shared
//! implementation ([`run`]) both hosts' thin entry points call, and the
//! same rendering helpers ([`render`]) either host's output is built
//! from. No new trait, no plugin abstraction — see the ADR for why that
//! would be over-engineering given the two hosts' verified payload
//! compatibility.

pub mod event;
pub mod normalize;
pub mod payload;
pub mod render;
pub mod run;

/// Which governed coding agent host is calling into this shared layer.
/// A local, cli-scoped enum: `crates/cli/src/hook.rs` only ever needs
/// `ClaudeCode` until `crates/cli/src/codex_hook.rs` exists, at which
/// point this type gains a `Codex` variant and (from then on) is reused
/// as-is by `libra_governor_domain::AgentKind` — see that type's docs
/// (HORO-1157) for the capability-matrix side of "which agent".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKind {
    ClaudeCode,
}

impl AgentKind {
    /// Short label used only in log lines — never in the
    /// `hookSpecificOutput` stdout contract itself, which stays
    /// host-agnostic text (see `render`).
    pub fn label(self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => "claude-code",
        }
    }
}
