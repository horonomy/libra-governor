# v0.0.5 release-candidate packet (DRAFT, Libra Governor)

Status: **draft, not a release decision.** Nothing is tagged, published or
deployed. `READY_FOR_RELEASE=false`, `RELEASE_APPROVED=false`,
`RELEASED=false`. No v0.0.5 tag, GitHub Release or production activation
occurs without a **separate, SHA-specific founder GO**; that GO names the
exact commit, and any later merge invalidates it.

Baseline: the immutable `v0.0.4` tag (`313d26a`, Cooperative Preview). Tracking
epic: HORO-2325. Candidate code SHA: **`f9746b7`** is the `main` after the last
code PR (#291) merged (2026-10-31); it is only a candidate until the
founder names a SHA, and any later merge, including this docs-only packet,
changes `main`. Documentation-only merges (this packet, its notes draft, README wording) change no source file, so the source-level evidence below stays valid for them; the founder-named SHA is the then-current `main`, and CI/SonarCloud are re-read on it.

Facts below come from the release-evidence directory
(`PHASE0-RECONCILIATION.md`, `CAMPAIGN-CHECKPOINT.md`,
`PENDING-VALIDATION-QUEUE.md`, `ACCEPTANCE-MATRIX.md`) and Jira; anything not
yet known is marked as open. No result is claimed that was not executed.

## Scope

v0.0.5 is a planned increment on v0.0.4: reliable Codex integration, truthful
shared-budget accounting, and safety/recovery hardening, with accepted Claude
Code behavior preserved. It remains a **Cooperative Preview**; the
admission-bounded (not hard-cap) model of
[ADR-0026](adr/0026-local-monthly-shared-budget.md) is unchanged.

In scope (approved by the 2026-10-29 founder directive and the 2026-10-31
"Option 1" confirmation, recorded in HORO-2325). Merge state as of 2026-10-31,
code `main` = `f9746b7`. Every PR below had an exact-head independent AI review
(reviewer id `claude-independent-agent`, run id and findings in the PR's
`OWNER-MERGE-REVIEW` comment), required hosted checks and SonarCloud green, and
a runner-gated merge commit verified in a separate call.

| Area | Ticket | State |
|---|---|---|
| Codex source verification (C0) | HORO-2280 | **Merged** (#264, `fe14978`). Docs only: source-derived at Codex 0.160/0.163/0.164/0.165, nothing executed, no real Codex host run |
| Provider lanes, protocol v24 to v25 (C1) | HORO-2281 | **Merged** (#265 `b851381`; ADR follow-up #267 `a1ba476`). Lane isolation only |
| Turn-scoped Codex settle (C2) | HORO-2282 | **Merged** (prerequisites #269 `86bb341`, characterization #270 `96607de`, `settle_codex_turn` #277 `c0f46ad`). Dark, marker-gated |
| Codex admit/settle hook wiring (C3) | HORO-2283 | **Merged** (#279 `af2ee20`). Dark; `PROTOCOL_VERSION` stays 25 |
| Orphan recovery and true-up (C4) | HORO-2284 | **Merged** (#278 `5a3ba7b`). Dark; never refunds |
| Status/doctor Codex reporting (C5) | HORO-2285 | **Merged** (#280 `ed4a0a3`). Reporting only |
| Hardening from the v0.0.4 audit | HORO-2303 | **Merged**: state-dir symlink chmod refusal (#254 `9e5036c`), Claude reinstall from another path repoints our hooks (#272 `2ca0526`), hand-quoted hook recognised as ours (#274 `060970f`), FIFO transcript pin (#273 `99e394f`), Stop-without-agent pin (#275 `a2a7f13`), statusline id-absence pin (#276 `d509f71`). Found by QA preflight and fixed after the first candidate: Codex reinstall no longer adds a second hook group, and the shared hook-ownership predicate no longer treats wrapped user commands (`env X=1 ...`, `cd && ...`, pipelines, `nohup`, `sudo`) as ours (#288 `819b558`, three independent review rounds). Remaining items: see Known limitations |
| Codex negative rollout counters | HORO-2325 | **Merged** (#291 `f9746b7`): found by independent dynamic QA (defect D-1): a rollout with negative token counters booked 0 cents; it now books the flat hold like every other unmeasurable turn. Codex turn-scoped reader only; Claude path, protocol and goldens untouched |
| Export capacity label | HORO-2293 | **Merged** (#271 `04e41fa`). Labels the field; does not fix the stale value |
| Test pins for the C4/C5 review follow-ups | HORO-2340 | **Merged** (#281 `ac5538b`): 16-hold orphan cap, newest-only true-up, C5 label tests. The other HORO-2340 review follow-ups stay open (Known limitations) |
| SonarCloud findings behind the main gate | HORO-2325 | **Merged**: shell guards (#283 `df5f9c3`), linear-time site link scanners (#282 `ee866e5`), required-nullable pins with justified suppressions (#284 `2441ef1`). No wire or deserialization behavior changed |
| Stop-summary golden flake | HORO-2343 | **Merged** (#287 `5705734`) |
| README, version, label | HORO-2325 | **Merged**: README describes the Codex opt-in (#285 `7c57000`); workspace version 0.0.5 so `scripts/check-release-tag.sh v0.0.5` can pass (#286 `62ef7a1`); accounting-boundary label no longer names a release (#289 `bd510d8`) |
| Pool/account identity integrity | HORO-2191 | Not started; not in this release |
| Gateway truncated-stream under-count | HORO-2286 | Not reproduced, not started; not in this release |

C2 to C5 acceptance criteria are mapped to named tests in the evidence file
`ACCEPTANCE-C2-C5.md`; all passed in a full workspace run at tree `82cd9a1`
(2957/2957). The final-candidate run is in the Acceptance state table below.

## Dark by default (off; not enabled by this release)

- **Codex budget enforcement (C2 to C5, built, off by default).** Codex
  admission, settlement, recovery and true-up run only when the shared budget
  file **and** the marker `<state_dir>/shared-budget.codex` both exist (a
  symlink does not count; the marker is checked on every call). Without the
  marker the **shared budget** is untouched: the Codex hooks send no
  shared-budget admission or settlement, the daemon answers shared-budget
  Codex requests `not_configured`, and the shared-budget tables do not change.
  The pre-existing v0.0.4 task-preflight and receipt behaviour is **not**
  changed by v0.0.5 and still runs without the marker: the prompt hook prints
  the preflight `additionalContext`, `Stop` writes an Estimate-vs-Actual
  receipt to stderr, and the non-budget task ledger tables (tasks, plans,
  contracts, contract_criteria, reservations, resource_accounts,
  session_preflights, session_tasks, task_budgets, receipts) gain rows. This
  was observed identical on v0.0.4 and v0.0.5 in independent QA (QA-1 SF-H,
  QA-3 SF-H-diff). The shared-budget claim is evidenced by named tests
  (shared-budget table fingerprint before/after, `not_configured` answers, the
  retained pre-existing Codex hook test, Claude golden-byte tests); see
  `ACCEPTANCE-C2-C5.md`. It is **not** a claim that disabled behavior is
  byte-identical to v0.0.4 beyond the cases where a test compares bytes.
  Enabling it is a separate founder decision. Codex hooks also only run after
  the operator trusts them in Codex's `/hooks`; an untrusted hook leaves an
  enrolled install silently ungated, and `doctor` says so.
- **S11 gateway per-request cap (HORO-2273 to 2279).** Dark engineering only;
  no gateway enforcement is on by default and none is enabled by this release.
- Enabling either is a separate founder decision with its own packet.

## Not claimed

- No strict or hard monthly cap; admission-bounded and cooperative only, with
  in-turn overshoot possible (ADR-0026).
- No Codex budget enforcement, and no claim that Codex subagent cost is
  covered (no attribution evidence).
- No OpenAI gateway, no new network egress, no new credential custody, no
  remote upload.
- No real-provider evidence: all executed evidence is test-owned (synthetic
  inputs or a fake upstream). Provider billing, quota and latency accuracy are
  **not verified**. Codex support is verified **by source reading only**
  (Codex 0.160, 0.163, 0.164, 0.165 tags); **no Codex runtime verification
  exists**. The installed Codex build was matched to its tag by version string
  only.
- No protection against an active same-user adversarial agent (ADR-0026 trust
  boundary).
- Platform support is unchanged from v0.0.4: Apple Silicon (aarch64) macOS only.
  The release workflow builds one target, `aarch64-apple-darwin`; hosted CI also
  builds and tests on ubuntu and macOS runners, which is test coverage and not a
  support statement. A clean-machine install of this build was not repeated for
  v0.0.5.
- Turns that Codex interrupts or aborts write no usage record; that spend is an
  undetectable under-count and is not covered (pinned by tests as a known
  limit).
- Codex subagents are neither admitted nor settled (known gap).
- Codex has no mid-turn hold renewal here: a turn longer than the 1 h hold
  expires and is settled late by orphan recovery on partial measurement.
- With no `gpt-*` pricing override configured, every Codex turn books the flat
  hold, unmeasured, and is never trued up.
- A long-lived daemon built before C3 rejects `SettleTurn`; an enrolled install
  is then ungated until the daemon restarts. `doctor` probes for this.

## Known limitations (user-facing)

Carried forward from v0.0.4 unless a merged v0.0.5 change removes them; each
ticket-backed limit below stays open until its fix is merged and verified.

| Limitation | Tracked in |
|---|---|
| `shared-budget init --force` replaces the budget file with no backup | HORO-2338 (follow-up, hard-link fallback), HORO-2303 item 9 |
| `shared-budget export` can show the pool row capacity after the cap was raised (now labelled as the pool-row value, not fixed) | HORO-2293 |
| Review follow-ups (uncounted true-up read error, repeated hold listing, missing 16-hold-cap test, doc wording) | HORO-2340 |
| A non-required harness test (`t05b`) flaked once on a sqlite lock race; `adapter_probe` PID-marker race | HORO-2340, HORO-2341 |
| Repeat real-host flush-lag measurement not done | HORO-2246 |
| Gateway path may under-count truncated streams (dark gateway only) | HORO-2286 |
| Pool and account identity not bound to captured identity | HORO-2191 |
| Pricing-table errors (1-hour cache tier, fast mode, geo multiplier under-counted; Haiku 5.5 over-counted) | ADR-0026 |
| The `amount_too_large` ceiling (one charge above ten times the monthly cap) applies to explicit per-charge requests and extra charges (subagent transcript, session-tail, late Codex true-up, a Codex turn booked without a hold), **not** to the end-of-turn settlement of a turn's hold from measured transcript or rollout usage, which books the measured amount without a ceiling. A corrupt or hostile transcript can therefore inflate the ledger (1e15 input tokens booked about $3e9 on Claude `Stop`; Codex saw the same) and make later prompts refuse for the rest of the month. Same on v0.0.4 and v0.0.5; within the documented same-user trust boundary. Disclosed, not fixed | tracked in the QA follow-up ticket HORO-2350 (wording corrections from the same QA: HORO-2349) |
| A Claude `SessionEnd` with no preceding `Stop` for that session does not release or settle the hold. It stays `active` and reduces the remaining budget until it expires (`HOLD_TTL_SECS` = 3600 s after the last renewal); on the next admission it is marked `expired`, its capacity returns and nothing is booked for the hold itself, unless a later `Stop` on the same lane settles it. (`SessionEnd` still runs its tail settlement, which books nothing for a session that never had a `Stop`.) Same on v0.0.4 and v0.0.5 | tracked in the QA follow-up ticket HORO-2350 |

## Acceptance state

Candidate code SHA: `f9746b71615bd3be3fdf441c1755cb4ff62548e9` (`main` after the last
code PR, #291). Per-ticket criteria are in `ACCEPTANCE-C2-C5.md`. This
packet is itself an open draft PR (#266); merging it changes `main`, so the
founder-named SHA must be chosen after that decision.

| Gate | State at `f9746b7` (docs-only `6bf668f` re-read for CI) |
|---|---|
| Candidate integration check (clean detached worktree, isolated `CARGO_TARGET_DIR`, deleted afterwards) | **PASS**: `cargo fmt --all -- --check` rc 0; `cargo clippy --workspace --all-targets -j 4 -- -D warnings` rc 0; `cargo nextest run --workspace -j 4 --no-fail-fast` 2986 run, 2986 passed (2 leaky), 40 skipped, rc 0, plus a release-profile build of the binary used for QA (sha256 `5d6f7d6d283bae9f6465f4512f338d3a350c7f72b8d9fb2da30a28b892512c9c`, `libra-governor 0.0.5`; a local QA build, not the release artifact). Log: `CANDIDATE-GATE-f9746b7.log`. The 40 skipped are exactly the 40 `#[ignore]` tests (28 container-only two-principal harness tests, 8 isolated-process helpers, 4 fixture generators); the 2 leaky tests deliberately spawn decoy processes. Neither is release-relevant. The earlier `fe14978` run (2957 passed) is superseded |
| Hosted CI required checks (`cargo fmt`, `cargo clippy`, `cargo build & test` ubuntu and macos, `Analyze` actions/python/rust) | **success** on `f9746b7` and on docs-only `6bf668f` (push): `CI`, `CodeQL` and `Positioning guard` completed success |
| Two-principal harness (disposable container; the 28 container-only tests the local run skips) | **success** on `819b558` via `workflow_dispatch` (run 41210232729); re-dispatched on `6bf668f` (run 41219030766): **success** (the container job, both trusted-principal feature builds); the job does not run on push to `main`, so it was dispatched manually |
| SonarCloud quality gate on `main` | Was **FAIL** at `fe14978` (`new_security_rating` C, `new_reliability_rating` D). Cause established from the SonarCloud analysis history: the analysis at `b851381` (#265) was the first to analyze Rust at all (per-language lines of code: no Rust before, 149,120 after; quality profile "Sonar way comprehensive (rust)" first applied), so 11 older new-code issues surfaced at once; no repository or workflow change caused it. Findings were classified and fixed, not waived: shell S7688 x3 and python S8786 x2 were real and are fixed (#283, #282); rust S9334 x5 and S2612 are false positives with evidence (required-but-nullable fields fixed by `host-capability-snapshot.schema.json`, a mandated 0644 mode per ADR-0019) and are pinned by tests with justified `NOSONAR` (#284). The new-code definition was not changed and no issue was accepted. Gate on `main` read **OK** at `bd510d8` with all five new-code conditions OK; re-read after the `f9746b7` and `6bf668f` analyses: **OK**, all five new-code conditions OK. SonarCloud still lists 128 open issues on `main` in total; the gate conditions are OK with them open. Among them are 11 pre-existing new-code findings (GitHub Actions rules in `phase0-ci.yml`/`ci.yml`, two float comparisons in `experiments/phase0/src/phase0/runner.py`, created 2026-09-17), ticketed as HORO-2344, and older maintainability findings (for example cognitive-complexity and redundant-closure rules), some on code added by v0.0.5 PRs; those are not triaged individually and are not a v0.0.5 release criterion |
| Independent code review | Every merged v0.0.5 PR has an exact-head `OWNER-MERGE-REVIEW` record; no Critical/High open. Reviews were static AI reviews (`claude-independent-agent`), not human reviews |
| Independent product QA, provider-free (dynamic, executors who did not write the candidate, tests or preflight) | **DONE against the pinned binaries, with three executors**: QA-1 (operator journey, docs-versus-behaviour, privacy sweep, migration over v0.0.4-written hooks, hand-quoted and wrapped commands): 36 PASS, 2 FAIL (both documentation wording, since corrected), 4 NOT RUN, no code defect; QA-2 (adversarial hooks and rollouts, lane isolation, multi-session restart, 238-cent arithmetic): found D-1, fixed in #291 and re-verified (10 negative-counter cases book the flat hold), no regressions; QA-3 (Claude differential against the published v0.0.4 binary): 13 rows, zero unexpected differences. Results: `qa/results/QA1-QA3`, matrix `qa/ACCEPTANCE-TO-EVIDENCE-MATRIX.md`, pins in the matrix. Developer tests and static reviews are separate columns there and never stand in for these runs |
| Independent product QA, real host (Codex and Claude Code) | **NOT RUN.** No real Codex or Claude Code host process has been run and no real provider usage observed; the claims that depend on one are listed as not verified in the notes. Needs the owner's login to a dedicated QA home |
| Protocol v25 compatibility statement | Daemon and CLI must come from the same release; a v24 daemon rejects a request carrying `provider`, and a pre-C3 v25 daemon rejects `SettleTurn` (an enrolled install is then ungated until the daemon restarts; `doctor` probes this) |
| Jira Fix Version and Goal for v0.0.5 | **NOT CREATED.** No create-version tool exists in the connected Atlassian tools, and the REST attempt authenticated but was refused (404: the account cannot create versions in HORO); the exact contents are in `JIRA-METADATA-HANDOFF.md` for the authorized Jira administrator |
| Open dependency PRs (#187 to #195, #260 to #262) | None required for v0.0.5 (`DEPENDENCY-DISPOSITION.md`); #192 to #195 change the release workflow actions and are deliberately not in this release |
| Founder SHA-specific GO | NOT GIVEN |

### Open decisions

1. **Jira Fix Version and Goal (R8).** Administrator step, contents handed over in
   `JIRA-METADATA-HANDOFF.md`; no authorized tool path exists in this environment.
2. **Real-host independent product QA (a baseline requirement, not an R-criterion).** The provider-free groups are done (above). The
   real-host group needs the owner to log in to dedicated QA homes and trust the hooks; one consolidated owner action is prepared in
   `qa/realhost/`. Until it runs, the not-verified list stays in the notes.
3. **Two pre-existing behaviours (HORO-2350), disclosed, not changed:** settlement of measured usage has no per-charge ceiling, and a Claude
   `SessionEnd` without a `Stop` leaves its hold until it expires. Whether to change either is an accounting-semantics decision for the founder.
4. **Founder SHA-specific GO.** Not given; tagging, publication and deployment stay gated.

## Release notes draft

The draft is the file [`release-candidate-v0.0.5-notes.md`](release-candidate-v0.0.5-notes.md),
kept next to this packet and **not** at `docs/release-notes/v0.0.5.md`: the
release workflow publishes `docs/release-notes/<tag>.md` as the first Release body
and fails closed, so a placeholder file at that path must not exist until the
founder-named SHA is frozen. There is no CHANGELOG file in this repository.

What the release workflow requires of that file (`.github/workflows/release.yml`,
job `create-release`, step "require the approved release-notes body"): it must exist,
be non-empty and under 100000 bytes, and contain each of nine literal, case-sensitive
markers (`grep -qF`): `not a hard spending cap`, `Admission-bounded (cooperative)`,
`In-turn overshoot`, `Codex is not gated`, `never your provider bill`,
`Accounting boundary`, `Fail-open behaviour`, `over-accounted by approximately 5x`,
`NOT a security boundary`. The draft is built from the approved v0.0.4 wording; the
only disclosure rewritten is the Codex one (now "Codex is not gated by default", with the
opt-in explained), and disclosure 5 now names opted-in Codex turns. All nine markers were
checked present in the draft with the same `grep -qF` test the workflow uses.

Fields that cannot exist before freeze, and are the only open ones: the frozen SHA and
the artifact checksum (both recorded in the Release at publication; the checksum is
produced by the release workflow when the tag is built, and bit-for-bit reproducibility
is not claimed), and the independent product QA result line (`QA_RESULT_AT_FREEZE`).

Freeze procedure (after the founder names a SHA): `git mv`/copy the draft to
`docs/release-notes/v0.0.5.md`, replace `QA_RESULT_AT_FREEZE` with the recorded QA
verdict, re-run the nine-marker check, and confirm `scripts/check-release-tag.sh v0.0.5`
passes against the workspace version. Do not tag until the founder GO.

## Rollback

Unchanged in mechanism from v0.0.4: the budget is opt-in and file-driven.
- To turn Codex accounting off, delete the marker `<state-dir>/shared-budget.codex`;
  the daemon and the hooks re-check it on every call, so settlement and admission
  return to inert immediately (covered by named tests).
- To turn the whole budget off, delete `shared-budget.json` (the daemon re-reads
  it on every decision). `libra-governor uninstall` removes Libra's hooks.
- A mixed install of v0.0.4 and v0.0.5 components is not supported: reinstall
  both binaries from the same release. A v24 daemon rejects any request carrying
  `provider`; a v25 daemon built before the Codex hooks rejects `SettleTurn`.
- Rollback of a published artifact is not evidenced.
