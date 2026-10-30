# v0.0.5 release-candidate packet (DRAFT, Libra Governor)

Status: **draft, not a release decision.** Nothing is tagged, published or
deployed. `READY_FOR_RELEASE=false`, `RELEASE_APPROVED=false`,
`RELEASED=false`. No v0.0.5 tag, GitHub Release or production activation
occurs without a **separate, SHA-specific founder GO**; that GO names the
exact commit, and any later merge invalidates it.

Baseline: the immutable `v0.0.4` tag (`313d26a`, Cooperative Preview). Tracking
epic: HORO-2325. Candidate SHA: **TBD** (whatever `main` is when the founder
names it). Facts below come from the release-evidence directory
(`PHASE0-RECONCILIATION.md`, `CAMPAIGN-CHECKPOINT.md`,
`PENDING-VALIDATION-QUEUE.md`, `ACCEPTANCE-MATRIX.md`) and Jira; anything not
yet known is marked **TBD**. No result is claimed that was not executed.

## Scope

v0.0.5 is a planned increment on v0.0.4: reliable Codex integration, truthful
shared-budget accounting, and safety/recovery hardening, with accepted Claude
Code behavior preserved. It remains a **Cooperative Preview**; the
admission-bounded (not hard-cap) model of
[ADR-0026](adr/0026-local-monthly-shared-budget.md) is unchanged.

In scope (approved by the 2026-10-29 founder directive, recorded in HORO-2325).
Merge state of every row is **TBD** at the time of writing; the table records
the branch state observed on 2026-10-30.

| Area | Ticket | Observed state (2026-10-30) |
|---|---|---|
| Codex source verification (C0) | HORO-2280 | Docs-only draft PR #264; source-derived at Codex 0.160/0.163/0.164/0.165, nothing run |
| Provider lanes, protocol v24 to v25 (C1) | HORO-2281 | Branch only, no PR; `cargo check`, protocol and daemon shared-budget tests reported green at `49e13f0`; full-suite gate not complete |
| Turn-scoped Codex settle (C2) | HORO-2282 | Six characterization tests written, not run; settle wiring not built |
| Codex admit/settle wiring (C3), recovery (C4), status/doctor (C5) | HORO-2283, 2284, 2285 | Not started |
| Hardening from the v0.0.4 audit | HORO-2303 | Draft PR #254 (state-dir symlink); four further one-item branches, not run |
| Export capacity label | HORO-2293 | Branch, test not run; labels the field, does not fix the stale value |
| Pool/account identity integrity | HORO-2191 | Not started |
| Gateway truncated-stream under-count | HORO-2286 | Not reproduced, not started |

Which of these actually land is **TBD**; the final list is whatever is merged
at the founder-named SHA.

## Dark by default (off; not enabled by this release)

- **Codex budget enforcement (C1 to C5).** Any Codex admission or settlement is
  gated by a marker file that is absent by default. Without it, Codex sessions
  are neither admitted nor charged, exactly as in v0.0.4.
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
- Platform support is unchanged from v0.0.4 (Apple Silicon macOS only) and is
  **TBD** until re-verified for this build.
- Turns that Codex interrupts or aborts write no usage record; that spend is an
  undetectable under-count and is not covered.

## Known limitations (user-facing, TBD against final scope)

Carried forward from v0.0.4 unless a merged v0.0.5 change removes them; each
ticket-backed limit below stays open until its fix is merged and verified.

| Limitation | Tracked in |
|---|---|
| State-directory symlink can redirect chmod/ledger in a non-default writable location | HORO-2303 item 2 (PR #254) |
| Hand-quoted hook commands can be missed by `uninstall`; reinstall from another path can leave a second hook set | HORO-2303 items 1 and 4 |
| `shared-budget init --force` replaces the budget file with no backup | HORO-2303 item 9 |
| Transcript FIFO/hung-mount reading has no executable test | HORO-2303 item 3 |
| `shared-budget export` can show the pool row capacity after the cap was raised | HORO-2293 |
| Repeat real-host flush-lag measurement not done | HORO-2246 |
| Gateway path may under-count truncated streams (dark gateway only) | HORO-2286 |
| Pool and account identity not bound to captured identity | HORO-2191 |
| Pricing-table errors (1-hour cache tier, fast mode, geo multiplier under-counted; Haiku 5.5 over-counted) | ADR-0026 |

## Acceptance state

Per-ticket state is in the evidence matrix (`ACCEPTANCE-MATRIX.md`). Summary at
time of writing: no v0.0.5 item has independent QA, no hosted CI result at a
candidate SHA, and no real-host result. Gate fields are **TBD**:

| Gate | State |
|---|---|
| Candidate SHA | TBD |
| Full workspace tests at candidate SHA | TBD |
| fmt, clippy `-D warnings`, pre-commit at candidate SHA | TBD |
| Hosted CI (all required checks, including SonarCloud) | TBD |
| Independent code review | TBD |
| Independent product QA | TBD |
| Protocol v25 compatibility statement (v24 client and daemon skew) | TBD |
| Jira Fix Version and Goal for v0.0.5 | NOT CREATED (see matrix) |
| Open dependency PRs (#187 to #195, #260 to #262) | TBD, not reviewed; #192 to #195 change the release workflow actions |
| Founder SHA-specific GO | NOT GIVEN |
