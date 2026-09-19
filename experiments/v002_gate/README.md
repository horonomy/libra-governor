# v0.0.2 Dogfood / Release-Readiness Evidence (HORO-1169)

Real internal dogfood evidence for the v0.0.2 Developer Preview, per the
founder's 2026-09-19 validation-policy decision (see HORO-1154): the
external-user-recruitment hard gate is replaced by genuine internal
usage evidence as the release-readiness check.

`run_v002_gate.py` extends `experiments/v001_gate/run_v001_gate.py`
(imported via `importlib`, not duplicated — same reuse pattern as
`mvp3_gate`'s `_BaseMatrix`) with the scenarios the v0.0.1 gate didn't
need to cover. Every scenario is a real subprocess of the real compiled
binary against real on-disk state — no in-process mocking, no fabricated
numbers, same discipline as every prior gate in this repo.

```bash
python3 experiments/v002_gate/run_v002_gate.py --work-root /tmp/libra-v002-work
```

## Results index

| # | Item | Verdict |
|---|---|---|
| 1–10 | Base v0.0.1 matrix, re-run against the current tree (install, doctor, bootstrap, preflight, full task, restart/resume, uninstall/reinstall, foreign-settings preservation, capability-tier text) | PASS |
| 11 | Privacy/leak check | PASS |
| 12 | Long-running session (30 real tool-use events, one session) | PASS |
| 13 | 8 concurrent sessions against one daemon | PASS |
| 14 | Failure/recovery — kill the daemon mid-task, confirm coherent state | PASS |
| 15 | Invalid/missing `config.json` (4 cases) degrades safely | PASS |
| 16 | Daemon RSS sanity bound over 5 real tasks | PASS |
| 17 | Real upgrade from the tagged `v0.0.1` release with a populated ledger | PASS (real finding, fixed — see below) |
| 18 | Real Codex CLI dogfood (`scripts/codex-smoke.sh`, real installed `codex-cli`) | PASS |
| 19 | Self-healing daemon upgrade (proves the item 17 fix for its real applicability) | PASS |

`results/scenario_results.json` is the driver's own machine-readable
pass/fail summary.

## Real finding (fixed, not suppressed)

**Item 17** found that upgrading `libra-governor` while its old daemon
(still speaking the old `PROTOCOL_VERSION`) is running left every
subsequent hook call failing open ("proceeding without governance")
until a human noticed `doctor`'s warning and manually killed the stale
process. This is the same class of issue v0.0.1's own gate disclosed at
its item 7 (upgrading from `mvp-3.0`) — that gate recommended no code
change since it predated any fix. This time, a real fix was made:
`crates/daemon/src/server.rs`'s `handle_connection` now signals `serve`'s
accept loop to shut the daemon down after replying to a client that
speaks a *newer* protocol version than it does. The next hook
invocation's existing `ensure_daemon_connection` spawn-on-absence logic
then transparently starts a fresh, current daemon — self-healing, using
the exact same code path a totally fresh install already relies on.

**This fix cannot retroactively help the specific v0.0.1 → v0.0.2
transition** — the running v0.0.1 daemon process was compiled before the
fix existed, so it cannot execute code it doesn't have. Item 17 discloses
this honestly: the real, permanent, one-time limitation for exactly this
one upgrade is that it needs one manual `pkill -f "libra-governor daemon
run"` (or a normal restart) if Claude Code was already running before
upgrading — documented in `README.md`'s known limitations and in
`doctor`'s own diagnostic message. **Every upgrade from v0.0.2 onward
self-heals automatically** — item 19 proves that mechanism for real,
end to end, against a running current daemon: sends a synthetic
newer-protocol request directly over the socket, confirms the daemon
logs its own shutdown and exits, and confirms the very next hook
invocation transparently governs normally again through a freshly
spawned daemon.

Two new regression tests back this in `crates/daemon/src/server.rs`:
`handle_connection_signals_shutdown_for_a_newer_client_protocol` and
`handle_connection_does_not_shut_down_for_an_older_client_protocol`.
