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
then to version 4 in HORO-1139 for the replan-visibility fields on
`PreflightResult`/`TaskSummary` — see that crate's `lib.rs` docs for the
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

## What this integration intentionally does not do (yet)

- No automated Completion Contract verification — `hook stop` always
  records `ExecutionOutcome::Unknown`. MVP 1.0 has no test-running
  integration; a task is never inferred "done" merely because the model
  stopped talking. Future scope.
- No hard budget enforcement or blocking of execution — MVP 1.0 is
  advisory only, on every hook including `Stop` (no `{"decision":
  "block", ...}` is ever emitted).
- No task classification — the estimator's class-bucketed-history tier
  is implemented and unit-tested in `libra-governor-estimator`, but no
  caller supplies a class yet (nothing in the schema classifies tasks).
  Every MVP 1.0 estimate comes from the global-local-history or
  cold-start tier. See that crate's docs.
- No cross-session task finalization — `resolve_or_create_task_for_session`
  already maps each distinct `session_id` to its own `TaskId`, so `hook
  stop` finalizes per-session/per-task; there is no multi-session merge
  to get wrong.
- No MCP server or skill command — out of scope for this ticket; the
  hook + statusline path above is the required integration surface.
- No Completion Contract correction UX — the draft is produced and
  surfaced, but editing it is a follow-up.
