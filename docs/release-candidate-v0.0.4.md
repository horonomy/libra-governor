# v0.0.4 release-candidate packet (Cooperative Preview)

> **Historical planning document.** v0.0.4 was published from `313d26a09407c8a77d2b4cc7685ea694a822905a` (tag `v0.0.4`, release run 40843550017). The status lines and tables below record the state when this document was written, before the founder GO (HORO-2186 comment 29520) and the tag push; they are not the current release status. Post-publication verification by the release owner is recorded in HORO-2186; no independent post-publication QA is claimed here.

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
| HORO-2248 first run, denial, statusline | ACCEPTED (scoped functional) | Codex QA HORO-2186 c29269 PASS on `a3e9ec9`; the >=70 experience gate is met (73 c29293 on `c38750d`, 71 c29341, 73 c29344 after the fix) |
| HORO-2246 bounded Stop flush wait | ACCEPTED (bounded orchestration only) | c29269: delayed Stop measured once, missing transcript waits ~1.5 s then flat hold once. Not a fresh real-host sample; interactive TTY unmeasured |
| HORO-2250 M3 prune then replay | ACCEPTED (reused evidence) | c29269 PASS_REUSED from earlier coordinator logs, not newly run; main-turn Stop replay after prune is covered by a test in PR #239 that first ran green on hosted CI but could not tell the guard from its absence (independent review), so it was strengthened; the strengthened form ran green on macOS and Ubuntu and is merged (PR #239, `b9cf2b8`) |
| In-product disclosure | ACCEPTED | c29269: `status` and `doctor` show all four limitations |
| Whole-feature Cooperative Preview | ACCEPTED (scoped functional PASS) | c29269 "scoped functional PASS; overall PARTIAL / no release sign-off"; no FAIL. c29269 verifies warn-only soft limits. DST gap and fold at a month's first midnight are covered by domain unit tests (`calendar.rs`, America/Asuncion and America/Havana), not by an end-to-end Codex run. Not covered: multi-daemon writers; power-loss/mid-COMMIT |
| Independent Product Experience score | ACCEPTED (bounded scope) | **75/100**, Codex c29404 (A1 review; B +2 for the corroborated interactive host evidence only); history: 73 (c29293, `c38750d`), 71 (c29341, open defect QA242-DOC-01), 73 (c29344, defect closed), 75 (c29404); Preview threshold 70 met; overall verdict stays PARTIAL; see the Product Experience gate |
| Jira Fix Version / Goal (HORO-1752) | DONE (2026-10-28) | Fix Version 10315 on 36 issues and Goal CBLPCRLM-71 linked to Epic HORO-1749, created through the Jira REST and Goals APIs (HORO-1752 comment 29444); the earlier "admin must create it" premise was wrong |
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

**Founder confirmation of this classification:** conditionally confirmed on
2026-10-29 (D5, recorded in HORO-2186) for the narrow Cooperative Preview only.
It is not a blanket security waiver; no row verdict was changed; unresolved
items stay traceable to Jira (HORO-2303 and HORO-2250). Rows 5 and 11 were
reassessed against source on 2026-10-29; see the note after the table.

Updates since this table was written (2026-10-29), each a clarification, no
verdict changes: row 1: A1 booked `/compact` as an unmeasured subagent at the
flat hold against a fake upstream; real-provider behaviour is still unverified.
Row 4: `doctor` now reports unusable legacy hook commands (PR #250, merged);
quote-aware ownership stays HORO-2303. Row 11: the fix is draft PR #254
(HORO-2303), held for v0.0.5. Row 12: A1 measured four flush waits of
76/107/118/124 ms against a fake upstream (Codex-accepted, scoped); real
provider timing is unmeasured.

| # | Item | Verdict | Evidence and impact |
|---|---|---|---|
| 1 | Post-/compact usage unmeasured | accepted limitation, disclosure added (PR #244, merged `727b541`) | No Claude compaction handling exists. Outcomes are unobserved: summarizer lines may be sidechain-flagged and silently excluded (an under-count), or book the flat hold. "Fails safe" was wrong. No test. Real-host check needs authorization. |
| 2 | HORO-2179 test gaps | approved future work | A 0015 migration test already exists (`migrations.rs`, `migration_15_...`); a statusline session/agent-id absence test does not. Privacy regression guard only. |
| 3 | Codex route has no gate | accepted limitation | Codex is never charged, blocked or admitted: Prompt/PreToolUse skip non-Claude agents and a Codex Stop settles nothing. Isolation rests on session ids not colliding, not a provider check. The disclosure in `status`, `doctor` and README is accurate. Optional hardening: skip `settle_turn` for non-Claude providers. |
| 4 | Quoted/renamed/Windows hook commands | approved future work | A hand-quoted or renamed command is missed by uninstall and can be duplicated by install. **Sub-case found: a binary path containing whitespace or a shell metacharacter wrote an unquoted hook that silently never ran (every prompt ungated) while doctor saw the hooks as wired. Fixed: PR #242 (merged `8da5e61`) refuses such a path at install.** |
| 5 | Any absolute subagent transcript path accepted | approved future work | Same-user only (socket 0600 in a 0700 dir). Content leak is limited to a short model name in status. A FIFO or stalled-mount path can block the single-threaded daemon (same-user DoS). Optional: require the path under the transcript's `subagents/` dir. |
| 6 | Reinstall from another path adds a second hook set | approved future work | No double charge or hold found in source (Admit is one hold per lane; Stop replay is keyed by prompt id). Legacy tool-call counts double. An older hook set without the flush wait could win the race and book the flat hold (inferred, untested). |
| 7 | Derived-lane counter rows never reclaimed | approved future work | One small row per session and agent; no money path reads it. |
| 8 | Prompt-agent / Stop-no-agent asymmetry | accepted limitation | Legacy token lane only: a leaked reservation until its 900 s TTL and one lost estimator sample; no USD effect. The shared budget always admits with no agent id. The asymmetric direction is pinned by `a_stop_without_an_agent_does_not_close_an_agent_lanes_turn` (HORO-2303). |
| 9 | Legacy watermark rows (migration 0022) | accepted limitation | 0022 was never in a released build (no v0.0.3 tag; v0.0.2 is the latest release; `release/v0.0.3` stops at 0015). Development ledgers should be recreated. |
| 10 | `init --force` replaces the file with no backup | accepted limitation, disclosure added (PR #244) | Ledger and pool state are untouched and the swap is atomic. A forced re-init silently drops hand-written `pricing_overrides`, `subagent_unmeasured_charge_usd_cents` and soft limits not passed again, and resets the turn hold to the default if omitted. Help says only "replaces the whole file". Smallest fix: keep a `.bak` hard link and say what is dropped. |
| 11 | `ensure_state_dir` chmod follows a symlink | approved future work | Same-user only on the default path (`~/.local/state`); a non-default `LIBRA_GOVERNOR_STATE_DIR` into a shared location could make the user chmod a symlink target they own to 0700. No symlink test. Fix: `symlink_metadata` plus an owner check. |
| 12 | Interactive TTY flush timing unmeasured | accepted limitation, disclosure added (PR #244) | Not "fails safe": a partly flushed turn settles short and the remainder is recovered only by a later Stop or SettleTail, and is lost if neither runs. Measured only with `-p`. A real TTY sample needs authorization. |
| 13 | No `clock_anomaly` repair command | accepted limitation, message corrected (PR #244) | Fails closed with no money effect, but "correct the clock" is not actionable once the clock is right; the real remedies (ADR-0026 recovery, or removing `shared-budget.json` to pause the budget) are not named. A forward clock jump also opens next month's pool with a full cap and is undisclosed. |
| 14 | **New: re-prompt renews a live hold with no capacity check** | **fixed: PR #240 merged (`98b112a`)** | After an interrupt (Stop does not fire) the hold stays live; a new prompt renewed it without the check a fresh reservation applies, so turns could continue after other sessions overdrew the pool. Failing-first tests ran red on hosted CI (101 passed, 1 failed); the fix applies the fresh-reservation test to a renewal that names a new prompt id (a prompt-less retry is unchanged, per ADR-0026 "a lower cap never revokes existing holds"); ADR-0026 documents it; two independent review passes; hosted CI green on the PR head. The trigger relies on interrupt behaviour not observed on a real host. |

**Reassessment of rows 5 and 11 (2026-10-29, founder condition; source review
at FINAL, no new dynamic test; the verdicts above are unchanged).**
Row 5, subagent transcript path: the table text names a FIFO. At FINAL
`reject_non_regular_file` (`crates/daemon/src/usage.rs`, introduced in
`c3fc094`, 2026-10-12) runs a `stat` that never opens the path, and
`scan_lines_backward` calls it before any `open`. By source inspection only,
that refuses a FIFO, device or directory path as unreadable; this has not been
verified by any executable test or QA of the subagent path, so the FIFO part of
the row text is not proven superseded (test gap, to HORO-2303). Residual: a `stat` on a hung network or FUSE mount
can block the single-threaded daemon, and a same-user swap between the `stat`
and the `open` is theoretically possible. A stalled daemon makes prompt
admission refuse and the other hooks fail open (release-notes disclosure 6);
the read precedes the charge in `charge_transcript_inner`. A same-user
process, including the governed agent, can already stop the daemon or edit the
budget file (ADR-0026, "Trust boundary of the file"), so this adds no new
capability, secret exposure or ledger-write path found in source.
Row 11, state-directory symlink: `exists()` and `set_permissions` in
`crates/daemon/src/paths.rs` follow symlinks. On the default path under the
user's own home, only that user (or root) can plant the link, and the effect is
a mode change to 0700 on something that user owns. With a non-default
`LIBRA_GOVERNOR_STATE_DIR` or `XDG_STATE_HOME` in a location other users can
write, a planted link could change the mode of a victim-owned target and could
redirect the ledger, socket and gateway capability token into a
directory the planter controls (inferred from source, untested). That is
outside the default supported path and is disclosed in release-notes
disclosure 7. The fix is draft PR #254 (HORO-2303), held for v0.0.5, and no
symlink test exists at FINAL.
Conclusion (founder D5 closeout, 2026-10-29): both rows are accepted
conditionally for the narrow Preview, within the documented default
private-state and same-user trust boundaries only. No protection is claimed
against an active same-UID adversarial agent, and users are warned not to place
the state directory in a shared or other-user-writable location (release-notes
disclosure 7). This is not a security waiver; HORO-2303 and its follow-up work
stay open.

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

**Independent score: 75/100** (Codex, HORO-2186 comment 29404, qualified
product `4e56fbd`): A 24/30, B 10/20, C 20/25, D 12/15, E 9/10. B rose two
points for the newly corroborated interactive-host Stop, flush and denial
behaviour only; there is no credit for a real provider, real latency, natural
workload or `ProductionObserved` evidence (the fake usage and protocol sleeps
are synthetic). The 73/100 of comment 29344 (B 8) is kept as history. Preview threshold 70 and internal dogfood threshold 60 are met. The
history is kept: 73 (comment 29293, on `c38750d`), then 71 (comment 29341,
D 11 and E 8, after the open defect below), then 73 (comment 29344), then 75 (comment 29404). D and E
were each raised one point for observed evidence (false healthy assurance
removed without mutation or ownership change; the stated repair actually
executed and install-alone verified insufficient), not restored
automatically. B gains nothing without current-host, terminal or provider
evidence. These are evidence-weighted judgments, not user-study statistics.
Verified by me: `QA-RESULTS.json` in `coordinator/20261027-libra-pr250-closeout/`
(finding verdict `PASS_CLOSED_SCOPED`, 60 assertions passed, 0 failed,
artifact hashes equal to handoff 29342) and its `SHA256SUMS.txt` (all entries
verify).

**QA242-DOC-01 (Medium, Codex HORO-2250 c29337): CLOSED, scoped.** A settings
file written before PR #242 keeps unquoted hook commands under a path the
shell splits (exit 127 before Libra runs) while `doctor` reported the hooks
ok. Fixed by PR #250 (merged `4e56fbd086e8234aec52f0185df8a3b5fa62c271`):
`doctor` reports such commands as an error (`shared_budget_hooks` a warning,
`claude_settings` still an error, with no budget configured) and names the
remedy (`uninstall`, move the binary to a plain path, `install`); ownership
matching, install and uninstall are unchanged. Codex c29344 reproduced the
original false ok with the prior binary, then on the CI artifact (run
40516615874) found both findings ERROR with 7 commands counted, the generated
command still exiting 127, install-alone still negative, the remedy working,
and five valid path controls healthy. With it: `FUNCTIONAL_QA_PASS=true` and
`SECURITY_QA_PASS=true`, both scoped to the offline, owned-runtime Cooperative
Preview (no universal security certification); `REAL_HOST_QA_PASS` is
NOT_VERIFIED; `RELEASE_CHECKLIST_PASS=false`;
`FULL_RELEASE_QA_SIGNED_OFF=false`; READY_FOR_RELEASE=false. Item 14 (PR #240)
was independently retested by Codex in c29341 (PASS).

**PR #252 (`694427f`, test-only) post-dates that verdict.** It adds characterization tests for corrupt and unsupported settings shapes. Its only product-file hunk in `crates/cli/src/claude_settings.rs` starts at line 928, after the `#[cfg(test)]` boundary at line 860 of `4e56fbd`, so runtime and release-binary inputs are unchanged. It is producer-tested (hosted CI 15/15) and not independently re-run, which a test-only change does not require.

What keeps B at 8/20 and what would raise it: a current-head real-host run,
an interactive TTY and partial-flush sample, post-/compact coverage, and a
real-provider session, each needing separate authorization. C and the rest
are bounded by a warm private fixture rather than a clean-machine install.

## Security gate (2026-10-27)

Separate from the hardening table. Evidence is local or hosted-CI unless
stated; none of it is independent QA.

| Check | Result |
|---|---|
| `cargo audit` on `98b112a` | **failed**: 7 RustSec advisories in the shipped binary (rustls 0.23.45: RUSTSEC-2026-0346/0347/0348/0349; rustls-webpki 0.103.15: -0382; webpki-roots 1.0.9: -0381; ctutils 0.4.2: -0385) and an `unsound` warning on tokio 1.53.2 (-0386) |
| Exposure | rustls and webpki are reached only through `libra-governor-gateway` (hyper-rustls), the optional gateway that is off by default and not used by the shared budget; ctutils through hmac/sha2. Linked into the release binary either way |
| Fix | PR #248 (merged `ee42bca`): `Cargo.lock` only, five packages to patched releases, `Cargo.toml` unchanged; one independent review, no findings |
| `cargo audit` on `ee42bca` | exit 0, no vulnerabilities or warnings (197 crates, advisory DB of 2026-10-27) |
| Secret scan | `gitleaks detect --redact` over the last 133 commits: no leaks |
| Dependabot alerts | 3 open, all Python `filelock` in `experiments/phase0/uv.lock` (not in the shipped binary). Open Dependabot PRs (#187-#195) are bot PRs that need an owner-approved review; #188 (tokio) is superseded by #248 |
| Binary hash | the lockfile change alters the binary: earlier hashes do not apply to `ee42bca`; local aarch64 release `sha256 aac36394…ba71` (not CI-produced, not attested) |

Product SHA for QA is `4e56fbd086e8234aec52f0185df8a3b5fa62c271` (PR #250;
`ee42bca` added the lockfile fix); later `main` commits are docs-only, except
test-only PR #252 (`694427f`). The lockfile and the doctor change each alter the binary bytes.

## Packet A1: isolated interactive acceptance (2026-10-28)

Founder-approved for A1 only (not A2, B, C, release). Producer evidence,
class `TestOwnedLive`, not `ProductionObserved`. **Independent review (Codex
comment 29404): scoped interactive-host evidence ACCEPTED; Packet A1 overall
PARTIAL** (complete isolation and closeout custody is partly
producer-attested); no new product Bug confirmed;
`REAL_HOST_QA_PASS` remains NOT_VERIFIED for the full contract; real-provider
A2 is not automatically a paid Preview prerequisite.
Evidence (not in the repo; it holds daemon logs and a harness copy):
`~/Bryant-Developments/horonomy/libra-release-evidence/20261028-a1-interactive/`,
index `SHA256SUMS.txt` sha256 `13be07fda41ec2c895db0cb7dae053b84255359c24f140aea5619b8ddde29fec`
(17 files, all verify). The first index (`60aa9413…`, 15 files) is what Codex
reviewed; this one adds the corrected sandbox profile template,
producer-transcribed receipts (labelled as such) and renames the superseded
wildcard `setup.sh`.

**Corrections after the Codex review (c29404), all verified against the logs:**

- 15 UserPromptSubmit log lines are attempts, including the blocked prompt; there
  were 14 allowed ordinary turns plus the blocked one. `/compact` produced no
  admission line of its own. "15 admissions" was wrong.
- Accounting: 14 measured Stops = 315c; three `SettleTail` recoveries (one at
  each `/exit`) = 63c; one unmeasured subagent fallback = 200c; **gross logged
  578c**. The 557c in `status.json` is the snapshot taken before the final
  `/exit` (315 + 42 + 200). No post-exit status snapshot exists, so the final
  reconciliation and the absence of a double charge for the third tail are
  unverified; this is not a proven accounting bug.
- 25 POST requests (plus 2 GET health checks) reached the fake upstream; they are
  not 25 completed billable messages.
- The stale-`$50` statusline is producer-observed; the retained final frame shows
  `$5.57/$50`, and the denial-time frame was not retained.
- The first sandbox profile (wildcard localhost allow) let external traffic
  through and was discarded; the superseded `setup.sh` is kept for custody and
  must not be replayed or used to certify no egress. The corrected exact-port
  profile is `CORRECTED-sandbox.sb.template`. Egress-probe and cleanup receipts
  are transcribed from the producer session (`PRODUCER-TRANSCRIBED-RECEIPTS.txt`,
  producer-attested). Not captured: global-state before/after hashes, per-PID
  exit codes, the cap-edit timeline against frames, argv receipts for the two
  `--continue` launches.

| Item | Value |
|---|---|
| Libra binary | CI-built aarch64, hosted run 40516615874, source `263a7a6`, product `4e56fbd`; binary sha256 `fb2376a5…050d` |
| Host | real `claude` 2.1.306 in a `tmux` pseudo-terminal (private tmux server), macOS arm64 |
| Isolation | temp HOME and state dir, dummy sentinel API key, loopback fake Messages-API upstream, `sandbox-exec` denying all network except the fake upstream port and unix sockets |
| Egress check | `curl` to example.com and 1.1.1.1 and another loopback port failed inside the sandbox; Claude Code's own start-up check of `api.anthropic.com` was refused until onboarding was pre-seeded; the fake upstream saw only the sentinel key (25 POST requests plus 2 GET health checks, no bearer token) |
| Real host | not touched: real `~/.claude/settings.json` (last modified 2026-10-23) and `~/.local/state/libra-governor` (2026-10-25) are older than the run; the Libra entries there predate it (a dev-build install) |
| Teardown | `/exit`, `daemon stop`, private tmux server and fake upstream killed; no leftover process; isolated trees removed |

Prompts: 14 allowed ordinary interactive turns over 3 sessions (2 resumed with
`--continue`), 4 of them executing a `Bash` tool, plus one `/compact` (no
admission line of its own), and 1 prompt blocked by an exhausted cap (the
recovery prompt is among the 14); 15 UserPromptSubmit log lines in total.

| Observation | Result |
|---|---|
| Stop settlements | 14 of 14 measured, 0 flat-hold fallbacks, 0 binding mismatches; 315c across the 14 Stops; status snapshot before the final `/exit` settled 557c with held 0 and consistent remaining; gross logged 578c (see corrections) |
| Stop flush wait | 4 of 14 Stops waited for the transcript: 76, 107, 118, 124 ms (5 to 6 probes); the other 10 were ready at the first probe; none reached the cap. The fake upstream answers instantly and streams in 120 ms steps, so real model latency is still unmeasured |
| Records after the last Stop | 3 `SettleTail` recoveries, one at each `/exit` (21c each, `already_charged=false`); the first two are in the 557c snapshot, the third was charged after it |
| `/compact` | surfaced as a `SubagentStop` and was booked as an **unmeasured subagent charge at the 200c flat fallback** (the fake spend was about 21c); `doctor` showed the flagged-settlements warning. This is an over-count here; with a large real context a compaction could cost more than the fallback (under-count). It was not missed, which the current `NOT_COVERED` text ("can be missed") does not say |
| Denial in the real UI | with remaining $0.64 below the $2.00 hold, `UserPromptSubmit operation blocked by hook` showed the period end in the configured zone, the raise-the-cap remedy and the "not provider quota" wording |
| Recovery | raising the cap applied to the very next prompt with no restart |
| Statusline | the command itself reported `budget $5.36/$6 (90%) · budget low: new sessions blocked`, but the TUI line still showed `/$50` after the denial and only updated after the next model turn (not refreshed after a hook-blocked prompt; whether Claude Code re-runs the statusline only on assistant messages was not established, and Libra sets no refresh interval) |
| Daemon log noise | 4 `connection error ... Broken pipe` lines, one per tool turn: the PostToolUse hook closes before reading the daemon reply; harmless in this run |
| `doctor` | exit 0 with two warnings: no Codex hooks installed (expected) and the flagged settlement from `/compact` |

Not covered by A1: real model latency, a real provider, the founder's real
host, a partly flushed turn under a slow real model (the only partial-flush
evidence is the two `SettleTail` recoveries), subagent spawns by the model, and
independent verification. Classification: no new Preview blocker. The
`/compact` booking and the stale TUI statusline are observed limitations; a
wording change to `NOT_COVERED` or a statusline refresh interval would change
product code and is not made here.

## Known limitations (user-facing)

- Admission-bounded (cooperative), not a strict monetary cap; in-turn
  overshoot is possible.
- Codex is ungated; there is no gateway hard cap.
- Amounts are Libra-accounted list-price-equivalent USD, never the provider
  bill; the 1-hour cache tier, the fast-mode premium and the `inference_geo`
  multiplier are under-counted and Haiku 5.5 prompts under 100k tokens are
  over-counted by about 5x (ADR-0026, "Known pricing limitations").
- Only Claude Code sessions and subagents whose settlement hooks run and
  whose transcripts can be read and attributed are counted.
- Evidence is `TestOwnedLive` (real binaries, synthetic transcripts); nothing
  is observed on a real host.

## Release checklist state

**Superseded on 2026-10-29.** The table below is the pre-candidate state and
is kept as history. The current state of each step is in
`docs/quota-observation/release-checklist.md` (as amended) and HORO-2186.

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
| Founder GO / ADR-0025 acceptance | granted 2026-10-29 for 313d26a09407c8a77d2b4cc7685ea694a822905a (HORO-2186 comment 29520) |

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

Local evidence at code SHA `98b112a16ac79cd4a04b1466ce6e81603d3bfc4c`, repeated at
`ee42bca` after the lockfile bump with the same results (CLI suites 81/81, the
unsafe-path refusal, install, `doctor`, `uninstall` round trip) (under
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

**Superseded on 2026-10-29.** The founder-reviewed release notes are DRAFT 6,
recorded with its sha256 in HORO-2186; the short draft below is history.

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
