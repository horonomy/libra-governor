# Codex CLI integration

HORO-1157 added Codex CLI as a second governed coding agent, sharing
every byte of hook-translation logic with the existing
[Claude Code integration](../claude-code/README.md) via
`crates/cli/src/agent/` — see
[`docs/adr/0004-agent-adapter-contract.md`](../../docs/adr/0004-agent-adapter-contract.md)
for why that is a shared translation layer rather than a trait or a
separate crate, and for exactly what was verified against Codex's real
hook schemas vs. what remains unverified.

## What ships

- `libra-governor codex-hook user-prompt-submit` — the Codex
  `UserPromptSubmit` hook entry point. Reads the hook JSON payload from
  stdin, asks the daemon (starting it if not already running) for a
  preflight, and prints a `hookSpecificOutput.additionalContext` JSON
  object — byte-for-byte the same translation `hook user-prompt-submit`
  (Claude Code) performs, via `crate::agent::run::run_prompt_submit`.
- `libra-governor codex-hook post-tool-use` — the Codex `PostToolUse`
  hook entry point. Fire-and-forget tool-call counting, never spawns the
  daemon, never blocks on a response.
- `libra-governor codex-hook stop` — the Codex `Stop` hook entry point.
  Finalizes the session's task into an `ExecutionReceipt`, printing the
  Estimate-vs-Actual summary to stderr. Maps to `Stop`, not `SessionEnd`
  — see the ADR's "Stop-not-SessionEnd decision".
- `libra-governor install --agent codex` / `uninstall --agent codex
  [--yes]` — wires/unwires exactly these three hooks into
  `~/.codex/hooks.json` (or `$CODEX_HOME/hooks.json`), via
  `crates/cli/src/codex_hooks_file.rs`, which mirrors
  `claude_settings.rs`'s exact safety discipline (parse-or-abort on
  malformed JSON, a verbatim-bytes backup before every write, atomic
  temp-file-then-rename, never touching a foreign hook group).
- `libra-governor doctor` — reports whether `hooks.json`'s three groups
  are wired, whether the wired command path matches the binary currently
  running the check (a moved binary needs re-trust — see "Trust gate"
  below), and a best-effort read of `~/.codex/config.toml`'s
  `[hooks.state]` trust gate.
- `libra-governor agents [--json]` — the capability matrix for both
  agents this integration knows about (see "Capability tiers" below).

## What Codex's real hook payloads expose (verified against
## `openai/codex`'s generated JSON schemas, HORO-1157)

- `UserPromptSubmit`: `session_id`, `cwd`, `prompt`, `model`, `turn_id`,
  `permission_mode`, `hook_event_name`, `transcript_path` (+ optional
  `agent_id`/`agent_type` for subagent sessions).
- `PostToolUse`: `session_id`, `cwd`, `tool_name`, `tool_input`,
  `tool_response`, `tool_use_id`, `model`, `turn_id`, `permission_mode`,
  `transcript_path`.
- `Stop`: `session_id`, `cwd`, `model`, `turn_id`, `stop_hook_active`,
  `last_assistant_message`, `permission_mode`, `transcript_path`.
- stdout contract: `hookSpecificOutput: { hookEventName,
  additionalContext }` plus `continue`/`decision`/`reason`/`stopReason`/
  `systemMessage`/`suppressOutput` — the same shape Claude Code's hooks
  already use, which is exactly why `crate::agent::render` needed no
  Codex-specific branch.
- Every field name Codex's payloads use for `session_id`/`cwd`/`prompt`/
  `tool_name`/`model` matches Claude Code's own payloads exactly — see
  `crates/cli/src/agent/payload.rs`.
- Default hook timeout is 600s; `SessionEnd`/`Interrupt` default to
  1s/3s max — too tight for a client-daemon round trip, which is why
  `hook stop` maps to `Stop`, not `SessionEnd` (see the ADR).

**Unverified, treated as unknown rather than asserted** — see the ADR's
"What was verified vs. what remains open" for the complete list:
whether Codex's `hooks` feature defaults on; `[hooks.state]`'s exact key
shape; whether `hooks.json`'s real file shape matches what
`codex_hooks_file.rs` assumes; whether `session_id` stays stable across
`compact`/`fork`/`resume`.

## Setup

Build the binary the same way as for Claude Code:

```bash
cargo build --release -p libra-governor-cli
# binary at target/release/libra-governor
```

Then wire the three Codex hooks:

```bash
/path/to/libra-governor install --agent codex
```

This writes `~/.codex/hooks.json` (or `$CODEX_HOME/hooks.json`) with:

```json
{
  "UserPromptSubmit": [
    { "hooks": [{ "type": "command", "command": "/absolute/path/to/libra-governor codex-hook user-prompt-submit", "timeout": 15 }] }
  ],
  "PostToolUse": [
    { "hooks": [{ "type": "command", "command": "/absolute/path/to/libra-governor codex-hook post-tool-use", "timeout": 15, "async": true }] }
  ],
  "Stop": [
    { "hooks": [{ "type": "command", "command": "/absolute/path/to/libra-governor codex-hook stop", "timeout": 15 }] }
  ]
}
```

Every hook gets a 15-second timeout (not Codex's own 600s default) so a
wedged daemon cannot hang a prompt for ten minutes; `PostToolUse` also
gets `"async": true` since it prints nothing and must never add
perceptible latency to a tool call.

### The `/hooks` trust step — required, not optional

**Writing `hooks.json` is not enough for Codex to actually run these
hooks.** `[hooks.state]` in `~/.codex/config.toml` gates whether a
configured hook fires at all, and trust is recorded by content hash.
After `install --agent codex`:

1. Open Codex.
2. Run `/hooks` inside Codex and trust the three `libra-governor` hooks.
3. If you later move or rebuild the binary, its content hash changes —
   you must re-trust it the same way.

`libra-governor doctor` flags a binary-path mismatch (a strong signal
you moved the binary and forgot to re-trust) but cannot itself detect
whether `/hooks` trust was granted — see "Trust gate" below for why.

No further daemon configuration is required to get today's defaults —
see the [Claude Code README's "Configuring the daemon
(`config.json`)"](../claude-code/README.md#configuring-the-daemon-configjson)
section, which is agent-agnostic (the daemon has no idea which agent
sent a request).

## Trust gate — `doctor`'s honesty limits

`libra-governor doctor` reads `~/.codex/config.toml` as plain text — no
TOML parser dependency was added (none existed anywhere in this
workspace before HORO-1157, and this ticket's scope explicitly avoids
adding one). It reports exactly three states, deliberately conservative:

| State | Meaning |
|---|---|
| `hooks disabled in Codex config` | A literal `[features]` section with `hooks = false` was found — unambiguous regardless of TOML nesting/quoting subtleties. |
| `likely not yet trusted` | No `[hooks.state]` section header exists anywhere in the file — Codex has nowhere else to record trust, so this is reasonably strong evidence `/hooks` has not been run. |
| `unknown` | Either `config.toml` could not be read, or a `[hooks.state]` section does exist but this doctor does not know its real key shape (unverified — see the ADR) and makes no claim about what is trusted inside it. |

`doctor` **never** reports "trusted" — that would require parsing
`[hooks.state]`'s real shape, which was not verified during HORO-1157.
Verify trust yourself with `/hooks` inside Codex.

## Capability tiers

`libra-governor agents --json` is the source of truth; a summary:

| Capability | Codex |
|---|---|
| Preflight gate, tool observation, completion receipt, inline explanation channel | Available — same hook shapes as Claude Code |
| Tool gate (hard pre-tool-call refusal) | Unavailable — MVP 1.0 hooks are advisory for every agent; the gateway is the only hard-enforcement point (ADR 0003) |
| Model event observation (token/cost/provider per request) | Unavailable — no hook payload on either agent exposes this |
| Model gateway / hard budget enforcement | **Unavailable — incompatible wire format.** Codex's custom model provider only supports `wire_api: "responses"` (`POST /v1/responses`); the gateway only speaks Anthropic's `POST /v1/messages`. Not a scoping choice — a real incompatibility. |
| Persistent status surface (statusline) | Unavailable — verified absent from Codex's config schema at any level |
| Session/subagent lifecycle, interruption signal, MCP explain surface | Unavailable — Codex exposes the underlying hook/MCP primitives, but this integration does not wire any of them to a daemon action yet (see non-goals below) |

## Explicit non-goals for this integration

- **No gateway/enforcement path for Codex** — the `responses` vs.
  `messages` wire-format incompatibility above is a real blocker, not an
  oversight.
- **No statusline equivalent** — verified absent from Codex's config
  schema.
- **No `PreToolUse` hard-gating** — matches Claude Code; hooks stay
  advisory for both agents in MVP 1.0.
- **No MCP explain/query surface** — Codex supports MCP as a client
  (`mcp_servers.<id>` stdio config), but `ARCHITECTURE.md` forbids MCP as
  an enforcement boundary regardless, and no explain surface is wired for
  either agent yet.
- **No `SessionStart`/`SessionEnd`/`SubagentStart`/`SubagentStop`/
  `Interrupt`/`PreCompact`/`PostCompact` daemon wiring** — each needs new
  protocol variants and ledger semantics nobody has specified yet; these
  map to `NormalizedEvent::RecognizedUnwired` only when they occur.

## Platforms

Same practical platform set as the existing Unix-socket daemon: macOS
12+, Ubuntu 20.04+/Debian 10+, Windows 11 via WSL2 only.

## Uninstall

```bash
libra-governor uninstall --agent codex --yes
```

Removes exactly the three Governor-owned hook groups from `hooks.json`
(any foreign hook group survives byte-for-byte), then — with the same
confirmation discipline as the Claude Code path — the shared state
directory. See the root [`README.md`](../../README.md#uninstall) for the
full safety guarantees, which are agent-agnostic.
