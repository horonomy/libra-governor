# Preview release checklist (HORO-1857 AC6)

A checklist an owner completes **before** pushing a `v*` tag. Nothing here
authorizes a release; publication needs separate founder approval. Fill the
right-hand column with evidence (command output, run URL, digest) — leave
`UNVERIFIED` rather than guessing.

| # | Check | Result |
|---|---|---|
| 1 | `main` is at the intended commit; `Cargo.toml` workspace version equals the tag without the leading `v` (`scripts/check-release-tag.sh vX.Y.Z`) | UNVERIFIED |
| 2 | Hosted CI is green on that commit, or each unavailable check is classified `CI_UNAVAILABLE_EXTERNAL` with local-equivalent evidence | CI_PASS for `5aec5a1` (push run 39885276421: fmt, clippy, ubuntu + macOS build & test); local equivalent also run (one background-SIGINT artifact, foreground 14/14). Re-evaluate if `main` moves before the tag. |
| 3 | A `workflow_dispatch` run of `release.yml` built all three targets from that exact SHA (record run URL) | UNVERIFIED |
| 4 | `aarch64-apple-darwin` tarball: sha256 matches, `gh attestation verify` passes, `--version` prints the tag version | UNVERIFIED (earlier manual pass was on a prior SHA) |
| 5 | `x86_64-apple-darwin` tarball installed and smoke-tested on an Intel Mac | UNVERIFIED (hardware). OUT OF SCOPE for the proposed Apple Silicon-only Preview (ADR-0025); not an active blocker if the founder approves that scope. No install claim is made. |
| 6 | `x86_64-unknown-linux-gnu` tarball installed and smoke-tested on Linux | UNVERIFIED. OUT OF SCOPE for the proposed Apple Silicon-only Preview (ADR-0025); not an active blocker if the founder approves that scope. No install claim is made. |
| 7 | Clean-machine extension journey (install, validate, approve, doctor, inspect, upgrade/invalidate/re-approve, rollback) against the prebuilt binary; no SKIP counted as PASS | UNVERIFIED |
| 8 | Existing Claude Code/Codex hook files and local state byte-identical before and after install | PARTIAL: sandboxed `install --hooks-only` then `uninstall` with the arm64 binary `5f16390f…` (code-equivalent to `4c0e794`) kept all foreign keys and restored settings.json to semantically equal JSON (not byte-compared; a `.libra-backup-*` file is written). The real `~/.codex/hooks.json` was not modified. Re-run on the published artifact. |
| 9 | Artifact manifest recorded: file, size, sha256, source SHA, workflow run URL | UNVERIFIED |
| 10 | Rollback tested: unregister and remove, state intact | PARTIAL: same sandbox run; `uninstall` without `--yes` left the state dir in place. Not exercised on the published artifact. |
| 11 | Offline hook-isolation bundle for independent Codex QA (HORO-2160): binary `5f16390f…` from `5aec5a1`, manifest `39246256…`, correction `cb2dbb44…`; 12 no-daemonize runs, QA verdict CONDITIONAL PASS (comment 28714), condition L2160-01 answered in 28715. Isolation only: no daemon-present governance, native activation or CI-produced-binary claim. | QA disposition of the corrected declaration pending. Files: `/Users/bryant/Bryant-Developments/horonomy/libra-g3-bundle-5aec5a1/` (local custody, not published); full digests in Jira HORO-2160 comments 28699 and 28715. |
| 12 | Founder GO recorded (ADR-0025 moved to Accepted) | NOT GRANTED |

Unsupported: Windows.
