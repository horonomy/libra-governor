# Governed agent integrations

Libra currently governs two coding agent hosts:

- [Claude Code](claude-code/README.md)
- [Codex CLI](codex/README.md)

Both share every byte of hook-translation logic
(`crates/cli/src/agent/`) — see
[`docs/adr/0004-agent-adapter-contract.md`](../docs/adr/0004-agent-adapter-contract.md)
for why that is a shared layer rather than a trait or a separate crate
per agent, and exactly what was verified against each host's real hook
schemas.

## Capability matrix

**Source of truth:** `libra-governor agents --json` — this table is a
human-readable rendering of that command's output
(`libra_governor_domain::AgentCapabilities::for_agent`), not a
separately-maintained claim. If this table and `agents --json` ever
disagree, trust the command.

| Capability | Claude Code | Codex |
|---|---|---|
| Preflight gate (`UserPromptSubmit`) | Available | Available |
| Tool observation (`PostToolUse`) | Available | Available |
| Tool gate (hard pre-call refusal) | Unavailable — MVP 1.0 hooks are advisory; the gateway is the only hard-enforcement point (ADR 0003) | Unavailable — same reason |
| Shared budget admission gate (`UserPromptSubmit` block, `PreToolUse` deny; ADR-0026) | Available — cooperative, admission-bounded, Libra-accounted USD; only acts when `shared-budget.json` exists | **Unavailable** — not wired by Libra: the Codex hook route never asks the daemon to admit a prompt and has no budget gate |
| Completion receipt (`Stop`) | Available | Available |
| Model event observation (token/cost/provider per request) | Unavailable — no hook payload exposes this | Unavailable — same |
| Model gateway | Available | **Unavailable — incompatible wire format** (`responses` vs. `messages`) |
| Hard budget enforcement | Available | Unavailable — same wire-format incompatibility |
| Persistent status surface (statusline) | Available | Unavailable — no statusline key in Codex's config schema |
| Inline explanation channel (`hookSpecificOutput.additionalContext`) | Available | Available |
| Session lifecycle (`SessionStart`/`SessionEnd`) | Unavailable — not wired by Libra yet | Unavailable — not wired by Libra yet |
| Subagent lifecycle (`SubagentStart`/`SubagentStop`) | Unavailable — not wired by Libra yet | Unavailable — not wired by Libra yet |
| Interruption signal | Unavailable — host exposes no `Interrupt`/cancel hook event | Unavailable — Codex exposes `Interrupt`, but it is not wired by Libra yet |
| MCP explain surface | Unavailable — not wired by Libra yet | Unavailable — not wired by Libra yet |

Every "Unavailable" row names a concrete reason (a missing host
primitive, a deliberate Libra scoping decision, or a real
incompatibility) rather than a bare "no" — see
`libra_governor_domain::agent::CapabilityGap`'s three variants for the
taxonomy, and `AgentCapabilities::for_agent`'s doc comment for why this
is a pure function of agent identity, never a runtime probe.

## Non-goals shared by both integrations

- No general hard tool-call gating from hooks — the gateway is the only
  hard pre-spend enforcement point (ADR 0003), and only Claude Code's
  gateway path is even wire-compatible today. The one hook-level
  exception is the local shared budget (ADR-0026, Claude Code only):
  `UserPromptSubmit` may block and `PreToolUse` may deny, solely on
  budget state, and the guarantee is cooperative and admission-bounded,
  never a hard cap.
- No MCP-based enforcement — `ARCHITECTURE.md` forbids MCP as a security
  or enforcement boundary for any agent.
- No daemon/protocol/ledger wiring for session or subagent lifecycle
  events yet — both hosts expose the underlying hook events; neither is
  wired to a daemon action.

See each agent's own README for what is fully in scope for that host.
