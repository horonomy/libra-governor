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

## Consequence

Until the items in "Explicitly NOT claimed" are closed or the founder
accepts them as exclusions, the honest v0.0.4 statement is a **narrow
preview with these limitations**, not an active-pacing release. Moving this
ADR to Accepted requires the founder's decision; moving v0.0.4 to
READY_FOR_RELEASE additionally requires HORO-1752 metadata, HORO-1768
acceptance and the HORO-1857 release checklist
(`docs/quota-observation/release-checklist.md`).
