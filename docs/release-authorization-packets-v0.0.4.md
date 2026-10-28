# v0.0.4 release authorization packets (Cooperative Preview)

Status: **requests, not approvals.** Nothing here is authorized until the
founder replies with the exact text under "Authorization reply" for that
packet. Nothing is tagged, published or deployed. `READY_FOR_RELEASE=false`,
`RELEASE_APPROVED=false`, `RELEASED=false`.

Qualified product SHA: `4e56fbd086e8234aec52f0185df8a3b5fa62c271` (PR #250,
Codex c29344). Later `main` commits are docs-only except test-only PR #252
(`694427f`, its only hunk is in the `#[cfg(test)]` module). `FINAL` below means
the exact `main` SHA the founder names for the tag; it is chosen last.

**Equivalence check to run before every packet, and before the GO.** Product
inputs of `FINAL` must equal the qualified SHA apart from the allowed delta:

```bash
git diff --stat 4e56fbd FINAL -- crates Cargo.toml Cargo.lock .github/workflows/release.yml scripts
# allowed: crates/cli/src/claude_settings.rs only, hunks after the #[cfg(test)] line
```

Anything else means the qualification no longer covers `FINAL`; stop and
request an affected-only Codex re-QA.

Separate gates, never collapsed: current-host/interactive acceptance (A),
clean-machine install and rollback (B), attested artifact pre-verification
(C), release metadata and checklist (D), and the final SHA-specific GO (E).

---

## A. Current-host and interactive acceptance

**Closes:** `REAL_HOST_QA_PASS` (currently NOT_VERIFIED): interactive terminal
Stop flush timing, a partly flushed turn, post-/compact behaviour, and the live
denial/statusline experience on the current code. Cap on the experience score:
B stays 8/20 without this.

Two tiers, so the founder can approve the cheap one without the paid one.

### A1. Real Claude Code binary, isolated, no network, no spend

Local isolated testing is already within the founder's standing authorization
(no paid inference, no live-host config). It is listed here because it spawns
an interactive `claude` process; reply "A1 go" and the release owner runs it.

- **Actions:** `claude` 2.1.306 in a pseudo-terminal (`tmux`), `HOME` and
  `LIBRA_GOVERNOR_STATE_DIR` in a fresh temp directory, dummy API key,
  `ANTHROPIC_BASE_URL` pointing at a local fake Messages-API upstream, inside a
  `sandbox-exec` profile that denies all network except loopback. Libra binary:
  the CI-built aarch64 artifact of `FINAL` (hash recorded), installed with
  `install --hooks-only` into the temp HOME only.
- **Measures:** Stop-hook settle timing per turn (`TranscriptReady` waits,
  flat-hold fallbacks) across at least 12 interactive turns including 4 with
  tools and 2 resumed sessions; the interactive partial-flush case (turn
  settled before its last record, then recovered by the next Stop/SettleTail);
  a `/compact` turn; cap exhaustion producing the denial text and `budget low`
  statusline in the real UI; `doctor` before and after.
- **Does not:** touch the real `~/.claude`, the real state dir, any real
  credential, the network, or any paid model. The fake upstream answers
  instantly, so real model latency stays unmeasured; the report says so.
- **Evidence class:** `TestOwnedLive` (real binary, fake upstream). Not
  `ProductionObserved`.
- **Stop conditions:** any process outside the temp tree touched; any
  non-loopback connection attempt; a prompt that asks for real login.
- **Rollback:** delete the temp tree. Nothing else changed.
- **Cost:** none beyond local CPU and about 1 GiB; SPE-109 admission applies.

### A2. Founder's real host, real provider (live-host change and paid inference)

Needs explicit approval of every line below. Not required for the Preview if
the founder accepts A1 as the interactive evidence and records that
`ProductionObserved` evidence does not exist.

- **Before install:** record whether `~/.local/state/libra-governor` and Libra
  hooks already exist on the host. If they do, back up the state directory, or
  abort and use a scoped `LIBRA_GOVERNOR_STATE_DIR` so the test spend does not
  enter an existing ledger.
- **Actions:** back up `~/.claude/settings.json` (install also writes a
  `.libra-backup-*` file), `libra-governor install --hooks-only`, `shared-budget
  init --monthly-usd <CAP> --time-zone <ZONE>`, run **at most N interactive
  prompts** with a Haiku-class model, one `/compact`, one deliberate cap
  exhaustion, `doctor`; then `libra-governor uninstall` and compare
  `settings.json` with the backup.
- **Bounds the founder must fill in:** `CAP` (suggest USD 5), `N` (suggest 8,
  short prompts), model, and a maximum real provider spend ceiling. Libra's cap
  is cooperative, not a hard stop, so the prompt count is the real bound.
- **Irreversible/at-risk:** real provider spend; the prompt text goes to the
  provider as usual (Libra stores no prompt text); the hook set runs on the
  founder's sessions for the duration.
- **Rollback:** `uninstall`, restore `settings.json` from the backup, delete
  `~/.local/state/libra-governor` only with explicit confirmation, and remove
  the binary (`cargo uninstall libra-governor-cli` for a source install, or
  delete the copied file for a tarball install; `uninstall` does not remove it).

**Authorization reply (A1):** `A1 go: run the isolated interactive acceptance on FINAL.`
**Authorization reply (A2):** `A2 go: CAP=<..>, N=<..>, model=<..>, spend ceiling=<..>, window=<..>.`

---

## B. Clean-machine installation and rollback

**Closes:** the clean-machine rows of the release checklist (`release-checklist.md`
row 10 and the install/rollback part of row 7; rows 5 and 6 are out of scope for the aarch64-only Preview), `RELEASE_CHECKLIST_PASS` in part. Row 7 is the extension journey (validate, approve, inspect, upgrade/invalidate/re-approve): these packets do not run it, so row 7 stays PARTIAL unless the founder asks to add those steps or accepts it as outside the budget feature. The warm private fixtures used
so far are not a clean machine.

There is no VM tooling on this Mac and only one user account (`bryant`). The
options, all of which need an authorization:

| Option | What changes | Notes |
|---|---|---|
| B1 second macOS user | `sudo sysadminctl -addUser` creates a local account; removed afterwards | closest to a clean machine on this host; needs admin rights and leaves a home dir to delete |
| B2 another Apple Silicon Mac | nothing on this host | the founder runs the commands below and returns the transcript |
| B3 skip | none | record that clean-machine install is UNVERIFIED and accepted by the founder |

- **Actions (B1 or B2):** with no Libra state and no prior hooks:
  1. Source path (the documented install): `git clone`, `./scripts/install.sh`
     (needs a Rust toolchain; record its absence/presence), then `doctor`,
     `shared-budget init`, `status`.
  2. Tarball path (only after packet C produced an attested artifact):
     `shasum -c`, attestation verification, `--version`, copy to a plain path,
     `install --hooks-only`, `doctor`, `init`, `status`.
  3. Rollback: `uninstall`, then compare `settings.json` with the pre-install
     copy; confirm foreign hooks untouched and the state dir left in place
     without `--yes`; then `uninstall --yes` and confirm removal.
- **Attestation verification without credentials:** the release owner downloads
  the attestation bundle (`gh attestation download <tar> --repo horonomy/libra-governor`)
  under their own authenticated account; the clean user then runs
  `gh attestation verify <tar> --bundle <file> --repo horonomy/libra-governor`
  (whether that works unauthenticated is to be confirmed and recorded), or the
  verification is recorded from the owner account. The clean user is never
  given the founder's token.
- **Does not:** publish anything; use the founder's credentials or any paid
  service; touch the founder's own account (B1/B2).
- **B1 notes:** create the account with `sysadminctl -addUser <name> -password -`
  (prompted, never a password on the command line); an admin-created user can
  receive a SecureToken and appear on the FileVault unlock screen until
  deleted. Rollback of the test itself: `sudo sysadminctl -deleteUser <name>`
  (removes the home directory by default; `-secure` is meaningless on APFS);
  recorded in the evidence.
- **Cost/risk:** an admin action on the host (B1); a few GiB of disk; SPE-109
  admission applies.

**Authorization reply:** `B1 go` / `B2 go (I will run it)` / `B3 skip, record UNVERIFIED`.

---

## C. Attested artifact pre-verification (`workflow_dispatch`)

**Closes:** checklist rows 3, 4, 9 (attestation verify, artifact manifest) and
`gh attestation verify` for the aarch64 tarball. This is the only way to
produce an attested artifact without a tag.

- **Exact action:** after confirming `git rev-parse origin/main == FINAL`:

  ```bash
  gh workflow run release.yml --repo horonomy/libra-governor --ref main
  gh run list --repo horonomy/libra-governor --workflow release.yml --event workflow_dispatch --branch main --limit 3 --json databaseId,headSha,createdAt
  # take the run created after the dispatch, not an older one
  # the run's headSha must equal FINAL; if not, cancel the run and stop
  ```
- **What the workflow does on dispatch (read from `release.yml`):** the
  `build (aarch64-apple-darwin)` job (permissions: contents read) builds,
  smoke-tests `--version`, assembles the tarball, sha256 and `PROVENANCE.txt`,
  and uploads a workflow artifact kept 14 days. The `attest` job
  (permissions: `id-token: write`, `attestations: write`) runs
  `actions/attest-build-provenance` on the tarball. `create-release` is
  skipped: it requires `github.event_name == 'push'` on a `v*` tag.
- **Irreversible effects (the reason this needs approval):** the repository is
  **public**, so the attestation is recorded in the **public Sigstore Rekor
  transparency log, permanently**, naming this repository, the workflow, the
  commit `FINAL` (ref `refs/heads/main`) and the tarball digest: Rekor
  permanently holds the signing certificate and the signed attestation entry.
  It also creates a publicly readable GitHub attestation record, which a
  repository admin may be able to delete through the attestations API
  (unverified); the Rekor entry cannot be withdrawn.
- **Does not:** create a tag, a GitHub Release, a deployment, or any package; it
  spends only a few hosted macOS-runner minutes.
- **Known risk to record:** `release.yml` uses `toolchain: stable` (unpinned),
  so the compiler at the later tag build may differ from the one used here and
  the bytes may differ; `runs-on: macos-latest` and the cache action are also
  unpinned, so the runner image is recorded from `PROVENANCE.txt`. The evidence therefore compares `rustc -Vv` in each
  `PROVENANCE.txt`; pinning the toolchain is an optional, separately reviewed
  change and is not part of this packet.
- **Verification after the run (release owner, no further authorization):**
  `gh run download <id>`; `shasum -c`; `gh attestation verify <tar> --repo
  horonomy/libra-governor`; `--version`; hash compared with the
  independently qualified artifact `fb2376a5…` (binary of hosted run 40516615874, verified by Codex in HORO-2186 c29344) and explained if different
  (different source commit, toolchain). Codex then verifies the attested
  artifact (sha256, version, sandboxed lifecycle); the release owner does not
  self-certify it.
- **Stop conditions:** run `headSha` is not `FINAL`; any job other than
  build/attest runs; `create-release` is not skipped.
- **Rollback:** none for the Rekor entry. The workflow artifact expires on its
  own after 14 days.

**Authorization reply:** `C go: dispatch release.yml on main at FINAL=<sha>; I accept the permanent public Rekor entry for that SHA and digest.`

---

## D. Remaining release metadata and checklist

Things only an authorized person can do, or that wait on A-C:

| Item | Who | Exact content |
|---|---|---|
| Jira Fix Version | Jira project admin (the release owner's tooling cannot create versions; HORO-1752 c29291) | create an unreleased version named `Agent Execution Economics v0.0.4 / Quota-Aware Execution Pacing`, then the release owner sets it on HORO-1752 and HORO-1857 and reads it back |
| Jira Goal | Jira admin / founder | create the v0.0.4 Goal requested in HORO-1752 comments 28544 and 28725 and link it |
| Checklist rows 3, 4, 9 | release owner, after C | run URL, digests, manifest, attestation result |
| Checklist row 10 and the install/rollback part of row 7 | release owner, after B | install and rollback transcripts |
| Row 8 byte-identical config | release owner | `settings.json` is re-serialized by install/uninstall, so semantic equality is the evidence unless the founder wants a byte-preserving change (future work, not planned) |
| Row 11 offline isolation bundle | already recorded (c28714/28715); stale SHAs | confirm still applicable to `FINAL` or drop |
| Release notes | release owner, founder review | the draft in `release-candidate-v0.0.4.md`; there is no CHANGELOG file and `generate_release_notes: true` is set on the real release |
| ADR-0025 / ADR-0026 status | founder | move to Accepted in the same change as the GO |
| Dependabot PRs #187-#195 | founder (owner approval is required; none is fabricated) | decide per PR; #191-#195 touch the release actions (`attest-build-provenance` 2 to 4, `upload/download-artifact` 4 to 7/8) and should be evaluated for the release path before any tag, not merged blindly |
| Approved future work (HORO-2303, draft PR #254) | founder | recommended: hold for v0.0.5; not a Preview blocker |

---

## E. Final SHA-specific GO (nothing is authorized before this)

A GO has to name the SHA and what it covers. Template for the founder:

```
GO v0.0.4 Cooperative Preview
SHA: <40 hex chars, equals FINAL>
Authorizes: create tag v0.0.4 on that SHA via the founder-run tag push, and the
resulting release.yml run (build, attest, create-release) for aarch64-apple-darwin only.
Evidence reviewed: packets A, B, C, D as closed in HORO-2186 comment <id>.
Not authorized: any other SHA, platform, announcement or deployment.
```

Pre-GO check by the release owner: `scripts/check-release-tag.sh v0.0.4` on
`FINAL`; `main` CI green on `FINAL`; the equivalence check above; no open
mandatory finding; `READY_FOR_RELEASE` is only set true after A to D are
closed with evidence, and `RELEASE_APPROVED` only by the GO.
