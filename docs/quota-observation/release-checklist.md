# Preview release checklist (HORO-1857 AC6)

A checklist an owner completes **before** pushing a `v*` tag. Nothing here
authorizes a release; publication needs separate founder approval. Fill the
right-hand column with evidence (command output, run URL, digest) — leave
`UNVERIFIED` rather than guessing.

| # | Check | Result |
|---|---|---|
| 1 | `main` is at the intended commit; `Cargo.toml` workspace version equals the tag without the leading `v` (`scripts/check-release-tag.sh vX.Y.Z`) | PARTIAL (local, 2026-10-27): `scripts/check-release-tag.sh v0.0.4` on main exits 0 ("matches workspace version 0.0.4") and `v0.0.5` exits 1. Re-run on the final SHA named in the GO. |
| 2 | Hosted CI is green on that commit, or each unavailable check is classified `CI_UNAVAILABLE_EXTERNAL` with local-equivalent evidence | CI_PASS for code SHA `98b112a` (push run 40494287855, `CI` workflow success; PR heads also green). `5aec5a1` and `fae8309` are stale. Re-evaluate if `main` moves before the tag. |
| 3 | A `workflow_dispatch` run of `release.yml` built from that exact SHA (record run URL); the proposed Apple Silicon Preview requires `aarch64-apple-darwin`, `release.yml` builds and attests only that target (the x86_64 legs were removed so no unverified asset is published as supported) | UNVERIFIED |
| 4 | `aarch64-apple-darwin` tarball: sha256 matches, `gh attestation verify` passes, `--version` prints the tag version | PARTIAL: a CI-built artifact from PR #238's build-only run 40481551611 (source `34c9b33`) passed `shasum -c`, prints `libra-governor 0.0.4`, Mach-O arm64. `gh attestation verify` is UNVERIFIED by design: PR runs skip attestation; the attested artifact needs the `workflow_dispatch` run (separate approval, public Rekor entry). |
| 5 | `x86_64-apple-darwin` tarball installed and smoke-tested on an Intel Mac | UNVERIFIED (hardware). Not built by `release.yml` and unsupported for the Apple Silicon-only Preview (ADR-0025); no install claim is made. |
| 6 | `x86_64-unknown-linux-gnu` tarball installed and smoke-tested on Linux | UNVERIFIED. Not built by `release.yml` and unsupported for the Apple Silicon-only Preview (ADR-0025); no install claim is made. |
| 7 | Clean-machine extension journey (install, validate, approve, doctor, inspect, upgrade/invalidate/re-approve, rollback) against the prebuilt binary; no SKIP counted as PASS | UNVERIFIED |
| 8 | Existing Claude Code/Codex hook files and local state byte-identical before and after install | PARTIAL: sandboxed `install --hooks-only` then `uninstall` with the arm64 binary `5f16390f…` (built from `5aec5a1`; production source unchanged through `4c0e794`) kept all foreign keys and restored settings.json to semantically equal JSON (not byte-compared; a `.libra-backup-*` file is written). The real `~/.codex/hooks.json` was not modified. Re-run on the published artifact. |
| 9 | Artifact manifest recorded: file, size, sha256, source SHA, workflow run URL | UNVERIFIED |
| 10 | Rollback tested: unregister and remove, state intact | PARTIAL: sandboxed `install`, `doctor`, `uninstall` with a CI-built and a locally built (`98b112a`) arm64 binary restore `settings.json` to semantically equal JSON (re-serialized, not byte-identical) and leave the state dir without `--yes`. Not exercised on a published artifact. |
| 11 | Offline hook-isolation bundle for independent Codex QA (HORO-2160): binary `5f16390f…` from `5aec5a1`, manifest `39246256…`, correction `cb2dbb44…`; 12 no-daemonize runs, QA verdict CONDITIONAL PASS (comment 28714), condition L2160-01 answered in 28715. Isolation only: no daemon-present governance, native activation or CI-produced-binary claim. | QA disposition of the corrected declaration pending. Files: local custody directory `libra-g3-bundle-5aec5a1` (owner machine, not published); full digests and path in Jira HORO-2160 comments 28699 and 28715. |
| 12 | Founder GO recorded (ADR-0025 moved to Accepted) | NOT GRANTED |

Unsupported: Windows.
