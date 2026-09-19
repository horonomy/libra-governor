//! `libra-governor codex-hook user-prompt-submit` / `post-tool-use` /
//! `stop` — the Codex CLI hook entry points (HORO-1157).
//!
//! Thin callers into the same shared translation layer Claude Code's
//! `hook.rs`/`hook_post_tool_use.rs`/`hook_stop.rs` use
//! (`crate::agent::run`), passing `AgentKind::Codex`. Codex's real hook
//! payload schemas name the same fields Claude Code's do for these three
//! events (see `docs/adr/0004-agent-adapter-contract.md`), so no
//! Codex-specific parsing exists — the only difference from the Claude
//! entry points is which agent label ends up in the daemon log.

use crate::agent::{run, AgentKind};

pub fn run_prompt_submit() {
    run::run_prompt_submit(AgentKind::Codex);
}

pub fn run_tool_completed() {
    run::run_tool_completed(AgentKind::Codex);
}

pub fn run_turn_completed() {
    run::run_turn_completed(AgentKind::Codex);
}
