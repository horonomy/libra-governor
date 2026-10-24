//! `libra-governor hook pre-tool-use` -- the Claude Code `PreToolUse` hook
//! entry point (HORO-2215).
//!
//! A thin caller into `crate::agent::run::run_tool_requested`. Prints
//! output only to deny a tool call because the local shared budget is
//! exhausted or invalid; every other outcome is silent (allow).

use crate::agent::{run, AgentKind};

pub fn run() {
    run::run_tool_requested(AgentKind::ClaudeCode);
}
