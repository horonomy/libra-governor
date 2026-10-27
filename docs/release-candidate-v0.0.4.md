# v0.0.4 release-candidate packet (Cooperative Preview)

Status: **draft, not a release decision.** Nothing is tagged, published or
deployed. `READY_FOR_RELEASE=false`, `RELEASE_APPROVED=false`,
`RELEASED=false`. Independent QA evaluated code at
`a3e9ec9dfef865b010992f0e6016f46ff799ba5a` (PR #235, functional, c29269) and
`main` at `c38750d33fc5e9aafa47f71d01ec4a20b9746023` (experience score,
c29293). Later merges (for example PR #238, the aarch64-only release matrix)
are not covered by either verdict. The final candidate SHA
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
| HORO-2248 first run, denial, statusline | ACCEPTED (scoped functional) | Codex QA HORO-2186 c29269 PASS on `a3e9ec9`; the >=70 experience gate is met by c29293 (73) |
| HORO-2246 bounded Stop flush wait | ACCEPTED (bounded orchestration only) | c29269: delayed Stop measured once, missing transcript waits ~1.5 s then flat hold once. Not a fresh real-host sample; interactive TTY unmeasured |
| HORO-2250 M3 prune then replay | ACCEPTED (reused evidence) | c29269 PASS_REUSED from earlier coordinator logs, not newly run; main-turn Stop replay after prune is covered by a test in PR #239 that first ran green on hosted CI but could not tell the guard from its absence (independent review), so it was strengthened; the strengthened form ran green on macOS and Ubuntu and is merged (PR #239, `b9cf2b8`) |
| In-product disclosure | ACCEPTED | c29269: `status` and `doctor` show all four limitations |
| Whole-feature Cooperative Preview | ACCEPTED (scoped functional PASS) | c29269 "scoped functional PASS; overall PARTIAL / no release sign-off"; no FAIL. c29269 verifies warn-only soft limits. DST gap and fold at a month's first midnight are covered by domain unit tests (`calendar.rs`, America/Asuncion and America/Havana), not by an end-to-end Codex run. Not covered: multi-daemon writers; power-loss/mid-COMMIT |
| Independent Product Experience score | ACCEPTED (bounded scope) | **73/100**, Codex c29293 on `c38750d`; Preview threshold 70 met; overall verdict stays PARTIAL; see the Product Experience gate |
| Jira Fix Version / Goal (HORO-1752) | OWNER_AUTHORIZATION_REQUIRED | Jira admin must create it (comments 28544, 28725) |
| Release checklist (HORO-1857) | REQUIRED_OPEN | see below |
| S11, Codex gate, OpenAI gateway, remote upload | APPROVED_FUTURE_VERSION | provenance-limited, see ADR-0026 |
| Real-host / ProductionObserved evidence | OWNER_AUTHORIZATION_REQUIRED | all evidence is `TestOwnedLive` |
| Tag, publish, deploy | OWNER_AUTHORIZATION_REQUIRED | SHA-named founder GO |

## HORO-2250 residual hardening verdict

Source-and-test audit of each item against `main` (read-only, no new
dynamic tests), revised after the founder directive that the earlier
classification is not proof of completion. Vocabulary: **blocking**,
**accepted limitation** (disclosed), **approved future work**. "Fix pending"
means a defect found by the audit with a PR open; it blocks the Preview until
that PR merges and passes review. Codex c29293 notes the producer's future
labels are not blanket security waivers; none is treated as one here.

| # | Item | Verdict | Evidence and impact |
|---|---|---|---|
| 1 | Post-/compact usage unmeasured | accepted limitation, disclosure added (PR #244, merged `727b541`) | No Claude compaction handling exists. Outcomes are unobserved: summarizer lines may be sidechain-flagged and silently excluded (an under-count), or book the flat hold. "Fails safe" was wrong. No test. Real-host check needs authorization. |
| 2 | HORO-2179 test gaps | approved future work | A 0015 migration test already exists (`migrations.rs`, `migration_15_...`); a statusline session/agent-id absence test does not. Privacy regression guard only. |
| 3 | Codex route has no gate | accepted limitation | Codex is never charged, blocked or admitted: Prompt/PreToolUse skip non-Claude agents and a Codex Stop settles nothing. Isolation rests on session ids not colliding, not a provider check. The disclosure in `status`, `doctor` and README is accurate. Optional hardening: skip `settle_turn` for non-Claude providers. |
| 4 | Quoted/renamed/Windows hook commands | approved future work | A hand-quoted or renamed command is missed by uninstall and can be duplicated by install. **Sub-case found: a binary path containing whitespace or a shell metacharacter wrote an unquoted hook that silently never ran (every prompt ungated) while doctor saw the hooks as wired. Fixed: PR #242 (merged `8da5e61`) refuses such a path at install.** |
| 5 | Any absolute subagent transcript path accepted | approved future work | Same-user only (socket 0600 in a 0700 dir). Content leak is limited to a short model name in status. A FIFO or stalled-mount path can block the single-threaded daemon (same-user DoS). Optional: require the path under the transcript's `subagents/` dir. |
| 6 | Reinstall from another path adds a second hook set | approved future work | No double charge or hold found in source (Admit is one hold per lane; Stop replay is keyed by prompt id). Legacy tool-call counts double. An older hook set without the flush wait could win the race and book the flat hold (inferred, untested). |
| 7 | Derived-lane counter rows never reclaimed | approved future work | One small row per session and agent; no money path reads it. |
| 8 | Prompt-agent / Stop-no-agent asymmetry | accepted limitation | Legacy token lane only: a leaked reservation until its 900 s TTL and one lost estimator sample; no USD effect. The shared budget always admits with no agent id. The asymmetric direction is untested. |
| 9 | Legacy watermark rows (migration 0022) | accepted limitation | 0022 was never in a released build (no v0.0.3 tag; v0.0.2 is the latest release; `release/v0.0.3` stops at 0015). Development ledgers should be recreated. |
| 10 | `init --force` replaces the file with no backup | accepted limitation, disclosure added (PR #244) | Ledger and pool state are untouched and the swap is atomic. A forced re-init silently drops hand-written `pricing_overrides`, `subagent_unmeasured_charge_usd_cents` and soft limits not passed again, and resets the turn hold to the default if omitted. Help says only "replaces the whole file". Smallest fix: keep a `.bak` hard link and say what is dropped. |
| 11 | `ensure_state_dir` chmod follows a symlink | approved future work | Same-user only on the default path (`~/.local/state`); a non-default `LIBRA_GOVERNOR_STATE_DIR` into a shared location could make the user chmod a symlink target they own to 0700. No symlink test. Fix: `symlink_metadata` plus an owner check. |
| 12 | Interactive TTY flush timing unmeasured | accepted limitation, disclosure added (PR #244) | Not "fails safe": a partly flushed turn settles short and the remainder is recovered only by a later Stop or SettleTail, and is lost if neither runs. Measured only with `-p`. A real TTY sample needs authorization. |
| 13 | No `clock_anomaly` repair command | accepted limitation, message corrected (PR #244) | Fails closed with no money effect, but "correct the clock" is not actionable once the clock is right; the real remedies (ADR-0026 recovery, or removing `shared-budget.json` to pause the budget) are not named. A forward clock jump also opens next month's pool with a full cap and is undisclosed. |
| 14 | **New: re-prompt renews a live hold with no capacity check** | **fixed: PR #240 merged (`98b112a`)** | After an interrupt (Stop does not fire) the hold stays live; a new prompt renewed it without the check a fresh reservation applies, so turns could continue after other sessions overdrew the pool. Failing-first tests ran red on hosted CI (101 passed, 1 failed); the fix applies the fresh-reservation test to a renewal that names a new prompt id (a prompt-less retry is unchanged, per ADR-0026 "a lower cap never revokes existing holds"); ADR-0026 documents it; two independent review passes; hosted CI green on the PR head. The trigger relies on interrupt behaviour not observed on a real host. |

Other undisclosed or partly disclosed ways spend can exceed the cap (audit,
cited in HORO-2186): a 32 MiB subagent file is permanently booked at the
fallback; the sweep caps (64 files, 128 MiB) can defer or lose records;
PreToolUse fails open when the daemon is down; the 1-hour cache tier,
fast-mode and geo multipliers are under-counted; an over-10x-cap charge is
refused unrecorded. These stay accepted limitations; PR #244 names the main
ones in `status`, `doctor` and the README.

Codex c29269 (informational): `shared-budget export` can show the pool row's
`capacity` (for example 300) after the configured cap was raised (500). Status
and admission use the live cap. It is export metadata, not an accounting error
(HORO-2293).

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

**Independent score: 73/100** (Codex, HORO-2186 comment 29293, on `main`
`c38750d`; evidence `coordinator/20261027-libra-preview-score/`, all hashes
verified). A 24/30, B 8/20, C 20/25, D 12/15, E 9/10. Preview threshold 70
and internal dogfood threshold 60 are met. This is an evidence-weighted
judgment, not a user-study statistic. Overall QA stays **PARTIAL**
(`INDEPENDENT_EXPERIENCE_QA_SIGNED_OFF=true` for the bounded scope,
`FULL_RELEASE_QA_SIGNED_OFF=false`). Passing 70 does not override an
unresolved critical defect; item 14 was such a candidate and is fixed by
PR #240 (no independent re-test yet).

What keeps B at 8/20 and what would raise it: a current-head real-host run,
an interactive TTY and partial-flush sample, post-/compact coverage, and a
real-provider session, each needing separate authorization. C and the rest
are bounded by a warm private fixture rather than a clean-machine install.

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
| x86_64 and Linux install tests | out of scope for an Apple Silicon Preview; the release matrix is restricted to aarch64 (PR #238, pending merge) |
| Release notes | draft below; no CHANGELOG file exists |
| Rollback on a published artifact | partial |
| Founder GO / ADR-0025 acceptance | not granted |

Local-equivalent evidence gathered on 2026-10-27 (not a final-SHA result, and
none of it is attested):

- `scripts/check-release-tag.sh v0.0.4` on `main` `6183602`: exit 0
  ("matches workspace version 0.0.4"); `v0.0.5`: exit 1, as designed.
- CI-built aarch64 artifact from the PR #238 build-only run 40481551611
  (source `34c9b33`, `macos-latest`): `shasum -c` OK, `--version` prints
  `libra-governor 0.0.4`, Mach-O arm64. `gh attestation verify` returns 404,
  as designed: pull-request runs skip attestation.
- Sandboxed lifecycle with that binary, isolated HOME and state dir, a
  foreign `Stop` hook pre-seeded: `install --hooks-only` added 6 hooks and
  wrote a backup; `doctor` reported the hooks wired; `shared-budget init`
  and `status` worked (daemon not started, reported as such); `uninstall`
  removed the hooks, left the state dir (no `--yes`) and restored
  `settings.json` to semantically equal JSON (not byte-identical: it is
  re-serialized). It also printed that the binary "was installed via `cargo
  install`" for a copied binary, a cosmetic misclassification.

Local evidence at code SHA `98b112a16ac79cd4a04b1466ce6e81603d3bfc4c` (under
SPE-109 admission; not CI-produced, not attested, not an independent QA
result):

- `cargo build --release --locked -p libra-governor-cli --bin libra-governor`
  succeeded (aarch64, 21,855,584 bytes, sha256 prefix `8399db17b73dce78`);
  `--version` prints `libra-governor 0.0.4`.
- CLI suites on that tree: `shared_budget_golden_journey` 11/11,
  `shared_budget_flush_wait` 9/9, `shared_budget_hooks` 32/32,
  `shared_budget_init` 11/11, `shared_budget_cmd` 18/18.
- Sandboxed lifecycle with the release binary (isolated HOME and state dir, a
  foreign `Stop` hook pre-seeded): install from a path containing a space is
  refused and `settings.json` is untouched; install from a plain path adds 6
  hooks; `doctor` shows all six wired and the new limits text; `uninstall`
  restores `settings.json` to semantically equal JSON.

Still not done on the final SHA: the attested `workflow_dispatch` run,
attestation verification, a clean-machine install, and rollback on a
published artifact.

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
