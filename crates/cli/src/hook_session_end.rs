//! `libra-governor hook session-end` -- the Claude Code `SessionEnd` hook
//! entry point (HORO-2216).
//!
//! A thin caller into `crate::agent::settlement`. Prints nothing, never
//! blocks, never exits nonzero: it asks the daemon to settle the tail of the
//! session's main transcript, and only when `shared-budget.json` exists.

use crate::agent::{settlement, AgentKind};

pub fn run() {
    settlement::run_session_end(AgentKind::ClaudeCode);
}
