# v0.0.4 release-candidate packet (Cooperative Preview)

Status: **draft, not a release decision.** Nothing is tagged, published or
deployed. `READY_FOR_RELEASE=false`, `RELEASE_APPROVED=false`,
`RELEASED=false`. Candidate evaluation baseline: `main` at
`a3e9ec9dfef865b010992f0e6016f46ff799ba5a` (PR #235). The final candidate SHA
is whatever `main` is when the founder names it in a SHA-pinned GO
([ADR-0025](adr/0025-v0-0-4-scope-evidence-and-exclusions.md) step 3); any
later merge invalidates this baseline.

## Scope

v0.0.4 ships the Claude Code shared monthly pool as a labelled **Cooperative
Preview** ([ADR-0026](adr/0026-local-monthly-shared-budget.md), "Founder
version-scope decision" and its provenance table). The original strict
cross-host hard-cap guarantee is **not** delivered. S11 (gateway per-request
cap) and the Codex budget gate are designed, off by default, and not enabled.

## Acceptance matrix

| Gate | Class | Evidence |
|---|---|---|
| Stale-Admit / replay chain | ACCEPTED | HORO-2250 c29181 (PR #224/#226), c29205 (PR #227 SCOPED PASS) |
| Four UX fixes | ACCEPTED | HORO-2258 c29093 (PR #223) |
| HORO-2248 first run, denial, statusline | IMPLEMENTED_QA_PENDING | PR #221; producer evidence c29039 only |
| HORO-2246 bounded Stop flush wait | IMPLEMENTED_QA_PENDING | `crates/cli/tests/shared_budget_flush_wait.rs`, daemon `shared_budget_settlement.rs`; synthetic lag only |
| HORO-2250 M3 prune then replay | IMPLEMENTED_QA_PENDING | `crates/ledger/tests/shared_pool_retention.rs`, daemon settlement tests (migration 0023, protocol v24) |
| In-product disclosure | IMPLEMENTED_QA_PENDING | `status`/`doctor` `NOT_COVERED` (PR #235), README, ADR-0026 |
| Whole-feature Cooperative Preview | REQUIRED_OPEN | independent QA requested, none yet |
| Independent Product Experience score | REQUIRED_OPEN | 72/100 is producer evidence only |
| Jira Fix Version / Goal (HORO-1752) | OWNER_AUTHORIZATION_REQUIRED | Jira admin must create it (comments 28544, 28725) |
| Release checklist (HORO-1857) | REQUIRED_OPEN | see below |
| S11, Codex gate, OpenAI gateway, remote upload | APPROVED_FUTURE_VERSION | provenance-limited, see ADR-0026 |
| Real-host / ProductionObserved evidence | OWNER_AUTHORIZATION_REQUIRED | all evidence is `TestOwnedLive` |
| Tag, publish, deploy | OWNER_AUTHORIZATION_REQUIRED | SHA-named founder GO |

## HORO-2250 residual hardening triage (proposal, from code review)

None of the 13 items is release-blocking on code evidence; HORO-2250 itself
calls the list "not release-blocking by themselves". Each is a disclosed
limitation or a future fix. The classification is a proposal until the
owner confirms it.

| Item | Class | Note |
|---|---|---|
| Post-/compact usage unverified on a real host | APPROVED_FUTURE_VERSION | unmeasured windows settle at the flat hold; the cap is an estimate |
| HORO-2179 test gaps | UNVERIFIED | ticket not opened |
| Codex route has no gate | APPROVED_FUTURE_VERSION | disclosed in `status`/`doctor`/README |
| Quoted/renamed/Windows hook commands not recognised | APPROVED_FUTURE_VERSION | uninstall may miss such a hook; Windows unsupported |
| Daemon accepts any absolute subagent transcript path | APPROVED_FUTURE_VERSION | same-user local process, 0700 state dir |
| Reinstall from a different path adds a second hook set | APPROVED_FUTURE_VERSION | no double-run test exists; escalate if one shows a double charge |
| Derived-lane counter rows never reclaimed | APPROVED_FUTURE_VERSION | small row growth, no money effect |
| Prompt-agent / Stop asymmetry | UNVERIFIED | not located |
| Legacy watermark rows | UNVERIFIED | probably dev ledgers only |
| `init --force` replaces the file without backup | APPROVED_FUTURE_VERSION | help text discloses it |
| `ensure_state_dir` chmod follows a symlink | APPROVED_FUTURE_VERSION | rare trigger, pre-existing |
| Interactive TTY flush timing unmeasured | APPROVED_FUTURE_VERSION | over-count that fails safe at the flat hold |
| No `clock_anomaly` repair command | APPROVED_FUTURE_VERSION | denial text says "correct the clock" |

## Known limitations (user-facing)

- Admission-bounded (cooperative), not a strict monetary cap; in-turn
  overshoot is possible.
- Codex is ungated; there is no gateway hard cap.
- Amounts are Libra-accounted list-price-equivalent USD, never the provider
  bill; the 1-hour cache tier is under-counted and some Haiku prompts are
  over-counted (ADR-0026, "Known pricing limitations").
- Only Claude Code sessions and subagents whose settlement hooks run and
  whose transcripts can be read and attributed are counted.
- Evidence is `TestOwnedLive` (real binaries, synthetic transcripts); nothing
  is observed on a real host.

## Release checklist state

State comes from `docs/quota-observation/release-checklist.md` and ADR-0025
steps 1-7. Its cited SHAs (`5aec5a1`, `fae8309`) are stale.

| Step | State |
|---|---|
| Workspace version 0.0.4 (`Cargo.toml`) | done |
| `scripts/check-release-tag.sh v0.0.4` | script present; not run for a real tag |
| Hosted CI green on the final SHA | re-evaluate on the pinned SHA |
| `workflow_dispatch` pre-verify of `release.yml` | not done; needs a SHA-named GO (public attestations) |
| aarch64 artifact checks (sha256, attestation, `--version`) | not done on the final SHA |
| Clean-machine install smoke (Apple Silicon) | not done on the final SHA |
| x86_64 and Linux install tests | out of scope for an Apple Silicon Preview; assets must be labelled unsupported-unverified, or the matrix restricted by a reviewed change |
| Release notes | draft below; no CHANGELOG file exists |
| Rollback on a published artifact | partial |
| Founder GO / ADR-0025 acceptance | not granted |

Runnable now without publishing: the tag-check script, and a local
`--release --locked` aarch64 build with `--version` and a sandboxed install
smoke. That is local-equivalent evidence, not a CI-produced artifact.

## Release notes draft

> **Libra Governor v0.0.4 — Cooperative Preview (Apple Silicon macOS).**
> A local monthly budget shared by your Claude Code sessions and subagents.
> Prompts are admitted or refused up front against a monthly USD cap you set
> (`libra-governor shared-budget init`). This is admission-bounded
> (cooperative), not a strict spending cap: a turn can cost more than its
> hold. Codex is not gated. Amounts are Libra's list-price-equivalent
> estimates, not your provider bill. Only the Apple Silicon (aarch64) macOS
> build is supported; other assets are unverified. Evidence so far comes from
> test-owned runs, not real-host observation.

## Rollback

The budget is opt-in and file-driven: delete `shared-budget.json` in the
state directory to turn it off (the daemon re-reads it on every decision).
`libra-governor uninstall` removes the hooks. Rollback of a published
artifact is not yet evidenced.
