# ADR 0004: The Agent Adapter Is a Shared Translation Layer, Not a Trait

- **Status:** Accepted
- **Date:** 2026-09-19
- **Ticket:** HORO-1157

## Context

Libra shipped its first integration against Claude Code (HORO-1125
onward): `hook user-prompt-submit`/`post-tool-use`/`stop` read Claude
Code's hook JSON payloads from stdin, talk to the daemon over
`crates/protocol`'s versioned wire format, and print/log the result.
Adding a second governed coding agent (Codex CLI) raises the obvious
question: does Libra need an `AgentAdapter` trait, a `crates/agent-adapter`
crate, or some other plugin abstraction to support more than one agent?

Two facts, checked against the actual code and against Codex's real
documented hook schemas rather than assumed, answer that question:

1. **`crates/protocol` and `crates/daemon` are already agent-neutral.**
   `handle_preflight`/`handle_tool_invoked`/`handle_finalize` and every
   `Request`/`Response` variant carry generic fields (`session_id`, `cwd`,
   `prompt`, `tool_name`, `model`, ...) — zero Claude-specific field name
   or tool-name vocabulary exists anywhere in either crate. This was
   verified by reading both crates in full during HORO-1157's design
   phase, not assumed.
2. **Codex's real hook payloads use the same field names.** Codex CLI has
   its own hooks mechanism (config at `~/.codex/hooks.json` or
   `[hooks]`/`[hooks.state]` in `~/.codex/config.toml`; `$CODEX_HOME`
   overrides `~/.codex`), with events including `SessionStart`,
   `SessionEnd`, `UserPromptSubmit`, `PreToolUse`, `PermissionRequest`,
   `PostToolUse`, `PreCompact`, `PostCompact`, `SubagentStart`,
   `SubagentStop`, `Stop`, `Interrupt`. Its generated JSON schemas (from
   `openai/codex`) show:
   - `UserPromptSubmit`: `session_id`, `cwd`, `prompt`, `model`,
     `turn_id`, `permission_mode`, `hook_event_name`, `transcript_path`
     (+ optional `agent_id`/`agent_type`).
   - `PostToolUse`: `session_id`, `cwd`, `tool_name`, `tool_input`,
     `tool_response`, `tool_use_id`, `model`, `turn_id`,
     `permission_mode`, `transcript_path`.
   - `Stop`: `session_id`, `cwd`, `model`, `turn_id`, `stop_hook_active`,
     `last_assistant_message`, `permission_mode`, `transcript_path`.
   - stdout contract for `UserPromptSubmit`:
     `hookSpecificOutput: { hookEventName, additionalContext }` plus
     `continue`/`decision`/`reason`/`stopReason`/`systemMessage`/
     `suppressOutput` — the same shape Claude Code's hooks already use.

   Every field either host's payload needs that the other's payload
   doesn't send is additive, never conflicting: `session_id`/`cwd`/
   `prompt`/`tool_name`/`model` name the same thing on both sides.

When two hosts already agree on wire shape, a trait or plugin crate would
be new machinery solving a problem the data does not have — indirection
with no divergence to justify it. See `ARCHITECTURE.md`'s constraint that
`crates/cli` stays a thin client with no independent policy logic; a
heavyweight adapter abstraction there would cut against that same grain.

## Decision

**No new trait. No `crates/agent-adapter` crate. No daemon, protocol, or
gateway change. No protocol version bump.** The adapter boundary is
entirely inside `crates/cli`, as a shared translation layer both agents'
thin entry points call:

- `crates/cli/src/agent/payload.rs` — one set of payload structs serving
  both hosts. Required fields only; everything host-specific
  (`transcript_path`, `turn_id`, `permission_mode`, `tool_use_id`,
  `agent_id`/`agent_type`, `last_assistant_message`, `stop_hook_active`)
  is an ignored extra field, never rejected (`#[serde(deny_unknown_fields)]`
  is never used here).
- `crates/cli/src/agent/event.rs` — [`NormalizedEvent`], the host-agnostic
  vocabulary: `PromptSubmitted`, `ToolCompleted`, `TurnCompleted`
  (payload-carrying, wired to a daemon action), `RecognizedUnwired` (a
  genuine agent lifecycle event this integration deliberately does not
  wire yet), `Unrecognized` (forward compatibility: an unknown event name
  is a value, never an error or a panic).
- `crates/cli/src/agent/normalize.rs` — [`normalize`], a total function
  (`EntryPoint`, raw stdin) -> `Result<NormalizedEvent, NormalizeError>`.
  Malformed JSON or a missing required field is a returned error, never a
  panic or a nonzero exit further up the call chain.
- `crates/cli/src/agent/run.rs` — `run_prompt_submit`/`run_tool_completed`/
  `run_turn_completed(agent: AgentKind)`, the single implementation both
  `crates/cli/src/hook.rs`/`hook_post_tool_use.rs`/`hook_stop.rs` (Claude
  Code) and `crates/cli/src/codex_hook.rs` (Codex) call. Preserves the
  exact pre-HORO-1157 stdout/stderr discipline and daemon-spawn-on-
  preflight-only behavior — see `crates/cli/tests/agent_contract.rs`'s
  byte-exact goldens recorded against the pre-refactor binary.
- `crates/cli/src/agent/render.rs` — the rendering helpers moved verbatim
  from the pre-refactor `hook.rs`/`hook_stop.rs`.

`libra_governor_domain::AgentKind` (`ClaudeCode`, `Codex`) is the one
enum both the hook translation layer and the capability matrix
(`AgentCapabilities`) use — `crates/cli/src/agent/mod.rs` re-exports it
rather than keeping a parallel cli-local copy.

### The `Stop`-not-`SessionEnd` decision

Codex's `Interrupt`/`SessionEnd` events default to a 1s timeout, 3s max —
too tight for a client-daemon round trip that may need to spawn the
daemon. `SessionEnd`'s schema also carries no `model` field, which the
finalize path's Execution Receipt formatting depends on. `Stop` gets
Codex's normal 600s budget and does carry `model`, so `hook stop`/
`codex-hook stop` finalize on `Stop`, matching Claude Code's own hook,
not on `SessionEnd`.

### Capability model, not a runtime feature-negotiation protocol

`libra_governor_domain::agent::AgentCapabilities` (mirroring
[ADR 0003](0003-gateway-enforcement-boundary.md) §8's
`EnforcementCapabilities` discipline) is a pure function of `AgentKind`
alone — `AgentCapabilities::for_agent`, never sniffed or probed at
runtime. `libra-governor agents [--json]` renders it. The structural
invariant `hard_budget_enforcement: Available` implies
`model_gateway: Available` is enforced by a unit test that inspects every
`AgentKind`, not just the two defined today.

### Trust gate

Writing `~/.codex/hooks.json` (via `crates/cli/src/codex_hooks_file.rs`,
mirroring `claude_settings.rs`'s exact safety discipline: parse-or-abort,
backup-before-write, atomic rename) is necessary but not sufficient for
Codex to run these hooks. `[hooks.state]` in `~/.codex/config.toml` gates
whether a configured hook actually runs, and trust is recorded by content
hash — the user must run `/hooks` inside Codex. `install --agent codex`'s
printed next-steps and `doctor`'s read-only inspection both surface this.
`doctor`'s trust-state check adds no TOML parser dependency (none existed
in this workspace before HORO-1157) — it is a minimal, conservative text
scan that reports `HooksDisabled` only for a literal `[features] hooks =
false` line, `LikelyNotYetTrusted` when no `[hooks.state]` section exists
at all, and otherwise always `Unknown`: this doctor has not verified
`[hooks.state]`'s real key shape, so it never claims a confidently-parsed
"trusted" verdict.

### Installed hook timeout

Every hook this integration installs into Codex's `hooks.json` carries a
15-second timeout, not Codex's own 600-second default: a wedged daemon
must not hang a user's prompt for ten minutes, and this repo's own daemon
client timeout is already ~5s plus a ~3s spawn budget, so 15s is a real
ceiling above that. `PostToolUse`'s installed entry additionally carries
`"async": true` — it prints nothing to stdout and must never add
perceptible latency to every tool call, matching the fire-and-forget
discipline `crates/cli/src/client.rs::fire_and_forget` already documents
for Claude Code's `PostToolUse` hook.

## Non-goals (explicit, with the concrete reason each is out of scope)

- **No gateway/enforcement path for Codex.** Codex's custom model
  provider (`model_providers.<id>`) only supports `wire_api = "responses"`
  (OpenAI's `POST /v1/responses`), incompatible with the gateway crate,
  which only speaks Anthropic's `POST /v1/messages`. `AgentCapabilities`
  reports this as `CapabilityGap::Incompatible` for both `model_gateway`
  and `hard_budget_enforcement`, not a vague "unsupported".
- **No statusline equivalent for Codex.** Verified absent from Codex's
  config schema at any level.
- **No `PreToolUse` hard-gating for either agent.** MVP 1.0 hooks stay
  advisory for both hosts; the gateway remains the only hard-enforcement
  point (ADR 0003).
- **No MCP surface implementation.** Codex supports MCP as a client
  (`mcp_servers.<id>` stdio config), but this integration does not wire
  an explain/query surface for either agent yet, and `ARCHITECTURE.md`
  already forbids MCP as an enforcement boundary regardless.
- **No `SessionStart`/`SessionEnd`/`SubagentStart`/`SubagentStop`/
  `Interrupt`/`PreCompact`/`PostCompact` daemon wiring.** Each of these
  would need new protocol variants and ledger semantics nobody has
  specified yet; they map to `NormalizedEvent::RecognizedUnwired` only.
- **No protocol version bump, no daemon change, no gateway change** — see
  "Decision" above.

## What was verified vs. what remains open

**Verified** (checked against `openai/codex`'s generated JSON schemas and
its hooks documentation, not assumed): the event names and field lists
quoted in "Context" above; the `hookSpecificOutput` stdout contract
shape; the 600s default hook timeout and the 1s/3s `SessionEnd`/
`Interrupt` timeout; the `wire_api: "responses"`-only custom model
provider; MCP client support; the macOS 12+/Ubuntu 20.04+/Debian
10+/Windows-11-via-WSL2 platform set; the absence of a statusline key in
Codex's config schema.

**Unverified — treated as unknown, never asserted as fact:**

- Whether Codex's `hooks` feature is on by default in a fresh install.
- The exact key shape of `[hooks.state]` — `doctor`'s trust-state check
  is deliberately conservative because of this (see "Trust gate" above).
- Whether Codex's own `hooks.json` file shape matches the assumption
  `codex_hooks_file.rs` makes (the same event-keyed
  array-of-matcher-groups shape Claude Code's `settings.json` uses) — see
  that module's own doc comment.
- Whether Codex's `session_id` stays stable across `compact`/`fork`/
  `resume` — the one open empirical question a real local Codex smoke
  test was meant to help answer; see the PR/ticket evidence for whether
  that smoke test was actually run in this environment, and if not, why.

## Consequences

### Gained

- A second governed agent with the full preflight/tool-observation/
  completion-receipt/inline-explanation-channel capability tier, at the
  cost of ~6 small files in `crates/cli` and one new `crates/domain`
  module — no new crate, no daemon/protocol/gateway churn.
- `libra-governor agents --json` gives any caller (a human, a script, a
  future dashboard) one honest source of truth for "what can Libra
  actually do for agent X", enforced by the same structural-invariant
  test discipline ADR 0003 established for gateway tiers.
- The Claude Code regression proof (`crates/cli/tests/agent_contract.rs`'s
  byte-exact goldens, `crates/cli/tests/hook_cli_integration.rs` and
  `crates/daemon/tests/{preflight,replan}_integration.rs` left completely
  untouched) is itself now checked-in evidence that the extraction
  changed no observable Claude Code behavior.

### Accepted costs

- `codex_hooks_file.rs`'s file-shape assumption is unverified; if wrong,
  `install --agent codex` needs a follow-up fix (disclosed above, not
  silently assumed correct).
- `doctor`'s trust-state reporting is conservative to the point of never
  confirming "trusted" — a real usability gap until `[hooks.state]`'s
  shape is verified, accepted deliberately over the alternative (a doctor
  that could report a false "trusted").
- No gateway/MCP path for Codex in this ticket — a real capability gap
  Codex users hit immediately, tracked as a non-goal rather than solved
  here because the wire-format incompatibility is a genuine blocker, not
  a scoping choice.

## Alternatives considered and rejected

- **`AgentAdapter` trait + per-agent implementations.** Rejected: with
  both hosts' payload shapes and stdout contracts already identical, a
  trait would abstract over zero actual variation — every method would
  either be shared logic wrapped in indirection or a one-line dispatch on
  `AgentKind`, which the current `match` in `agent::run` already does
  more simply.
- **`crates/agent-adapter` as a separate crate.** Rejected for the same
  reason, plus it would violate `ARCHITECTURE.md`'s existing crate
  boundaries for no benefit — this is CLI-surface translation, not a
  reusable library concern any other crate needs.
- **Runtime capability probing** (asking Codex/Claude Code what they
  support at startup). Rejected: mirrors ADR 0003 §8's own rejection of
  auto-negotiated enforcement tiers — a wrong guess here would be a worse
  lie than a static, tested capability table, and neither host exposes a
  "what do you support" API to probe in the first place.
