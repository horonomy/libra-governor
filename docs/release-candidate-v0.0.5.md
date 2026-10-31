# v0.0.5 release-candidate packet (DRAFT, Libra Governor)

Status: **draft, not a release decision.** Nothing is tagged, published or
deployed. `READY_FOR_RELEASE=false`, `RELEASE_APPROVED=false`,
`RELEASED=false`. No v0.0.5 tag, GitHub Release or production activation
occurs without a **separate, SHA-specific founder GO**; that GO names the
exact commit, and any later merge invalidates it.

Baseline: the immutable `v0.0.4` tag (`313d26a`, Cooperative Preview). Tracking
epic: HORO-2325. Candidate SHA: **`fe14978`** is the `main` after the last
approved v0.0.5 PR merged (2026-10-31); it is only a candidate until the
founder names a SHA, and any later merge changes it. Facts below come from the release-evidence directory
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
`main` = `fe14978`. Every PR below had an exact-head independent AI review
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
| Hardening from the v0.0.4 audit | HORO-2303 | **Merged**: state-dir symlink chmod refusal (#254 `9e5036c`), reinstall from another path repoints our hooks (#272 `2ca0526`), hand-quoted hook recognised as ours (#274 `060970f`), FIFO transcript pin (#273 `99e394f`), Stop-without-agent pin (#275 `a2a7f13`), statusline id-absence pin (#276 `d509f71`). Remaining items: see Known limitations |
| Export capacity label | HORO-2293 | **Merged** (#271 `04e41fa`). Labels the field; does not fix the stale value |
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
  marker the Codex hooks send nothing, the daemon answers every Codex request
  `not_configured` and the ledger is unchanged. This is evidenced by named
  tests (whole-ledger fingerprint before/after, `not_configured` answers, the
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

## Acceptance state

Candidate SHA: `fe14978e25da6e660c8069460751de949ace4012` (`main` after the last
approved v0.0.5 PR). Per-ticket criteria are in `ACCEPTANCE-C2-C5.md`. This
packet is itself an open draft PR (#266); merging it changes `main`, so the
founder-named SHA must be chosen after that decision.

| Gate | State at `fe14978` |
|---|---|
| Candidate integration check (clean detached worktree, isolated `CARGO_TARGET_DIR`, deleted afterwards) | **PASS**: `cargo fmt --all -- --check` rc 0; `cargo clippy --workspace --all-targets -- -D warnings` rc 0; `cargo nextest run --workspace` 2957 run, 2957 passed (2 leaky), 40 skipped, rc 0. Log: `CANDIDATE-GATE-fe14978.log`. An earlier run of the same check with an overlapping writer is not counted as evidence |
| Hosted CI required checks (`cargo fmt`, `cargo clippy`, `cargo build & test` ubuntu and macos, `Analyze` actions/python/rust) | **success** on `fe14978`; `CI` and `CodeQL` workflows completed success |
| SonarCloud quality gate on `main` | **FAIL (open)**: `new_security_rating` 3 (C) and `new_reliability_rating` 4 (D), both required A. The 11 issues behind it were all created 2026-10-04 to 2026-10-11, in files none of the v0.0.5 PRs touched (`two-principal-harness/src/lib.rs` S2612; `harness/two-principal/provision.sh` S7688 x3; `host_runtime/config_record.rs` and `protocol/host_event.rs` S9334 x5; `tools/build_site.py` S8786 x2). The same gate was `success` at `69a6a27` and failed first at the merge of #265 (`b851381`); the cause of that flip is not established (hypothesis: a SonarCloud-side new-code baseline or issue-state change on `main`, not a source change). Every v0.0.5 PR head passed its own SonarCloud check. Needs a SonarCloud administrator decision; see Open decisions |
| Independent code review | Every merged v0.0.5 PR has an exact-head `OWNER-MERGE-REVIEW` record; no Critical/High open. Reviews were static AI reviews (`claude-independent-agent`), not human reviews |
| Independent product QA | **NOT DONE.** No Codex QA request has been made; no real-host result exists |
| Protocol v25 compatibility statement | Daemon and CLI must come from the same release; a v24 daemon rejects a request carrying `provider`, and a pre-C3 v25 daemon rejects `SettleTurn` (an enrolled install is then ungated until the daemon restarts; `doctor` probes this) |
| Jira Fix Version and Goal for v0.0.5 | **NOT CREATED.** No create-version tool exists in the connected Atlassian tools and the REST attempt was not authorized; the exact contents are in `JIRA-METADATA-HANDOFF.md` for the authorized Jira administrator |
| Open dependency PRs (#187 to #195, #260 to #262) | None required for v0.0.5 (`DEPENDENCY-DISPOSITION.md`); #192 to #195 change the release workflow actions and are deliberately not in this release |
| Founder SHA-specific GO | NOT GIVEN |

### Open decisions

1. **SonarCloud gate on `main` (R2).** Options for the SonarCloud administrator
   or founder: accept the 11 old issues with a written justification, fix them in
   a separate PR (the `serde(default)` ones change deserialization behavior and
   touch the protocol crate, so they are not safe to rush into a release
   candidate), or change the project's new-code definition. Until one is chosen
   R2 is not met and `READY_FOR_RELEASE` stays false.
2. **Jira Fix Version and Goal (R8).** Administrator step, contents handed over.

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
