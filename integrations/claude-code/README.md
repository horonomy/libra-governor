# Claude Code integration

The first real Claude Code integration (HORO-1125): a submitted prompt
triggers the local Governor daemon to run bounded, read-only
reconnaissance and produce a preflight result (a draft Completion
Contract, plus a placeholder the probabilistic cost/time estimator —
HORO-1126 — will fill in) surfaced inside Claude Code's own context, and
a one-line statusline showing the daemon's current state. See
[`../../ARCHITECTURE.md`](../../ARCHITECTURE.md) for how this fits the
overall hooks/daemon responsibility boundary.

## What ships in this ticket

- `libra-governor hook user-prompt-submit` — a `UserPromptSubmit` hook
  command. Reads the hook JSON payload from stdin, asks the daemon
  (starting it if not already running) for a preflight, and prints a
  `hookSpecificOutput.additionalContext` JSON object so Claude Code
  injects the preflight summary into its own context window.
- `libra-governor statusline` — a `statusLine` command. Reads the
  daemon's current task/preflight state and prints one short line, e.g.:

  ```
  libra: task a1b2c3d4 | preflight: high | recon: 0.4s
  ```

  Never spawns the daemon and never makes an LLM call of its own — see
  `crates/cli/src/statusline.rs`.

Both talk to the daemon over the versioned JSON-over-Unix-socket
protocol defined in `crates/protocol`. Everything about admission,
reconnaissance, and the ledger stays local — see the Privacy Boundary
section of `ARCHITECTURE.md`.

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
     `libra: task ... | preflight: ... | recon: ...s` shortly after you
     submit the prompt.
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

- No cost/time estimate — `PreflightResult.estimate` is always `None`
  here; the math lands in HORO-1126.
- No hard budget enforcement or blocking of execution — MVP 1.0 is
  advisory only.
- No MCP server or skill command — out of scope for this ticket; the
  hook + statusline path above is the required integration surface.
- No Completion Contract correction UX — the draft is produced and
  surfaced, but editing it is a follow-up.
