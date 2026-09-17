# ADR 0001: Initial Architecture — Daemon/Hooks/Gateway/MCP Boundary

- **Status:** Accepted
- **Date:** 2026-09-17
- **Ticket:** HORO-1118

## Context

Libra needs a clear, defensible component boundary from day one, before
any product logic is written, so that later tickets (estimator, ledger,
admission policy) have an unambiguous place to live and an unambiguous
security model to respect.

The product North Star is:

> Never start work you are unlikely to afford to finish.

with the supporting invariant: estimate the full Definition of Done
before admission, reserve enough resources to finish, and re-plan only
when the expected benefit of replanning exceeds its own cost, switching
cost, and delay.

Libra is explicitly not a generic token counter, not a generic LLM
router/gateway, and not another coding-agent chat UI (see `PRODUCT.md`).
Existing agent UIs (Claude Code, Codex) remain the host UX.

## Decision

The system is split into five components with non-overlapping
responsibilities:

```
Claude Code -> deterministic hooks/statusline -> local Governor daemon -> optional enforcement gateway -> LLM provider
```

1. **Daemon** — the source-of-truth state machine, ledger, and policy
   engine. All admission decisions, spend accounting, and policy
   evaluation happen here and only here.
2. **Hooks** (`integrations/claude-code`) — deterministic admission/tool/
   replan lifecycle gates wired into the host agent tool. Thin adapters;
   no policy logic of their own.
3. **Gateway (optional)** — the hard enforcement boundary for provider
   spend, used only where LLM traffic can be routed through it.
   Enforcement, not decision-making.
4. **MCP** — explain/query/manual-control API only. Never a security or
   enforcement boundary; nothing safety-critical may depend solely on
   MCP being reachable.
5. **Skills/commands and statusline/system messages** — UX and
   explanation layers respectively. No independent state, no policy
   logic.

Additionally: the local data plane is Rust + SQLite (WAL mode), and full
prompt/source/tool output remain local by default (privacy invariant) —
the gateway only ever sees the traffic explicitly routed through it, for
spend enforcement, not content inspection.

## Consequences

- Any future feature proposal must state which of these five components
  it belongs to. A feature that needs MCP to be an enforcement boundary
  is a sign the design is wrong, not a reason to special-case MCP.
- The gateway remains optional; Libra must be fully useful (preflight
  estimation, ledger, hooks) with no gateway deployed at all.
- This boundary is a prerequisite for HORO-1124 and later tickets that
  implement real domain logic inside `crates/daemon`, `crates/ledger`,
  and `crates/estimator` — those tickets build inside these boundaries
  rather than re-litigating them.

## Alternatives considered

- **A single monolithic daemon with no separate gateway concept**:
  rejected because provider-spend enforcement needs a boundary that can
  sit in the actual network path, which a purely local daemon cannot
  guarantee for all deployments.
- **MCP as an enforcement surface** (e.g. gating tool calls through MCP
  responses): rejected because MCP reachability and semantics are not
  guaranteed to be a trustworthy or synchronous control point across all
  host tools; treating it as enforcement would make security depend on
  a UX-facing protocol.
- **Sending full prompt/tool-output content off-machine by default** (to
  power richer server-side estimation): rejected as contrary to the
  privacy invariant and to the target user base's expectations for a
  local governor over agentic coding work.
