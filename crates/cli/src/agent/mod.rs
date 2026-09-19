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
/// Re-exported from `libra_governor_domain` so the capability matrix
/// (`libra-governor agents`) and the hook translation layer always agree
/// on "which agent" — one enum, not two independently-maintained copies.
pub use libra_governor_domain::AgentKind;

/// Short label used only in log lines — never in the
/// `hookSpecificOutput` stdout contract itself, which stays
/// host-agnostic text (see [`render`]).
pub fn label(agent: AgentKind) -> &'static str {
    match agent {
        AgentKind::ClaudeCode => "claude-code",
        AgentKind::Codex => "codex",
    }
}
