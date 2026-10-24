//! `libra-governor hook subagent-stop` -- the Claude Code `SubagentStop` hook
//! entry point (HORO-2216).
//!
//! A thin caller into `crate::agent::settlement`. Prints nothing, never
//! blocks, never exits nonzero: it asks the daemon to charge the subagent's
//! transcript to the local shared budget, and only when
//! `shared-budget.json` exists.

use crate::agent::{settlement, AgentKind};

pub fn run() {
    settlement::run_subagent_stop(AgentKind::ClaudeCode);
}
