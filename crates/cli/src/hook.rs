//! `libra-governor hook user-prompt-submit` — the Claude Code
//! `UserPromptSubmit` hook entry point.
//!
//! A thin caller into the shared translation layer
//! (`crate::agent::run::run_prompt_submit`, HORO-1157) — see that
//! module's docs for the full stdin/stdout contract. Behavior is
//! byte-identical to before HORO-1157's extraction; see
//! `crates/cli/tests/agent_contract.rs` for the regression proof.

use crate::agent::{run, AgentKind};

pub fn run() {
    run::run_prompt_submit(AgentKind::ClaudeCode);
}
