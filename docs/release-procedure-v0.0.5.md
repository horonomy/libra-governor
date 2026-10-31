# Libra Governor v0.0.5: release criteria and candidate procedure

Status: DRAFT. This is a procedure and a checklist, not a release claim.
Baseline: tag `v0.0.4` = `313d26a` (immutable). Epic: HORO-2325.

"Technical READY_FOR_RELEASE" means every criterion below is met at one named
`main` SHA with the evidence recorded. It is not publication. Tagging, creating a
GitHub Release or activating anything needs a separate, SHA-specific founder GO,
which is outside these criteria.

## Release criteria

| ID | Criterion | Evidence that satisfies it |
|---|---|---|
| R1 | Every PR in the release scope set is merged to `main` as a merge commit through the supported owner merge gate, with the reviewed head pinned. | `owner-merge-gate.py verify` returns `VERIFIED` for each PR (merge commit has the reviewed head as a parent and is on `main`). |
| R2 | Hosted CI is green on the final `main` SHA. | The seven required checks (`cargo fmt`, `cargo clippy`, `cargo build & test` on ubuntu and macos, `Analyze` for actions, python and rust) are `success`; SonarCloud gate is `OK`. Skipped release-only jobs are listed, not counted. |
| R3 | Candidate integration check passes on a clean checkout of the final SHA. | `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo nextest run --workspace` all exit 0, in an isolated `CARGO_TARGET_DIR`, with the test counts recorded. A known flaky test is allowed only with a ticket and a rerun that passed. |
| R4 | Claude Code behavior is byte-exact. | The protocol golden-byte tests and the Claude hook goldens are part of R3 and are named in the evidence. Optional fields are omitted from the wire when absent. |
| R5 | Codex enforcement and S11 are off by default, and the disabled state is shown by tests, not asserted. | Named tests pass at the final SHA: Codex `Admit`/`Check` answer `not_configured` and write nothing; with the enrolment marker absent a whole-ledger before/after comparison shows no change for C2 settlement, C3 hook wiring and C4 recovery; removing the marker returns to inert; Claude golden-byte and hook tests are unchanged. No "byte-identical" claim is made unless a test compares bytes. The release text states the gate is not enabled. |
| R5a | Each of C2 to C5 meets its ticket acceptance criteria. | The acceptance matrix `ACCEPTANCE-C2-C5.md` maps every criterion to a named passing test or to an explicit "not verifiable here" statement. |
| R6 | The release packet is accurate. | `docs/release-candidate-v0.0.5.md` is refreshed with the final SHA, release notes, rollback and known limitations. Labels stay honest: source-verified, not verified against a real provider. |
| R7 | Every merged PR has an independent review record on its exact head, and no Critical or High finding is open. | `OWNER-MERGE-REVIEW` comment per PR; Medium and Low findings are ticketed. |
| R8 | Jira is reconciled. | Fix Version and Goal exist (or the absence is documented with the supported-interface attempt), each epic child has a disposition and an evidence comment. |
| R9 | Dependencies are accounted for. | Each open dependency PR and advisory is listed as required or not required, with the reason. Only required ones block. |
| R10 | Compatibility is documented. | Protocol v25: the daemon and the CLI must come from the same release; a v24 daemon rejects a request that carries `provider`. |

## Candidate integration check (R3, R4, R5)

Run only after every in-scope PR is merged (R1).

1. `git fetch origin`; record `SHA=$(git rev-parse origin/main)`.
2. Create a detached worktree of that SHA. Do not reuse a working branch.
3. Wait for resource admission for a heavy workload. Reserve disk for the build.
4. Use a dedicated build directory for this run: `export CARGO_TARGET_DIR=<scratch>/candidate-$SHA`.
   Do not use a target directory shared with other worktrees: artifacts of
   different source trees with the same crate names were observed to be reused
   across worktrees, which gives false failures and false passes (Config recorded
   this in SPE-150).
5. Run, in order, and record each exit code and the test summary line:
   - `cargo fmt --all -- --check`
   - `cargo clippy --workspace --all-targets -j 4 -- -D warnings`
   - `cargo nextest run --workspace -j 4 --no-fail-fast`
6. Delete the build directory afterwards.
7. Store the log next to the release evidence, with the SHA on the first line.
8. Confirm hosted CI for the same SHA (R2) and record the check names.

The operator's script for this host is kept with the evidence
(`libra-release-evidence/v0.0.5-20261029/candidate-gate.sh`); this document is the
authoritative description of the procedure.

## Merge procedure per PR (R1, R7)

1. Independent review of the exact head. Findings above Medium block.
2. Required checks green on that head. A failed check that actually ran is
   investigated and not bypassed; a flaky test is ticketed and rerun once.
3. `owner-merge-gate.py check`, then the evidence file, then the printed merge
   command alone, then `owner-merge-gate.py verify` in a separate call.
4. If the branch is behind `main`, merge `main` into it (never rebase or force-push),
   re-validate and re-review the new head.

## Scope notes

- Option 1 (founder, 2026-10-31): C0 to C5 are in v0.0.5; the Codex integration stays disabled by default.
- Not delivered in v0.0.5 and not claimed: the S11 gateway chain (HORO-2273 to 2279; S11a needs real-provider capture), HORO-2286 and HORO-2191.

## Out of scope for these criteria

- Tag, GitHub Release, deployment or any publication: separate founder GO for a named SHA.
- Real-provider spend or capture: not authorized for this release.
- Enabling any Codex budget control or the S11 gateway: not part of v0.0.5.
