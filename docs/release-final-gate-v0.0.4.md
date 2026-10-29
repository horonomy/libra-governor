# v0.0.4 final release gate classification (Cooperative Preview)

Status: **preparation, not a release decision.** `READY_FOR_RELEASE=false`,
`RELEASE_APPROVED=false`, `RELEASED=false`. Nothing is tagged, published or
deployed. Qualified product SHA: `4e56fbd086e8234aec52f0185df8a3b5fa62c271`.
State recorded here: `main` at `fef26173459ed5205057b4948b2796bc35f238f8`
(PR #257), reconciling HORO-2186 comments 29404 (Codex) and 29408 (release
owner). Independent experience score 75/100; Packet A1 PARTIAL with scoped
interactive-host evidence accepted. Jira statuses are stale and are not
evidence.

## Basis for "mandatory"

The founder-approved scope ([ADR-0026](adr/0026-local-monthly-shared-budget.md),
"Founder version-scope decision", HORO-2261) ships the **Cooperative Preview**
"subject to its remaining original applicable QA and release gates", and
states that no publication, deployment, new egress route or **paid model test
is authorised** by that decision. The publication-time requirements are in
[ADR-0025](adr/0025-v0-0-4-scope-evidence-and-exclusions.md). Under that
contract a paid real-provider run (A2) is **not** a stated requirement. What
the contract leaves to the founder is whether shipping with
`REAL_HOST_QA_PASS` still `NOT_VERIFIED` is acceptable; this document does not
waive it.

## Final-accounting evidence (A1)

- **578c is gross logged, not a verified final ledger balance.** It is the sum
  of 14 measured Stops (315c), three `SettleTail` recoveries (63c) and one
  unmeasured-subagent fallback (200c) read from `daemon.final.log`.
- **557c is a pre-final-exit status snapshot**, not a closing balance. No
  post-exit `status` snapshot was retained, so the closing held/settled/
  remaining state, and that the third tail was charged once, were not
  independently observed.
- No accounting defect is confirmed. The tail path has daemon and CLI tests
  (`crates/daemon/tests/shared_budget_settlement.rs`,
  `crates/cli/tests/shared_budget_settlement_hooks.rs`); those are
  `TestOwnedLive` evidence about the code, not about this run's closing state.
- Disposition: **optional, explicitly unverified.** Closing it needs a new
  isolated run that snapshots `status` after the last `/exit`. That is
  proportionate only if the founder wants it; A1 is not rerun without a
  demonstrated evidence gap, and this one does not block the Preview contract.

## Classification of the remaining gates

### Mandatory for the approved Cooperative Preview

| Gate | State | Closed by |
|---|---|---|
| Tag equals workspace version (`scripts/check-release-tag.sh v0.0.4`) | passes on `fef2617` (exit 0); re-run on the pinned SHA | release owner |
| Hosted CI green on the pinned SHA | `CI` and `CodeQL` success on `fef2617`; re-evaluate if `main` moves | release owner |
| Attested pre-verification of `release.yml` on that exact SHA (checklist rows 3, 9) | not done | Packet C (owner-authorized) |
| Artifact verification: sha256, `gh attestation verify`, `--version`, independent Codex check (row 4) | partial; attestation unverified by design until C | after C |
| Clean-machine aarch64 install smoke and rollback on the artifact (rows 7 install part, 10) | partial, sandboxed only | Packet B (owner-authorized) |
| Artifact manifest (row 9) | not done | after C |
| Release notes carrying the five ADR-0026 disclosures | draft in `release-candidate-v0.0.4.md`; in-product text merged | founder review |
| Hardening classification confirmed; ADR-0025 and ADR-0026 accepted | pending | founder |
| SHA-named GO (row 12) | granted 2026-10-29 for 313d26a09407c8a77d2b4cc7685ea694a822905a (HORO-2186 comment 29520) | founder, Packet E |

### Optional but explicitly unverified

- Real provider, real latency and `ProductionObserved` evidence (A2). Not
  required by the approved contract; stays unverified if A2 is declined, and the
  release notes keep the "test-owned runs, not real-host observation" label.
- Post-`/compact` usage on a real provider (ADR-0026 real-host checklist item 2);
  A1 saw it only against the fake upstream, booked at the flat fallback.
- Closing ledger state after the last `/exit` (above).
- A1 custody gaps recorded in comment 29408: before/after global-state hashes,
  per-PID exit codes, cap-edit timeline, `--continue` argv receipts.
- Checklist row 7 extension journey (validate, approve, upgrade/invalidate/
  re-approve): outside the budget feature; stays PARTIAL unless the founder adds it.
- Checklist row 8 byte-identical `settings.json` (semantic equality only).
- Checklist row 11 isolation bundle (stale SHAs; confirm applicable or drop).
- Rows 5 and 6 (x86_64 macOS, Linux): not built by `release.yml`, unsupported,
  no install claim made.

### Owner-authorized (the release owner will not self-grant)

Packet B, Packet C (permanent public Rekor entry), optional A2, Dependabot
PRs #187-#195 (owner-approved review; #191-#195 change release-path actions and
need evaluating before any tag, not blind merging), and the final GO.

### Jira-admin dependent

Fix Version `Agent Execution Economics v0.0.4 / Quota-Aware Execution Pacing`
and the v0.0.4 Goal (HORO-1752 comments 28544, 28725, 29291). The release
owner's tooling has no create-version call. ADR-0025 lists HORO-1752 metadata
among the conditions for `READY_FOR_RELEASE`; it gates the label, not product
safety. Whether HORO-1768 acceptance still applies under the Cooperative
Preview scope is a founder confirmation; its Jira status is stale.

### Approved future work (not Preview blockers without evidence)

S11 gateway enforcement, Codex budget gate, OpenAI Responses gateway, remote
upload, the strict cross-host hard cap, and HORO-2303 (quote-aware hook
ownership, state-dir symlink fix on draft PR #254, subagent path validation,
reinstall double hook set, counter reclaim, repair-clock, `init --force` backup).

## Candidate `FINAL` and equivalence to the qualified product

`main` differs from `4e56fbd` only by documentation and one test-only hunk:
`git diff 4e56fbd fef2617 -- crates` is 50 added lines in
`crates/cli/src/claude_settings.rs` starting at line 928, after the
`#[cfg(test)]` boundary at line 860; `.github/` is identical (`release.yml`
unchanged); `Cargo.toml` and `Cargo.lock` are unchanged. Runtime source is
therefore the qualified source. Byte-identical binaries are **not** claimed:
`release.yml` uses unpinned `toolchain: stable` and `macos-latest`.

`FINAL` is the founder-named SHA; any later merge to `main` invalidates
earlier C evidence. Recommendation: freeze `main` after this document merges
and name that SHA, or name `4e56fbd` itself (an ancestor of `main`) if the
founder prefers a tag on exactly the qualified commit. The first keeps the
packet docs inside the tagged tree; the second keeps the build input identical
to the QA'd source. Either is the founder's choice.

## Next executable gate: Packet C

Order: **C, then B, then D, then E.** C is first because it produces the
attested artifact that B installs and that rows 3, 4 and 9 need.

Release-owner preflight, runnable now without authorization and recorded
2026-10-28 on `fef2617`:

| Check | Result |
|---|---|
| `scripts/check-release-tag.sh v0.0.4` | exit 0 |
| `CI` and `CodeQL` on `fef2617` | success |
| `.github/` vs `4e56fbd` | identical |
| `crates/` vs `4e56fbd` | test-only hunk after `#[cfg(test)]` |
| Open release-path Dependabot PRs | #191-#195 unmerged, so action versions are those already exercised in CI |

On founder approval the owner runs exactly the dispatch in
[Packet C](release-authorization-packets-v0.0.4.md) (`gh workflow run
release.yml --ref main`, verify run `headSha == FINAL`), downloads the artifact,
runs `shasum -c`, `gh attestation verify`, `--version`, compares `rustc -Vv`
with the qualified artifact, and hands the digests to Codex. Reply template:

```
C go: dispatch release.yml on main at FINAL=<40-hex sha>; I accept the permanent public Rekor entry for that SHA and digest.
```

This authorizes no tag, release, deployment or announcement.
