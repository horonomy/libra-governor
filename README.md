# Libra Governor — Developer Preview

> Never start work you are unlikely to afford to finish.

Libra is a local **Governor**: a data-plane daemon that sits between an
agentic coding tool (Claude Code today) and the LLM provider it talks
to. It estimates the cost and time-to-complete of a task *before*
admitting it, tracks spend against that estimate as the task runs, and
makes replanning a deliberate, auditable decision rather than an implicit
one. See [`PRODUCT.md`](PRODUCT.md) for the full product North Star and
[`ARCHITECTURE.md`](ARCHITECTURE.md) for how the pieces fit together.

This is the **v0.0.1 Developer Preview**: the validated MVP 3 local flow
(HORO-1137 Policy presets, HORO-1139 runtime replanning, HORO-1141
Completion Reserve, HORO-1144 the optional enforcement gateway), plus
the installation, diagnostics, and documentation this file covers
(HORO-1150). No new admission/policy/gateway logic was added to
productize it.

## Requirements

- A Rust toolchain (`cargo`, `rustc`) — see <https://rustup.rs> if you
  do not have one.
- macOS or Linux. Claude Code with hook and statusline support.
- No Docker. No account or login. No SaaS dependency — everything below
  runs entirely on your machine.

## Install

This repository's CI (`.github/workflows/ci.yml`) does not publish a
release binary anywhere — there is no `curl | sh`-a-prebuilt-binary path
to offer honestly at v0.0.1. The real install path is building from a
clone with `cargo install`:

```bash
git clone https://github.com/horonomy/libra-governor.git
cd libra-governor
./scripts/install.sh
```

`scripts/install.sh` runs `cargo install --path crates/cli --locked`,
wires this binary's hooks and statusline into
`~/.claude/settings.json` (via `libra-governor install` — see
"Uninstall" below for exactly what that touches), and finishes by
running `libra-governor doctor` so you see real, current state rather
than an installer's own claim of success.

Equivalent manual steps, if you would rather not run the script:

```bash
cargo install --path crates/cli --locked
libra-governor install
libra-governor doctor
```

## Quickstart (5 minutes)

1. Run the install steps above.
2. Restart Claude Code (or start a new session) so it picks up the
   `settings.json` change.
3. Open any project in Claude Code and submit a prompt.
4. Watch the statusline (bottom of the Claude Code UI) update within a
   couple of seconds to something like:

   ```
   libra: task a1b2c3d4 | plan e5f6a7b8 | preflight: Preflight complete | recon: 0.03s | remaining P90: 42s | stable
   ```

5. Run `libra-governor doctor` any time to check on things.

That's it — the `balanced` policy preset and no enforcement gateway are
the defaults, and nothing here blocks or interrupts your normal Claude
Code workflow (see "Safe defaults" below).

## Your first preflight, walked through

When you submit a prompt, the `UserPromptSubmit` hook
(`libra-governor hook user-prompt-submit`) sends it to the local daemon,
which:

1. Starts itself on demand if it is not already running (no separate
   "start the daemon" step — see
   [`integrations/claude-code/README.md`](integrations/claude-code/README.md#daemon-lifecycle)).
2. Runs bounded, read-only reconnaissance against your repository (file
   counts, detected build/test tooling — never raw file contents).
3. Drafts a Completion Contract and a probabilistic P50/P80/P90
   cost/time estimate from your own local history (or an honestly
   labeled cold-start estimate on a fresh install — see
   [`crates/domain/src/estimate.rs`](crates/domain/src/estimate.rs)).
4. Evaluates admission against your active policy preset and reserves
   the Completion Reserve required to *finish* the task, not just start
   it.
5. Surfaces the result into Claude's own context (the preflight summary)
   and the statusline.

As you work, `PostToolUse` cheaply counts tool calls, and if the run
materially deviates from what the plan implied (a possible tool-call
loop, or a call count history says is atypical), the daemon
automatically replans — recomputing and widening the remaining estimate,
subject to a cooldown/max-replan budget — and the statusline shows this
(`replanned x2`, or `awaiting approval` once that budget is exhausted).
See
[`crates/domain/src/replan.rs`](crates/domain/src/replan.rs) for exactly
what signals this is (and honestly is not) able to detect from Claude
Code's hook payloads. On `Stop`, the session's task is finalized into an
`ExecutionReceipt` — the estimate-vs-actual record used for calibration
(`libra-governor calibration report`).

## Policy presets

Four named presets, defined in
[`crates/domain/src/policy.rs`](crates/domain/src/policy.rs):
`balanced` (the default), `deadline_first`, `cost_first`, and
`strict_budget`. Select one (and optionally scale its resource/time
target) via an optional `config.json` in the daemon's state directory —
full field reference in
[`integrations/claude-code/README.md`](integrations/claude-code/README.md#configuring-the-daemon-configjson).
Absence of `config.json` is the normal case; every existing install
without one keeps the `balanced` default unchanged. `libra-governor
doctor` reports which preset is actually active.

## Statusline and replanning

The statusline is the visible explanation channel for what the daemon
has decided — not something buried in a log file you never open. It
shows the current task id, plan id, preflight/recon status, the current
remaining P90 estimate, and replan state (`stable`, `replanned xN`, or
`awaiting approval`). See
[`crates/cli/src/statusline.rs`](crates/cli/src/statusline.rs) for the
exact render logic, which is unit-tested independently of a live socket.

## Diagnostics: `libra-governor doctor`

A read-only diagnostic snapshot — never spawns the daemon, never mutates
anything, never prints a secret value (only presence/absence — see
"Security & privacy" below):

```bash
libra-governor doctor          # human-readable
libra-governor doctor --json   # machine-readable: {"findings": [...], "daemon": {...} | null}
```

It reports, combining local file checks with a `Request::Doctor` round
trip to the daemon when one is reachable:

- Daemon availability and version, and whether its protocol version
  matches this CLI's own (a version-skewed daemon is reported as an
  error naming the fix — restart it).
- SQLite ledger schema version vs. this build's latest known migration.
- Claude Code hook/statusline wiring (`~/.claude/settings.json`).
- The active admission policy preset.
- Gateway configuration presence, whether it is actually running, and
  its honest capability tier (see "Enforcement gateway" in
  [`integrations/claude-code/README.md`](integrations/claude-code/README.md#enforcement-gateway-horo-1144--optional-off-by-default)).
- `config.json` presence and validity, checked both locally (so a fresh
  install with no daemon running yet still catches a corrupt file) and,
  when a daemon is reachable, against what it actually loaded at
  startup — a present-but-invalid file is flagged as an error either way
  (the daemon is silently running on its hardcoded defaults until this
  is fixed).
- Stale config: whether a currently-running daemon's in-memory policy
  preset/gateway presence still matches what's on disk right now — an
  edited `config.json` with no daemon restart since is flagged as an
  error naming the fix (restart it).
- Telemetry posture (see "Security & privacy" below).

Exit code `0` unless at least one finding is error-severity — a daemon
that simply has not started yet (the normal state before your first
prompt) is a warning, not a failure.

## Troubleshooting

Real, observed failure modes and the exact fix — not invented generic
advice:

| Symptom | Cause | Fix |
|---|---|---|
| `doctor` reports a protocol version mismatch | You upgraded the binary while an old daemon was still running (every `PROTOCOL_VERSION` bump has required this — see [`crates/protocol/src/lib.rs`](crates/protocol/src/lib.rs)'s bump history) | `pkill -f "libra-governor daemon run"`; the next hook invocation respawns it |
| Statusline shows `libra: -` | The daemon is not running; `statusline` never spawns one by design (a refreshing statusline spawning a daemon would be a race factory) | Submit a prompt — `hook user-prompt-submit` spawns it on demand |
| `doctor` reports `config.json ... rejected` | A typo or invalid value in `config.json` — full detail is in `daemon.log`, not swallowed | Fix the file (see the field reference in `integrations/claude-code/README.md`); the daemon keeps running on defaults meanwhile, never crashes on this |
| `doctor` reports `stale_config` | You edited `config.json` after the daemon last started, so it's still running on the old values | `pkill -f "libra-governor daemon run"`; the next hook invocation respawns it with the new file |
| `install` says an existing `statusLine` was left untouched | You already had a non-Governor `statusLine` configured in `~/.claude/settings.json` | Decide which one you want; `install` never overwrites a foreign `statusLine`, so wire Governor's manually (see `integrations/claude-code/README.md`) if you want to replace it |
| `doctor` reports the ledger schema is ahead of this binary | A newer daemon build already migrated the database, and you are now running an older CLI/daemon binary | Rebuild/reinstall this binary at the newer version |
| Gateway configured but `doctor` says `not running` | Configuration validation or credential resolution failed at daemon startup (fails open, never takes the daemon down) | Run `libra-governor gateway status` for the exact `disabled_reason`; the daemon keeps serving hooks/statusline normally either way |
| A gateway request is refused with HTTP 403 | The gateway's own admission failed closed (see `x-libra-decision` header) | See "What a refusal looks like" in `integrations/claude-code/README.md` |

## Security & privacy

Truthful, matching the actual implementation — not aspirational copy:

- **What stays local:** full prompt text, source code, and raw tool
  output never leave your machine as part of Libra's own operation —
  this is a structural guarantee, not a policy switch (see
  [`ARCHITECTURE.md`](ARCHITECTURE.md#privacy-boundary)). The ledger's
  schema has no column for a raw prompt, file content, or tool output —
  see `crates/ledger/migrations/0001_init.sql` and
  `libra_governor_domain`'s crate-level docs.
- **What is persisted in SQLite** (`~/.local/state/libra-governor/ledger.sqlite3`,
  owner-only `0700`/`0600` permissions): task/plan/contract structure,
  estimates, execution receipts (tool-call counts, duration, outcome),
  reservation/settlement ledger entries, and — only when the optional
  gateway is enabled — a `gateway_requests` provenance row per proxied
  request that has **no column for a body, a header, a prompt, or tool
  output** (see `crates/ledger/migrations/0007_gateway_requests.sql`).
- **What may pass through the optional gateway:** only traffic you
  explicitly route through it (`ANTHROPIC_BASE_URL` pointed at the
  gateway's loopback address). It is off by default
  (`DaemonConfig.gateway: None`). Even when enabled, it enforces
  provider-spend limits — it does not inspect, log, or exfiltrate
  request/response bodies at any level; there is deliberately no debug
  body-dump switch anywhere in the code.
- **What is never uploaded by default:** everything. There is no
  telemetry code path anywhere in this repository, and no Team Alpha /
  cross-machine sync exists yet — `libra-governor doctor`'s telemetry
  finding reflects this as a real, observed absence, not an aspiration.
- **Logs and receipts:** `daemon.log`
  (`crates/daemon/src/log.rs`) never receives raw prompt text or hook
  payload content. Execution receipts record structural facts (counts,
  durations, outcome) never the content that produced them.
- **API/BYOK vs. subscription enforcement — exact limits:** reusing
  HORO-1144's model rather than re-describing it inconsistently — see
  `EnforcementCapabilities`/`EnforcementTier` in
  [`crates/domain/src/capability.rs`](crates/domain/src/capability.rs)
  and the capability-tier table in
  [`integrations/claude-code/README.md`](integrations/claude-code/README.md#capability-tiers--what-each-mode-can-honestly-claim).
  In short: a Governor-held API/BYOK credential gets a genuine pre-spend
  monetary hard cap; a forwarded subscription credential gets exact
  token observation but **no** monetary cap (the provider does not
  expose that quota's accounting to us); no gateway gets neither — hooks
  are advisory only. `libra-governor gateway status` and
  `libra-governor doctor` report which one your deployment actually is.
- **How to disable/remove the integration:** see "Uninstall" below.

## Safe defaults

Advisory behavior never unexpectedly blocks an existing Claude Code
workflow: every hook is advisory-only (no `{"decision": "block", ...}`
is ever emitted — see
[`integrations/claude-code/README.md`](integrations/claude-code/README.md#what-this-integration-intentionally-does-not-do-yet)),
and hard enforcement requires you to deliberately configure and start
the optional gateway with a compatible credential/provider
configuration — configuration validation *refuses to start* a
combination that would overclaim a monetary cap it cannot honor (see
`libra_governor_gateway::config::validate`). Conflicting or invalid
configuration is diagnosed (`libra-governor doctor`, `daemon.log`)
rather than silently overwritten. `libra-governor install` merges into
`~/.claude/settings.json` — it never replaces the file wholesale; see
`crates/cli/src/claude_settings.rs` for the exact safe read-modify-write
algorithm.

## Uninstall

```bash
libra-governor uninstall          # removes settings.json keys only; asks before deleting state
libra-governor uninstall --yes    # also deletes the state directory without prompting
```

Removes exactly what this integration's own installer added:

1. The `UserPromptSubmit`/`PostToolUse`/`Stop` hook entries,
   `statusLine`, and — if present — the gateway's `env.ANTHROPIC_BASE_URL`
   (loopback-only) and `apiKeyHelper` keys from `~/.claude/settings.json`.
   Every other key in that file — anything from another tool, or your
   own hand edits — is preserved with its original value untouched (see
   the ownership predicate in `crates/cli/src/claude_settings.rs`, and
   its test suite that seeds foreign content and asserts every foreign
   value survives). The file itself is rewritten as pretty-printed JSON
   with keys in alphabetical order, so the surrounding bytes — key
   order, exact whitespace — are not preserved verbatim, only the
   values. A timestamped backup of the pre-edit file is written before
   any change if you need the original byte-for-byte.
2. The state directory (`ledger.sqlite3`, `daemon.sock`, `daemon.log`,
   `config.json`, `gateway.token`) — **only** with `--yes` or an
   interactive "yes" confirmation, since it holds your only local record
   of estimate-vs-actual calibration history and deleting it is
   unrecoverable.
3. Nothing else. The daemon binary itself is never deleted by
   `uninstall` — it was put in place by `cargo install`, so only `cargo
   uninstall libra-governor-cli` keeps cargo's own package bookkeeping
   consistent. When `install`'s own marker file proves this exact binary
   path was installed by `libra-governor install`, `uninstall` prints
   that command for you to run; a binary you built or installed some
   other way is never mentioned.

Run `libra-governor doctor` afterward to confirm — it will report
"not installed."

## Known limitations

- **Developer Preview, not a general release.** No published binaries
  exist yet (see "Install" above); `cargo install` from a local clone is
  the real v0.0.1 path.
- **The gateway's session-binding header is unverified against live
  Claude Code traffic** — see
  [`integrations/claude-code/README.md`](integrations/claude-code/README.md#known-limitation-the-session-binding-header-is-unverified).
- **No automated Completion Contract verification** — a task is never
  inferred "done" merely because the model stopped talking (MVP scope).
- **No cross-machine sync or telemetry** — every install's history is
  local to that machine.
- Further limitations (task classification, cross-session merge, MCP
  surface, Contract correction UX) are listed in
  [`integrations/claude-code/README.md`](integrations/claude-code/README.md#what-this-integration-intentionally-does-not-do-yet).

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for branch/commit/PR
conventions, and [`CLAUDE.md`](CLAUDE.md) for repository-specific agent
instructions. Build and test commands:

```bash
cargo build --workspace
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

A real, runnable install/uninstall lifecycle smoke test lives at
[`scripts/smoke-test.sh`](scripts/smoke-test.sh) (not wired into CI —
see that script's own docs for why).
