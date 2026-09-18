# v0.0.1 Release Gate Evidence (HORO-1152)

This directory holds the real, committed evidence for the v0.0.1
"Claude Code Developer Preview" release gate, following the same
pattern established by `experiments/mvp1_validation/` (HORO-1127),
`experiments/mvp2_calibration/` (HORO-1132), and `experiments/mvp3_gate/`
(HORO-1146): real subprocesses of the real compiled `libra-governor`
binary, real on-disk SQLite ledgers, a real daemon process, no
in-process mocking of the product code under test, no fabricated
numbers.

Branch: `v0.0.1/HORO-1152/release_gate`, based on `origin/main` at merge
of HORO-1150 ("Productize the Claude Code Developer Preview": `install`
/`doctor`/`uninstall` CLI commands, `scripts/install.sh`,
`scripts/smoke-test.sh`, root `README.md`).

## Scope

This ticket does not make the GO/ITERATE/PIVOT/KILL decision and does
not perform the tag/release — that is reserved for the coordinator after
reviewing this evidence. This directory's job is real, honestly-labeled
evidence and a factual summary against the 11-item verification matrix
and documentation review the ticket specifies.

## Load-bearing methodology note: "fresh environment" here means an
isolated profile on this same machine, not a second physical machine

There is no separate physical or VM machine available in this
environment. Everywhere the gate matrix calls for "a fresh user
environment," this evidence set uses a fully isolated fake `$HOME` on
**this same development machine**: a fresh temp directory standing in
for `$HOME`, a fresh `~/.claude`-equivalent `settings.json` (absent or
seeded with realistic foreign content, per scenario), and a fresh state
directory under `<fake $HOME>/.local/state/libra-governor` — with the
actual documented `cargo install --path crates/cli --locked` /
`scripts/install.sh` mechanics genuinely invoked, not shortcut. This is
the same honest approximation the coordinator already told Jira it
would use for this class of evidence; it is disclosed here explicitly
rather than overstated as "tested on a separate machine."

Two narrow, disclosed compromises inside that isolation, both aimed at
keeping the install genuinely real while not requiring a fresh network
fetch of the entire crates.io index per scenario:

- `$FAKE_HOME/.cargo/registry` and `$FAKE_HOME/.cargo/git` are symlinked
  to this machine's real cargo cache — **only the download cache is
  shared**; the install root (`$FAKE_HOME/.cargo/bin`) is genuinely
  fresh per profile, and every `cargo install` in this gate is a real
  compile against the real source tree at the commit under test.
- `RUSTUP_HOME` is left pointed at the real rustup installation (rustup
  resolves its toolchain list from there regardless of `$HOME`) — the
  toolchain binaries are shared across profiles; nothing about the
  product build or install path is shortcut by this.

`LIBRA_GOVERNOR_STATE_DIR` and `LIBRA_GOVERNOR_CLAUDE_DIR` overrides
(primarily meant for this repo's own unit tests) were explicitly
**unset** for every scenario except where a scenario name says
otherwise — every state/settings path in this evidence set falls out of
the real `$HOME`-based resolution order a genuine user gets, not the
test-only override.

## Driver

`run_v001_gate.py` drives items 1–10 end to end against the real
binary; item 11 (privacy/leak check) is a separate, standalone script,
`results/privacy_leak_check.sh`, run against the artifacts items 1–10
produced (see "Why item 11 is separate" in `results/privacy_leak_check.md`).
Item 7's upgrade scenario required one manual follow-up step (killing a
stale old-protocol daemon) documented inline in
`results/07_upgrade_from_mvp3.txt` rather than folded silently into the
script.

```bash
export CARGO_TARGET_DIR=/tmp/libra-horo1152-target-$$
python3 experiments/v001_gate/run_v001_gate.py --work-root /tmp/libra-horo1152-work
bash experiments/v001_gate/results/privacy_leak_check.sh
```

## Results index

| # | Item | Result file | Verdict |
|---|---|---|---|
| 1 | Install from documented release path | `results/01_install_from_documented_path.txt` | PASS |
| 2 | First `doctor` | `results/02_first_doctor.txt` | PASS |
| 3 | Claude Code integration bootstrap (absent + foreign settings.json) | `results/03a_install_cmd_absent_settings.txt`, `results/03b_install_cmd_foreign_settings.txt` | PASS |
| 4 | First bounded preflight | `results/04_first_bounded_preflight.txt` | PASS |
| 5 | Full governed task + Execution Receipt | `results/05_full_task_execution_receipt.txt` | PASS |
| 6 | Restart/resume | `results/06_restart_resume.txt` | PASS |
| 7 | Upgrade from `mvp-3.0` | `results/07_upgrade_from_mvp3.txt` | PASS (with a real UX finding — see below) |
| 8 | Uninstall/reinstall | `results/08_uninstall_reinstall.txt` | PASS |
| 9 | No damage to unrelated Claude configuration | `results/09_foreign_settings_full_cycle.txt` | PASS on values; real byte-identity defect found — see below |
| 10 | Capability tier / subscription limitation copy | `results/10_capability_tier_text.txt` | PASS |
| 11 | No secret/private data leakage | `results/privacy_leak_check.md`, `results/privacy_leak_check_run.txt` | PASS |

Documentation review: `results/doc_review.md`. `cargo fmt`/`clippy`/
`build`/`test --workspace`: `results/fmt_check.txt`,
`results/clippy_check.txt`, `results/build_workspace.txt`,
`results/full_workspace_test_run.txt` — all green, 466 tests passed, 0
failed. `scripts/smoke-test.sh` also run for real, independently
corroborating items 3b/8/9: `results/smoke_test_run.txt`.
`results/scenario_results.json` is the driver's own machine-readable
pass/fail summary.

## Real findings (not suppressed)

1. **Upgrade-workflow UX (item 7):** upgrading the installed binary in
   place does not restart an already-running daemon process spawned by
   the old binary. The next `doctor`/hook call correctly reports a
   protocol-version mismatch and names the exact fix (`pkill -f
   "libra-governor daemon run"`) — this is documented behavior (see the
   troubleshooting table in `README.md`, and it was independently
   verified live in this gate, not merely read), not a silent failure.
   State (the ledger, receipts, settings.json wiring) genuinely survives
   the upgrade either way. Recommendation: no code change required; the
   documentation already covers this correctly.
2. **Uninstall is not byte-for-byte on the full cycle (item 9):** every
   foreign `settings.json` key's *value* survives a full
   install→uninstall cycle, but the file is not byte-identical
   afterward — JSON re-serialization alphabetizes top-level and nested
   object keys. `README.md`'s "Uninstall" section currently promises
   "byte-for-byte" preservation, which overstates what was actually
   observed. Recommendation: either soften the doc claim to
   "value-for-value" (matching what the code and its own unit tests
   actually guarantee) or, if byte-for-byte is the intended contract,
   file a follow-up to preserve original key order/formatting on
   rewrite.

See `results/doc_review.md` for the full documentation-review findings,
including which troubleshooting-table rows this gate verified live vs.
which remain unverified by this specific gate (and were left as
unverified rather than dressed up as confirmed).
