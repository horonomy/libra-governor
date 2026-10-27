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
| HORO-2248 first run, denial, statusline | ACCEPTED (scoped functional) | Codex QA HORO-2186 c29269 PASS on `a3e9ec9`; the >=70 experience gate stays open |
| HORO-2246 bounded Stop flush wait | ACCEPTED (bounded orchestration only) | c29269: delayed Stop measured once, missing transcript waits ~1.5 s then flat hold once. Not a fresh real-host sample; interactive TTY unmeasured |
| HORO-2250 M3 prune then replay | ACCEPTED (reused evidence) | c29269 PASS_REUSED from earlier coordinator logs, not newly run; main-turn Stop replay after prune has a local test commit that is not yet run |
| In-product disclosure | ACCEPTED | c29269: `status` and `doctor` show all four limitations |
| Whole-feature Cooperative Preview | ACCEPTED (scoped functional PASS) | c29269 "scoped functional PASS; overall PARTIAL / no release sign-off"; no FAIL. Not covered: DST-gap/fold zone and soft-limit warn-only were not reported; multi-daemon writers; power-loss/mid-COMMIT |
| Independent Product Experience score | REQUIRED_OPEN | c29269 reports NOT_VERIFIED (rubric not located by QA); see the rubric section |
| Jira Fix Version / Goal (HORO-1752) | OWNER_AUTHORIZATION_REQUIRED | Jira admin must create it (comments 28544, 28725) |
| Release checklist (HORO-1857) | REQUIRED_OPEN | see below |
| S11, Codex gate, OpenAI gateway, remote upload | APPROVED_FUTURE_VERSION | provenance-limited, see ADR-0026 |
| Real-host / ProductionObserved evidence | OWNER_AUTHORIZATION_REQUIRED | all evidence is `TestOwnedLive` |
| Tag, publish, deploy | OWNER_AUTHORIZATION_REQUIRED | SHA-named founder GO |

## HORO-2250 residual hardening verdict

Release-owner classification against `main` source and the ticket text,
subject to the founder's override. Vocabulary: **blocking**, **accepted
limitation** (disclosed, no work for v0.0.4), **approved future work**.
**Blocking: none of the 13.** HORO-2250 itself says the backlog is "not
release-blocking by themselves". Findings marked (source) come from a reading
of the code, not a new test.

| # | Item | Verdict | Basis |
|---|---|---|---|
| 1 | Post-/compact usage unmeasured | accepted limitation | unmeasured windows settle at the flat hold; `crates/daemon/src/usage/priced.rs` says so; the cap is a disclosed estimate |
| 2 | HORO-2179 test gaps | approved future work | HORO-2179 itself: "Non-blocking test gaps ... not release blockers" (a statusline identifier-absence test and a 0015 migration test) |
| 3 | Codex route has no gate | accepted limitation | disclosed in `status`, `doctor`, README |
| 4 | Quoted/renamed/Windows hook commands unrecognised | approved future work | `claude_settings.rs` basename match; uninstall may miss such a hook; Windows unsupported |
| 5 | Foreign-dir subagent transcript paths | approved future work | same-user local process, 0700 state dir (source) |
| 6 | Reinstall from another path adds a second hook set | approved future work | duplicate Admit/Stop are idempotent by prompt id (M1 and the HORO-2258 quiet-duplicate fix); a latency/noise cost, no double charge found (source) |
| 7 | Derived-lane counter rows never reclaimed | approved future work | small row growth, no money effect (ADR-0026 acknowledges it) |
| 8 | Prompt-agent / Stop-no-agent asymmetry | accepted limitation | `resolve_finalize_lane` in `server.rs` is documented as one-directional; it concerns the legacy per-session lane, while pool holds are keyed by session id (source, ADR-0026 identity section) |
| 9 | Legacy watermark rows from unmerged branch heads | accepted limitation | migration 0022 is new in unreleased v0.0.4, so only development ledgers carry such rows |
| 10 | `init --force` replaces the file with no backup | accepted limitation | the help text says so; config only, no ledger data |
| 11 | `ensure_state_dir` chmod follows a symlink | approved future work | pre-existing, rare trigger |
| 12 | Interactive TTY flush timing unmeasured | accepted limitation | over-count at the flat hold, fails safe; real-host run needs separate authorization |
| 13 | No `clock_anomaly` repair command | accepted limitation | the denial text says "correct the clock to continue" (`budget_gate.rs`) |

New, from Codex c29269 (informational): `shared-budget export` can show the
pool row's `capacity` (for example 300) after the configured cap was raised
(500). Status and admission use the live cap. It is export metadata, not an
accounting error; tracked as a usability follow-up.

## Product Experience gate

**Canonical rubric.** The five-dimension, 100-point rubric comes from a
human-origin directive in the release-owner session, 2026-10-25 14:31:36Z
("FOUNDER DIRECTIVE - PRODUCT COMPLETION GATE", the "Phase 3 Product
Experience Quality Gate" section). Same provenance limit as the scope
decision: session channel, not independently attested. It is not stored in any
Libra repo file or Jira rubric table; this section records it.

| Dimension | Max |
|---|---|
| A. Original core value / outcomes | 30 |
| B. Real-host Golden Journey | 20 |
| C. Setup / day-to-day usability | 25 |
| D. Reliability / security / correctness | 15 |
| E. Diagnosis / explanation / recovery | 10 |

Thresholds: internal dogfood minimum 60, Preview minimum 70. Rules: score only
observed, traceable evidence; missing evidence is NOT_VERIFIED; no
hypothetical points; a producer score is not independent QA; synthetic, mock
and real-host evidence are labelled separately; passing 70 does not override
an unresolved critical security, financial or data-integrity defect.

**Producer score (not independent): 72/100** = A 23, B 13, C 17, D 12, E 7.
Evidence is `TestOwnedLive` (real Claude Code 2.1.306 `-p` against a fake
upstream, isolated HOME), recorded in the producer reports on HORO-2258 and
HORO-2186. B has no interactive TTY, real-provider or clean-machine evidence.
That evidence is not new; no points were added after it.

**Independent score: NOT_VERIFIED.** Codex c29269 did not locate the weights
and did not score. Codex needs to score independently against this rubric
using its own c29269 observations; the producer must not set that number.

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
