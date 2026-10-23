# ADR-0025: v0.0.4 scope, evidence class and explicit exclusions

- **Status**: Proposed. This records what the repository can and cannot
  honestly claim for v0.0.4. It is **not** a GO verdict: the final GO/NO-GO
  and any tag or publication belong to the founder.
- **Tickets**: HORO-1749 (epic), HORO-1768 (release gate), HORO-1727,
  HORO-1857, HORO-2160, HORO-2162. Modelled on
  [ADR-0014](0014-v0-0-3-go-scope-and-exclusions.md); `release/v0.0.3` is a
  separate line and is untouched.
- **Native Jira Fix Version**: none exists yet (HORO-1752). The scope below
  is the `[Libra v0.0.4]` ticket family, not a Jira-queryable release.

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

**Proposed, pending the founder's decision; this ADR remains Proposed.** The
narrowest honest v0.0.4 is a *preview for `aarch64-apple-darwin` only*.

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

## Consequence

Until the items in "Explicitly NOT claimed" are closed or the founder
accepts them as exclusions, the honest v0.0.4 statement is a **narrow
preview with these limitations**, not an active-pacing release. Moving this
ADR to Accepted requires the founder's decision; moving v0.0.4 to
READY_FOR_RELEASE additionally requires HORO-1752 metadata, HORO-1768
acceptance and the HORO-1857 release checklist
(`docs/quota-observation/release-checklist.md`).
