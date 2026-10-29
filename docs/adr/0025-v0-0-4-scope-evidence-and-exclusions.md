# ADR-0025: v0.0.4 scope, evidence class and explicit exclusions

- **Status**: **The founder approved the v0.0.4 Cooperative Preview scope and
  the documented risk dispositions on 2026-10-29 (D5, recorded in HORO-2186).**
  That approval did not itself authorize a release. The founder then issued a
  separate SHA-specific GO (D8) on 2026-10-29 for v0.0.4 at
  `313d26a09407c8a77d2b4cc7685ea694a822905a` (the frozen release SHA after PR #259; it supersedes
  `ba3638055e7d9c9378947a4c272277be0db02112`, the earlier candidate), recorded by the release owner at
  HORO-2186 comment 29520. The founder pushed tag `v0.0.4` on that SHA; release run
  40843550017 built, attested and published the Release. Post-publication
  verification is recorded in HORO-2186. Engineering scope
  was set by Option 2
  ([ADR-0026](0026-local-monthly-shared-budget.md), HORO-2248): v0.0.4 is the
  local monthly shared-budget MVP, labelled Cooperative Preview. The sections
  below keep the evidence classes and the earlier narrow Apple Silicon Preview
  proposal as history; the section "Release dispositions (founder, 2026-10-29)"
  at the end records what the Preview does and does not claim.
- **Tickets**: HORO-1749 (epic), HORO-1768 (release gate), HORO-1727,
  HORO-1857, HORO-2160, HORO-2162. Modelled on
  [ADR-0014](0014-v0-0-3-go-scope-and-exclusions.md); `release/v0.0.3` is a
  separate line and is untouched.
- **Native Jira Fix Version**: when this ADR was first written none existed
  (HORO-1752). Since 2026-10-28 the unreleased Fix Version 10315 ("Agent
  Execution Economics v0.0.4 / Quota-Aware Execution Pacing") exists and is
  set on the `[Libra v0.0.4]` ticket family (36 issues at 2026-10-29), and Goal
  CBLPCRLM-71 is linked to Epic HORO-1749. The scope below is that ticket
  family; the Fix Version has no release date and is not released.

## Evidence classes used here

`Simulation` (deterministic replay of frozen traces), `TestOwnedLive` (a real
daemon/binary driven by a test that owns every input), `ProductionObserved`
(a real host with real provider usage). Nothing below is `ProductionObserved`
unless it says so.

## In scope and evidenced

| Capability | Evidence class | Pointer |
|---|---|---|
| Turn-scoped default for an unconfigured daemon (400k per-turn ceiling, no lifetime total shown), fail-closed rollback | TestOwnedLive (real binary, real daemon) | ADR-0024; `crates/cli/tests/turn_scope_dogfood.rs`; HORO-1727 QA_PASS_WITH_FOLLOWUPS |
| Metered shared quota pool: atomic reserve/settle/release, idempotency, concurrent race, duplicate settlement | TestOwnedLive (ledger and daemon tests) | ADR-0023; `crates/ledger/tests/shared_pool_adversarial.rs` |
| Pacing gate (sustain/burst planning, hard-limit respect on controlled paths, negative controls that must fail) | Simulation, plus a test-owned daemon consistency check | `crates/pacing-gate` (10 frozen fixtures incl. `t7_burst_to_sustain`, `x0_unguarded`); `crates/cli/tests/launch_live_occupancy_consistency.rs` |
| HORO-1768 AC4 (corrupt snapshot, reservation race, lost response) | TestOwnedLive | test names listed in `crates/pacing-gate/src/verify.rs` module docs |
| Hooks can run as plain short-lived clients (`LIBRA_GOVERNOR_NO_AUTOSPAWN`) | TestOwnedLive | HORO-2160; `crates/cli/tests/hook_no_autospawn.rs` |
| A release tag must equal the workspace version before anything is published | Positive and negative controls on the script; workflow step not exercised on a real tag | HORO-2162; `scripts/check-release-tag.sh` |

## Explicitly NOT claimed

1. **Real-host pacing value.** No `ProductionObserved` evidence exists that
   pacing delivers more completed work per quota than a static budget. The
   gate's verdict is structurally unable to reach GO from simulation
   evidence (`crates/pacing-gate/tests/verdict_gating.rs`). HORO-1768
   AC1/AC3 are met at `Simulation`/`TestOwnedLive` only.
2. **Cross-host live attribution.** HORO-1599 (provider-native identity
   capture) and HORO-1714 (Codex lifecycle and economic binding) are still
   In Progress, and HORO-1718 independent Codex QA has `QA_SIGNED_OFF=false`.
   HORO-1768 AC5 forbids promoting active scheduling while they are open.
3. **Early-warning/shadow decisions** (HORO-1693) remain unproven, as in
   ADR-0014. Hierarchical custody (HORO-1694) stays latent.
4. **A published preview.** No release has ever been published from
   `release.yml`. Only the `aarch64-apple-darwin` artifact has a recorded
   install test (Jira HORO-1857 comment 27560); `x86_64-apple-darwin` and
   `x86_64-unknown-linux-gnu` are built in CI but their install is
   UNVERIFIED. Windows is unsupported.
5. **Lifetime ceilings for long-running agents.** The turn-scoped default
   deliberately removes the lifetime cap for unconfigured daemons; whether
   gateway authority should honour the persisted turn scope is an open
   founder decision (HORO-1899).

## Proposed release target: Apple Silicon macOS Preview

**Earlier proposal, kept as history (see Status).** The narrowest honest v0.0.4 was a *preview for `aarch64-apple-darwin` only*.

| Tier | Content | Status |
|---|---|---|
| Claimed (Golden Journey) | Turn-scoped default with fail-closed rollback; metered pool reserve/settle/release; pacing gate with negative controls; hooks as plain clients (`LIBRA_GOVERNOR_NO_AUTOSPAWN`); tag == workspace version. Budget enforcement and data integrity are not relaxed. | `TestOwnedLive` / `Simulation` |
| Claimed (delivery) | `aarch64-apple-darwin` tarball built from the final SHA, with sha256, attestation and `--version` verified | Publication-time (below) |
| Not claimed | `x86_64-apple-darwin` and `x86_64-unknown-linux-gnu` installation (built in CI, unsupported-unverified); Windows (unsupported) | Out of Preview scope |
| Not claimed | Real-model Codex native activation; `ProductionObserved` pacing value; cross-host attribution (HORO-1599/1714/1718) | Out of Preview scope |
| Not claimed | Governed (daemon-present) behaviour on Codex; the HORO-2160 offline bundle supports isolation QA only | Out of Preview scope |

Release notes for a Preview must say: Apple Silicon only; observation and
turn-scoped budget enforcement, not active pacing; no cross-host
attribution claim. Deferring the out-of-scope rows waives no security or
data-loss risk: they are claims the Preview does not make.

### Publication-time requirements (none performed yet)

1. `main` at the intended SHA; `scripts/check-release-tag.sh vX.Y.Z` passes.
2. Hosted CI green on that SHA.
3. Pre-verify with a `workflow_dispatch` run of `release.yml` on that exact SHA. It builds and attests but never publishes. Attestations are public and permanent, so this needs the founder's GO naming the SHA.
4. Verify tarball sha256, `gh attestation verify`, `--version` equals the tag, and a clean install smoke on Apple Silicon.
5. Record the artifact manifest (file, size, sha256, source SHA, run URL).
6. Founder GO for the tag step itself. A `v*` tag push **publishes the GitHub Release immediately and irreversibly**, so it comes only after items 3-5.
7. **Scope enforcement gap:** `release.yml` builds, attests and publishes all three targets. The Preview does not claim `x86_64-apple-darwin` or `x86_64-unknown-linux-gnu`, so the release notes must label those assets unsupported-unverified, or the founder may choose a separate, reviewed change that restricts the build matrix to `aarch64-apple-darwin`. Neither has been done.
   *Resolved after this item was written:* PR #238 restricted the `release.yml`
   build matrix to `aarch64-apple-darwin`; at FINAL the workflow builds and
   attests that target only (unchanged at the frozen SHA). The release notes still label every other
   platform unsupported-unverified.

## Consequence

Until the items in "Explicitly NOT claimed" are closed or the founder
accepts them as exclusions, v0.0.4 makes no active-pacing claim (the
earlier proposal was a narrow preview with these limitations). Moving
v0.0.4 to READY_FOR_RELEASE additionally requires HORO-1752 metadata, HORO-1768
acceptance and the HORO-1857 release checklist
(`docs/quota-observation/release-checklist.md`).

## Release dispositions (founder, 2026-10-29)

Decisions recorded in the v0.0.4 release-owner session (relayed in HORO-2186; the Jira author field cannot separate founder from agent, so these are relays, not proof). None of them waives a gate that was not explicitly named.

- **D2, settings restoration.** Accepted for the Preview: semantic preservation of `settings.json` plus a byte-identical install-time backup (`settings.json.libra-backup-*`). `uninstall` does **not** restore `settings.json` byte-for-byte (it re-serializes the JSON) and leaves the backup and `settings.json.horonom-write.lock` files. Literal byte-identical restoration (checklist row 8) is **not** met and is not marked PASS. Disclosed in the release notes.
- **D3, install/uninstall acceptance.** Accepted: independent B1 evidence (Codex PASS_SCOPED_SECOND_USER_TEST_OWNED_LIVE): a clean second macOS user on the same Apple Silicon Mac, attested binary, synthetic data. This is **not** a fresh-machine qualification. Rollback of a published artifact is **unverified** and is tracked as a post-publication rehearsal.
- **D4, isolation bundle.** The HORO-2160 offline isolation bundle (checklist row 11) is retired as a Preview gate where its currently applicable invariants are covered by accepted B1/C and `crates/cli/tests/hook_no_autospawn.rs`. Historical evidence is preserved. Not covered on FINAL: escaping-descendant checks, no-daemon write footprints, hook executable-hash manifest, isolation of the other four hook commands, Codex-route isolation.
- **D7, dependencies.** Dependabot PRs #187–#195 are deferred to a separately reviewed post-release maintenance workstream; the three non-shipped `filelock` alerts (`experiments/phase0/uv.lock`) are tracked separately. `cargo audit` is clean at FINAL.
- **D1, extension journey (checklist row 7).** Independent Codex review c29453: PASS_SCOPED_ISOLATED_ARTIFACT (producer-run isolated evidence on the attested binary; not independently rerun; not a fresh machine or provider).
- **Release-note wording (founder, 2026-10-29).** The release notes must (1) state that admission and refusal apply only while the daemon and hooks run correctly and disclose fail-open behaviour when the daemon is unavailable (tool-call checks and end-of-turn/session settlement fail open; prompt admission refuses when a budget is configured and the daemon does not answer); (2) never promise universal flat-hold accounting: unmeasured or unattributed spend can settle at the flat hold, be recorded as unattributed, or be missed; (3) distinguish real Claude Code execution against a fake upstream from genuine provider billing, latency and production observation.
- **Not claimed by the Preview:** strict cross-host hard cap; Codex enforcement; ProductionObserved real-provider correctness; demonstrated pacing productivity; any platform other than Apple Silicon macOS.
- **D5, scope and risk dispositions approved (founder, 2026-10-29).** The founder approved the Preview scope, the release-note amendments (known pricing errors, conditional settlement-recovery wording, and the `init --force`, hook-ownership, clock-anomaly, settlement-sweep and install-path limitations, with the fail-open disclosure) and the risk dispositions recorded in this section. The approval does not authorize a tag, GitHub Release, deployment, publication or announcement; only a separate SHA-specific GO (D8) can.
- **D5, hardening classification.** The 14-row classification in `docs/release-candidate-v0.0.4.md` is confirmed conditionally for the narrow Cooperative Preview. It is not a blanket security waiver, no row verdict was changed, and every unresolved item stays traceable to Jira (HORO-2303, HORO-2250, HORO-2179, HORO-2293). Rows 5 and 11 were reassessed against source; see the reassessment note in that file.
- **D5, release sequencing exception.** Packets section D asked for ADR-0025 / ADR-0026 to move to Accepted "in the same change as the GO", and checklist row 12 asks for "ADR-0025 moved to Accepted". Neither is satisfied inside the tagged tree: a GO that names FINAL cannot be contained in FINAL, and the ADR, checklist and packet text in the tagged tree stays as it was at FINAL. The founder approved an explicit documented exception: the approved HORO-2186 decision record plus the SHA-specific D8 GO are the authoritative pre-tag release decision, and this text is updated in a docs-only follow-up commit made only after a separately authorized successful tag and release. The original discrepancy is preserved in the checklist. Verified 2026-10-29 at FINAL: no CI workflow, `release.yml` step or script reads ADR, checklist or packet text, so no automated gate requires ADR status inside the tagged tree. If any mandatory automated or security gate later requires it, the exception does not apply and a new-SHA qualification is required.
- **D5, HORO-1768 (Preview-only disposition).** HORO-1768 stays OPEN/PARTIAL. Its original acceptance criteria and dependencies are preserved and not claimed: AC1 (measured successful work per quota) and AC3 (live controlled end-to-end test consistent with simulation) are not satisfied, and AC5's real-host acceptance dependencies (HORO-1714 and HORO-1599) are still open. The original active-pacing release gate remains NO-GO for any unverified active-pacing claim. The Cooperative Preview is a separate, explicitly authorized product scope that makes no claim about real-provider pacing productivity, ProductionObserved correctness, native Codex budget enforcement or a strict cross-host hard cap.
- **D5 closeout, release-notes wording (founder, 2026-10-29).** Disclosure 7 is approved: Libra is NOT a security boundary against a governed agent operating under the same OS user identity. The additional limits on late records (recoverable by a later Stop or SettleTail where supported; already-settled windows not guaranteed to be re-measured), flush timing (fake-upstream evidence only; no real-provider latency claim) and charges above ten times the monthly cap (Libra declines to record the charge; the provider's billing is unaffected) are approved with those corrections.
- **D5 closeout, hardening rows 5 and 11.** Accepted conditionally within the documented default private-state and same-user trust boundaries. The row 5 FIFO guard is established by source inspection only; no executable subagent-path QA exists. No protection against an active same-UID adversarial agent is claimed, and HORO-2303 stays open. This is not a security waiver.
- **D5 closeout, first-publication release notes: gate closed in engineering by PR #259 (HORO-2322).** The earlier candidate `ba3638055e7d9c9378947a4c272277be0db02112` (never tagged) would have published auto-generated notes only. At the frozen SHA `313d26a09407c8a77d2b4cc7685ea694a822905a`, `release.yml` checks out the tagged source, refuses to publish unless `docs/release-notes/<tag>.md` exists, is non-empty, is under 100000 bytes and contains nine mandatory disclosure markers, and then publishes it as the body with `generate_release_notes: false`. Evidence: 14 guard controls (producer, re-executed by independent QA), and a private test-owned scratch-repository rehearsal (first body byte-equal to the tag-commit file; three negative tags failed before any release or draft). Limits: the rehearsal used synthetic assets, no build/attest jobs and no attestation verification, so the real tag path is verified only by post-publication body comparison.
- **D5, HORO-1727 calibration.** Reconciled from QA evidence, not Jira status: independent QA reported QA_PASS_WITH_FOLLOWUPS on main `f6ab5bd` (HORO-1727 comment 28448, 2026-10-22), and the founder directive in comment 27893 withdrew the earlier ~5x miscalibration claim after the HORO-1882 de-duplication correction. The ticket is still In Progress in Jira and that status is not evidence either way.
