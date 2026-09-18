# Libra Governor — Architecture

## System shape

```
Claude Code -> deterministic hooks/statusline -> local Governor daemon -> optional enforcement gateway -> LLM provider
```

Libra is a **local data plane**: the component that actually decides
whether work is admitted, tracks spend against an estimate, and gates
replans lives on the developer's machine, backed by SQLite in WAL mode.

## Component responsibilities

### Daemon (`crates/daemon`)

The daemon is the **source of truth**. It owns:

- The admission state machine — a task is admitted only after the
  estimator has produced a probabilistic cost/time estimate and the
  policy has confirmed enough budget is reserved to finish it.
- The ledger — an append-only, auditable record of estimated vs. actual
  spend per task, per replan.
- Policy evaluation — quality floors, cost elasticity rules, and the
  criteria for what counts as a "material boundary crossing" that must
  interrupt the user versus a routine replan that proceeds automatically.

Nothing about admission, ledger integrity, or policy evaluation is
delegated elsewhere. Every other component either feeds the daemon
information or asks it a question; only the daemon decides.

### Hooks (`integrations/claude-code`)

Deterministic lifecycle hooks wired into the host agent tool (Claude Code
today; Codex and others are integration targets, not architectural
exceptions). Hooks fire at admission time, at tool-use time, and at
replan time, and call into the daemon synchronously. Hooks contain no
policy logic of their own — they are thin, deterministic adapters between
the host tool's lifecycle events and the daemon's API.

### Enforcement gateway (`crates/gateway`, optional)

Where LLM provider traffic can be routed through an HTTP boundary, the
gateway is the **hard enforcement point** for provider spend: it can
refuse to forward a request that would violate a budget the daemon has
already decided must not be exceeded. The gateway enforces; it does not
decide. It exists only for the subset of deployments where routing
traffic through it is possible and desired — it is not a requirement for
Libra to function, and it is not a general-purpose LLM router.

Implemented in HORO-1144 as a library crate run on a thread inside the
daemon process, gated at runtime by `DaemonConfig.gateway: Option<..>`
(default `None`). "It does not decide" is structural rather than
conventional: `crates/gateway` depends on `libra-governor-domain` and
nothing else of Libra's, so it cannot open the ledger or construct a
`Policy`. Everything it is permitted to do to a task budget goes through
the `SpendAuthority` trait it defines, whose only implementation lives in
`crates/daemon`.

Not every deployment gets the same guarantee, and the system says so in
types rather than in prose: `EnforcementTier` distinguishes a
Governor-held API/BYOK credential (monetary hard cap enforced against a
pinned pricing version) from a forwarded subscription credential (token
usage observed exactly, no monetary cap — the provider does not expose
that quota's accounting) from no gateway at all. Configuration validation
*refuses to start* a combination that would overclaim. See
[`docs/adr/0003-gateway-enforcement-boundary.md`](docs/adr/0003-gateway-enforcement-boundary.md).

### MCP surface

The MCP interface is an **explain/query/manual-control API only**. It lets
a user or another tool ask the daemon "why was this admitted," "what is
the current ledger state," or issue a manual override command. **MCP is
never a security or enforcement boundary** — anything reachable only
through MCP is, by definition, not something the system depends on for
correctness or safety. Enforcement lives in the daemon and (optionally)
the gateway.

### Skills / commands

Skills and slash-commands are **UX only** — convenience wrappers that
call the daemon or MCP surface. They carry no independent state and no
policy logic.

### Statusline / system messages

The statusline and any system messages surfaced into the host agent tool
are the **visible explanation channel** for execution and replan
decisions. When the daemon admits, throttles, or replans work, that
decision is made legible to the user through this channel — not buried in
logs the user never sees.

## Privacy boundary

Full prompt content, source code, and tool output **remain local by
default**. The daemon's ledger and state store data on-disk via SQLite;
nothing about a task's actual content is transmitted off the developer's
machine as part of Libra's own operation. Only the optional gateway, when
enabled and only for the traffic explicitly routed through it, touches
data leaving the machine — and even then, it enforces provider-spend
limits rather than inspecting or exfiltrating content for Libra's own
purposes. That is a structural guarantee too: the gateway's
`gateway_requests` provenance table has no column for a body, a header, a
prompt, or tool output, and there is deliberately no debug body-dump
logging switch at any level. Any future feature that would change this default requires an
explicit, separately reviewed decision — it is not the default posture.

## Local data plane

- **Language:** Rust (workspace: `crates/domain`, `crates/protocol`,
  `crates/daemon`, `crates/cli`, `crates/ledger`, `crates/estimator`,
  `crates/gateway`).
- **Storage:** SQLite, WAL mode, on the local filesystem.
- **Process model:** a single long-lived local daemon process, a CLI
  (`libra-governor`) for direct interaction, and thin integration hooks
  for host agent tools.

This bootstrap ticket (HORO-1118) establishes the workspace skeleton and
these documented boundaries only. Real domain logic, ledger schema, and
estimator implementation are follow-up tickets (e.g. HORO-1124).
