# Claude Code integration

HORO-1125 shipped the first real Claude Code integration: a submitted
prompt triggers the local Governor daemon to run bounded, read-only
reconnaissance and produce a preflight result (a draft Completion
Contract) surfaced inside Claude Code's own context, plus a one-line
statusline showing the daemon's current state. HORO-1126 closes the loop:
the preflight now carries a real probabilistic P50/P80/P90 cost/time
[`Estimate`](../../crates/domain/src/estimate.rs), tool calls are counted
during the session, and a `Stop` hook finalizes the session's task into
an [`ExecutionReceipt`](../../crates/domain/src/execution_receipt.rs) —
the estimate-vs-actual record used for calibration. HORO-1139 closes a
further loop — "govern the run": every `PostToolUse` notification is now
also checked for a MATERIAL deviation from what the current plan's
estimate implied (a possible tool-call loop, or a tool-call count that
has exceeded what history says is typical — see
[`crates/domain/src/replan.rs`](../../crates/domain/src/replan.rs) for
exactly which signals are genuinely available from Claude Code's hook
payloads and which are honestly left undetected). A material event,
subject to cooldown/max-replan hysteresis, triggers a deterministic
replan: the remaining-work estimate is recomputed and widened, linked
back to the plan it replaces with a structured reason, and persisted.
The statusline now surfaces this directly (current plan id, remaining
P90, and replan state) — see "What Claude Code's hook payloads actually
expose" below for why this is the statusline's job rather than
`PostToolUse`'s. See [`../../ARCHITECTURE.md`](../../ARCHITECTURE.md)
for how this fits the overall hooks/daemon responsibility boundary.

## What ships

- `libra-governor hook user-prompt-submit` — a `UserPromptSubmit` hook
  command. Reads the hook JSON payload from stdin, asks the daemon
  (starting it if not already running) for a preflight — now including a
  real `Estimate` (P50/P80/P90 duration and resource quantiles, computed
  by `libra-governor-estimator` from local `ExecutionReceipt` history;
  see cold-start handling below) — and prints a
  `hookSpecificOutput.additionalContext` JSON object so Claude Code
  injects the preflight summary into its own context window.
- `libra-governor hook post-tool-use` — a `PostToolUse` hook command.
  Fires a cheap, fire-and-forget notification at the daemon to increment
  a per-session tool-call counter. Never spawns the daemon and never
  waits for its response (see `crates/cli/src/client.rs::fire_and_forget`
  docs) — this must not add perceptible latency to every tool call.
- `libra-governor hook stop` — a `Stop` hook command. Asks the daemon to
  finalize the session's task: compute elapsed wall-clock duration
  (from the session's first `Preflight`), gather the tool-call count,
  and persist an `ExecutionReceipt` with `outcome: Unknown` (MVP 1.0 has
  no automated Completion Contract verification — see "What this
  integration intentionally does not do" below). Prints a concise
  Estimate-vs-Actual summary to **stderr** (never stdout, which stays
  reserved for the hook protocol); a safe no-op when the session never
  had a preceding preflight.
- `libra-governor statusline` — a `statusLine` command. Reads the
  daemon's current task/preflight state and prints one short line, e.g.:

  ```
  libra: task a1b2c3d4 | plan f00dcafe | preflight: high | recon: 0.4s | remaining P90: 90s | replanned 1x
  ```

  `replanned 1x` (or `stable` / `escalated — awaiting approval`,
  HORO-1139) reflects the most recent material replan, if any — this is
  the primary visibility surface for runtime replanning; see "What ships"
  above. Never spawns the daemon and never makes an LLM call of its own —
  see `crates/cli/src/statusline.rs`.
- `libra-governor calibration report` — a manual command (not a hook):
  asks the daemon for real duration-coverage and admission-replay
  calibration evidence, computed over every locally recorded receipt
  paired back to its originating estimate, and prints a human-readable
  report to stdout. Honestly reports "insufficient data" rather than a
  fabricated number when local history is thin — see
  `libra-governor-estimator::calibration` docs.

All four talk to the daemon over the versioned JSON-over-Unix-socket
protocol defined in `crates/protocol` (bumped to version 2 in HORO-1126,
to version 3 in HORO-1132 for the `CalibrationReport` request/response,
to version 4 in HORO-1139 for the replan-visibility fields on
`PreflightResult`/`TaskSummary`, to version 5 in HORO-1141 for
`PreflightResult`'s `admission`/`completion_reserve` and the receipt's
reservation evidence, and to version 6 in HORO-1144 for the
`GatewayStatus` request/response — see that crate's `lib.rs` docs for the
upgrade caveat: a long-lived daemon on an older protocol version must be
restarted, it will not understand a newer client's request variants).
Everything about admission, reconnaissance, estimation, and the ledger
stays local — see the Privacy Boundary section of `ARCHITECTURE.md`.

## What Claude Code's hook payloads actually expose (verified against
## the official hooks docs, HORO-1126)

- `Stop` and `PostToolUse` payloads both include a `model` field (the
  canonical model name) — `hook stop` parses and records it on the
  receipt when present.
- Neither payload exposes a provider identifier, token counts, or
  cost/spend data. `ExecutionReceipt.provider` is therefore always `None`
  today, and `actual_usage` is always an empty list — an honest "unknown"
  rather than a fabricated zero-cost figure. See
  `crates/domain/src/execution_receipt.rs` field docs.

## Setup

Build the binary and put it on your `PATH` (or reference it by absolute
path in the settings below):

```bash
cargo build --release -p libra-governor-cli
# binary at target/release/libra-governor
```

Add the following to `.claude/settings.json` (project-level) or
`~/.claude/settings.json` (user-level):

```json
{
  "hooks": {
    "UserPromptSubmit": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "/absolute/path/to/libra-governor hook user-prompt-submit"
          }
        ]
      }
    ],
    "PostToolUse": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "/absolute/path/to/libra-governor hook post-tool-use"
          }
        ]
      }
    ],
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "/absolute/path/to/libra-governor hook stop"
          }
        ]
      }
    ]
  },
  "statusLine": {
    "type": "command",
    "command": "/absolute/path/to/libra-governor statusline"
  }
}
```

No further configuration is required. The daemon is started on demand —
see "Daemon lifecycle" below — and stores its state under
`$LIBRA_GOVERNOR_STATE_DIR`, or `$XDG_STATE_HOME/libra-governor`, or
`~/.local/state/libra-governor` (see `crates/daemon/src/paths.rs`).

## Daemon lifecycle

The daemon is *not* started separately — the hook subcommand starts it
on demand the first time it is needed:

1. `hook user-prompt-submit` tries to connect to the daemon's Unix
   socket.
2. If that fails, it spawns `libra-governor daemon run` as a detached
   background process (stdin/stdout/stderr redirected to `/dev/null`;
   diagnostics go to `daemon.log` in the state dir, never to stdout,
   which is reserved for the hook protocol response) and polls for the
   socket to become connectable for up to ~3 seconds before giving up
   and degrading gracefully.
3. The daemon then keeps running in the background across prompts and
   sessions, so subsequent hook invocations connect immediately with no
   spawn latency.

`statusline` **never** spawns the daemon — a statusline refreshes on a
short interval, and spawning from it would be a race factory. If the
daemon is not running, `statusline` prints `libra: -` and exits
immediately.

### Already-running / stale-socket detection

`libra-governor daemon run` binds its Unix socket before doing anything
else. If the bind fails with `AddrInUse`:

- It tries to *connect* to the same path. A successful connect proves a
  live daemon already owns it — this process logs the fact and exits
  cleanly (the spawn-if-absent race is expected to sometimes produce two
  near-simultaneous daemon starts; only one keeps running).
- A failed connect proves the socket file is stale (left behind by a
  crashed daemon) — the file is removed and the bind retried exactly
  once.

The bind is never preceded by an unconditional `unlink`, so a
slow-starting live daemon's socket is never deleted out from under it.
See `crates/daemon/src/server.rs::bind_or_detect_running` for the exact
logic and its known narrow-window limitation (documented there).

## Manual smoke test against a real Claude Code install

1. Build the binary: `cargo build --release -p libra-governor-cli`.
2. Add the `.claude/settings.json` snippet above to a real project,
   pointing `command` at your built binary's absolute path.
3. Open that project in Claude Code and submit any prompt.
4. Confirm:
   - The statusline (bottom of the Claude Code UI) updates to show
     `libra: task ... | plan ... | preflight: ... | recon: ...s |
     remaining P90: ... | stable` shortly after you submit the prompt.
   - `tail -f ~/.local/state/libra-governor/daemon.log` shows a `daemon
     started` line the first time, and no errors on later prompts.
   - `ls ~/.local/state/libra-governor/` shows `daemon.sock`,
     `ledger.sqlite3`, and `daemon.log`.
5. Submit a second prompt in the same Claude Code session. Confirm the
   statusline's task id is unchanged (same task, per
   `docs/adr/0002-task-not-session-as-economic-unit.md`) while the
   Completion Contract revision surfaced in Claude's context advances
   (revision 1 -> 2).
6. Kill the daemon (`pkill -f "libra-governor daemon run"`) and submit
   another prompt. Confirm the hook still returns quickly (it respawns
   the daemon) and the statusline briefly shows `libra: -` before the
   new daemon comes up.

This manual path is not automated in CI; the equivalent scenarios are
covered by `crates/cli/tests/hook_cli_integration.rs` (spawns the real
binary as a subprocess) and `crates/daemon/tests/preflight_integration.rs`
(drives the real daemon dispatch logic over a real socket).

## Enforcement gateway (HORO-1144) — optional, off by default

Everything above is **advisory**. A hook can print a preflight summary
into Claude Code's context and the ledger can record that a budget was
exceeded, but nothing physically stops the agent from spending the money.
The enforcement gateway is the missing checkpoint: a loopback reverse
proxy that reserves each request's worst-case cost *before* forwarding
it, refuses one the budget cannot absorb, and settles the reservation
against the provider's own reported usage afterwards.

It is **off unless configured** (`DaemonConfig.gateway` defaults to
`None`) and runs on a thread inside the existing daemon process — not a
second binary, not a Cargo feature. See
[`docs/adr/0003-gateway-enforcement-boundary.md`](../../docs/adr/0003-gateway-enforcement-boundary.md)
for the full decision record.

### Capability tiers — what each mode can honestly claim

| Mode | Tier | Credential | Usage accounting | Monetary hard cap |
|---|---|---|---|---|
| API / BYOK key, held by the Governor | `GatewayMetered` | Governor-held; the agent never sees it | Provider-reported, exact | **Enforced**, at a pinned `pricing_version` |
| Subscription (Max/Pro) OAuth, forwarded unchanged | `GatewayObservedQuota` | Agent-held; the Governor takes no custody | Provider-reported, exact | **Not available** — the subscription's own quota accounting is not exposed to us |
| No gateway — hooks only | `HooksOnly` | Not applicable | None | **Not available** — hooks fire after the request is already in flight |

This is not a documentation promise. Configuration validation **refuses
to start** a `PassThroughSubscription` gateway against a USD-denominated
admission policy, because that pairing would advertise a monetary cap the
system cannot honor. Run `libra-governor gateway status` to see what your
deployment actually claims:

```
Tier:       GatewayObservedQuota
Credential: AgentHeld
Usage:      ProviderReported
Monetary cap: NOT ENFORCED (OpaqueProviderQuota)
```

### Setup

The gateway needs two things Claude Code does not have today: an endpoint
to talk to, and something to authenticate with. The real provider key
stays out of both.

1. **Get the local capability token.** This is 32 bytes of OS randomness
   that authorize use of a loopback proxy on this machine. It is **not**
   a provider credential and is worth nothing anywhere else.

   ```bash
   libra-governor gateway token
   ```

2. **Point Claude Code at the gateway**, in `~/.claude/settings.json`:

   ```json
   {
     "env": {
       "ANTHROPIC_BASE_URL": "http://127.0.0.1:8787"
     },
     "apiKeyHelper": "/absolute/path/to/libra-governor gateway token"
   }
   ```

   Plain HTTP, loopback only, deliberately: Claude Code does not route
   loopback traffic through its own trust store, so a self-signed local
   certificate would fail. The boundary is the loopback bind plus the
   capability token, not TLS on this hop.

3. **Tell the daemon where the real key lives** — as a *command*, never as
   a value in a config file:

   ```
   security find-generic-password -s anthropic-api-key -w     # macOS Keychain
   pass show anthropic/api-key                                # pass
   op read "op://Private/Anthropic/api-key"                   # 1Password
   ```

   The daemon runs that command at startup, reads stdout into a newtype
   with no `Display`, no `Serialize`, and a `Debug` that prints
   `<redacted>`, and substitutes it on the outbound request. It never
   reaches Claude Code's environment, arguments, or configuration; never
   the ledger; and never a log line at any level.

4. **Restart the daemon** (`pkill -f "libra-governor daemon run"`; the
   next hook invocation respawns it). The gateway is started from the
   daemon's own configuration — there is deliberately no
   `gateway start` command, because a security boundary a client can
   switch off with one keystroke is not one.

### What a refusal looks like

A refused request gets HTTP **403** (never 429 — Claude Code treats 429
as a rate limit and retries with backoff, which would storm a boundary
that will refuse every time), an Anthropic-shaped error body so Claude
Code renders it, and two headers:

```
x-libra-decision: budget_exceeded | task_unbound | unpriced_model | no_budget |
                  unenforceable_request | ambiguous_credential | host_mismatch |
                  route_not_found | payload_too_large | overloaded
x-libra-request-id: <uuid>
```

```json
{"type":"error","error":{"type":"permission_error",
 "message":"libra-governor: this request would exceed the budget admitted for this task"}}
```

### Failure semantics

- **The gateway's own admission fails closed.** Anything it cannot meter
  exactly, it refuses before any provider call: no `max_tokens`, no task
  binding, an unpriced model against a USD budget, a quota-percent
  budget, a policy denial, exhausted headroom, or an unreachable ledger.
- **The daemon's core function fails open.** If gateway configuration
  validation or credential resolution fails, the daemon logs it, leaves
  the gateway disabled, and keeps serving hooks and the statusline
  normally. Check `gateway status` — it reports `not running` with the
  reason.
- **A crash leaves the reservation to the TTL.** An in-flight request
  whose process dies leaves an `Active` reservation that
  `expire_stale_reservations` reclaims after `gateway_reservation_ttl_secs`
  (default 600s, shorter than the plan-level 900s).

### What the gateway intentionally does not do

- **No `GET /v1/models`, no Bedrock/Vertex/Foundry, no non-Anthropic
  provider.** The closed route table proxies exactly three endpoints:
  `POST /v1/messages`, `POST /v1/messages/count_tokens` (free, unmetered),
  and `GET|HEAD /api/hello` (answered locally).
- **No interactive approval.** A policy `ApprovalRequired` forwards the
  request and surfaces the fact through `gateway status`; the proxy has
  no channel through which to interrupt a human mid-request.
- **No resolved-IP SSRF guard beyond TLS hostname pinning.** Certificate
  validation against the configured hostname already binds the real
  destination — a DNS rebind to loopback fails validation and carries no
  bytes. An IP check would add a TOCTOU race for a property TLS already
  guarantees.
- **No live pricing.** `PRICING_VERSION` names a pinned static snapshot
  recorded on every reservation and provenance row. When prices change it
  is stale until updated; a `pricing_overrides` config field is the
  escape hatch.
- **No body or header logging, at any level, ever.** There is no debug
  dump switch, and the `gateway_requests` table has no column that could
  hold one.
- **No crash-time spend recovery.** A crash mid-request returns the
  capacity via the TTL but loses that one request's *spend* record, so
  total spend is under-counted by it.
- **No aggregate tuning for parallel subagents.** N concurrent requests
  each reserve their own worst case against one `TaskBudget`; their sum
  can refuse a request that real usage would have fit. Not fixed in this
  MVP.

### Known limitation: the session-binding header is unverified

The gateway attributes a request to a task via a session-id header —
`x-claude-code-session-id` by default, configurable via
`DaemonConfig.gateway_session_header`. A request carrying no such header
is refused `403 task_unbound`, because charging it to whichever task
happens to be around would be worse than refusing it.

**Whether a real Claude Code build emits that header on its Messages
requests has not been verified against live provider traffic** — the
integration tests drive a local fake upstream, by design (no real
credential and no live API call exists anywhere in this repository). If
your Claude Code build sends a different header, set
`gateway_session_header` to match; if it sends none, the gateway will
refuse every metered request rather than mis-attribute one. This is the
one part of the setup above that a real end-to-end smoke test is needed
to confirm.

## What this integration intentionally does not do (yet)

- No automated Completion Contract verification — `hook stop` always
  records `ExecutionOutcome::Unknown`. MVP 1.0 has no test-running
  integration; a task is never inferred "done" merely because the model
  stopped talking. Future scope.
- No hard budget enforcement *through the hooks* — the hook path is
  advisory only, on every hook including `Stop` (no `{"decision":
  "block", ...}` is ever emitted). Hard enforcement, where it is wanted,
  is the optional gateway documented above (HORO-1144), not a blocking
  hook decision.
- No task classification — the estimator's class-bucketed-history tier
  is implemented and unit-tested in `libra-governor-estimator`, but no
  caller supplies a class yet (nothing in the schema classifies tasks).
  Every MVP 1.0 estimate comes from the global-local-history or
  cold-start tier. See that crate's docs.
- No cross-session task finalization — `resolve_or_create_task_for_session`
  already maps each distinct `session_id` to its own `TaskId`, so `hook
  stop` finalizes per-session/per-task; there is no multi-session merge
  to get wrong.
- No MCP server or skill command — out of scope; the hook + statusline
  path above is the required integration surface. Per `ARCHITECTURE.md`,
  any future MCP surface stays explain/query/manual-control only and is
  never an enforcement boundary: enforcement lives in the daemon and, when
  enabled, the gateway.
- No Completion Contract correction UX — the draft is produced and
  surfaced, but editing it is a follow-up.
