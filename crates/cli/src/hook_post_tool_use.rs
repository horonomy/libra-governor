//! `libra-governor hook post-tool-use` — the Claude Code `PostToolUse`
//! hook entry point.
//!
//! A thin caller into the shared translation layer
//! (`crate::agent::run::run_tool_completed`, HORO-1157) — see that
//! module's docs for the full stdin contract. Behavior is byte-identical
//! to before HORO-1157's extraction.

use crate::agent::{run, AgentKind};

pub fn run() {
    run::run_tool_completed(AgentKind::ClaudeCode);
}
