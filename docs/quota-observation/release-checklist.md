# Preview release checklist (HORO-1857 AC6)

A checklist an owner completes **before** pushing a `v*` tag. Nothing here
authorizes a release; publication needs separate founder approval. Fill the
right-hand column with evidence (command output, run URL, digest) — leave
`UNVERIFIED` rather than guessing.

| # | Check | Result |
|---|---|---|
| 1 | `main` is at the intended commit; `Cargo.toml` workspace version equals the tag without the leading `v` (`scripts/check-release-tag.sh vX.Y.Z`) | UNVERIFIED |
| 2 | Hosted CI is green on that commit, or each unavailable check is classified `CI_UNAVAILABLE_EXTERNAL` with local-equivalent evidence | UNVERIFIED |
| 3 | A `workflow_dispatch` run of `release.yml` built all three targets from that exact SHA (record run URL) | UNVERIFIED |
| 4 | `aarch64-apple-darwin` tarball: sha256 matches, `gh attestation verify` passes, `--version` prints the tag version | UNVERIFIED (earlier manual pass was on a prior SHA) |
| 5 | `x86_64-apple-darwin` tarball installed and smoke-tested on an Intel Mac | UNVERIFIED (hardware) |
| 6 | `x86_64-unknown-linux-gnu` tarball installed and smoke-tested on Linux | UNVERIFIED |
| 7 | Clean-machine extension journey (install, validate, approve, doctor, inspect, upgrade/invalidate/re-approve, rollback) against the prebuilt binary; no SKIP counted as PASS | UNVERIFIED |
| 8 | Existing Claude Code/Codex hook files and local state byte-identical before and after install | UNVERIFIED |
| 9 | Artifact manifest recorded: file, size, sha256, source SHA, workflow run URL | UNVERIFIED |
| 10 | Rollback tested: unregister and remove, state intact | UNVERIFIED |
| 11 | Founder GO recorded (ADR-0025 moved to Accepted) | NOT GRANTED |

Unsupported: Windows.
