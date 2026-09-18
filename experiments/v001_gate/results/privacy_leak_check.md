# Item 11 — No secret/private data leakage (HORO-1152)

Real greps against real artifacts produced by items 1–10 above, run on
this machine. Script: [`privacy_leak_check.sh`](privacy_leak_check.sh);
raw run output: [`privacy_leak_check_run.txt`](privacy_leak_check_run.txt).

## Nonces used

| Nonce | Where it was introduced |
|---|---|
| `fake-test-credential-nonce-8f2c1a` | `credential_args` in the `GovernorHeld` `config.json` fixture (item 10) |
| `add input validation to the login handler` | default preflight prompt used throughout items 4–7 |
| `implement a new /health endpoint` | prompt used in item 5's full-task scenario and re-used against the item-7 upgrade profile for this check |
| `experiments/mvp1_validation/fixtures/rust-crate` | the fixture repo path passed as `cwd` |

## Targets and results

| Target | Credential nonce | Prompt 1 | Prompt 2 | Fixture path |
|---|---|---|---|---|
| `daemon.log` | PASS (absent) | PASS (absent) | PASS (absent) | present (expected — it's an on-disk artifact path, not secret) |
| `ledger.sqlite3` (via `strings`) | PASS (absent) | PASS (absent) | PASS (absent) | absent |
| `libra-governor doctor` / `doctor --json` actual stdout | PASS (absent) | n/a | n/a | n/a |
| `settings.json.libra-backup-*` (item 9 cycle) | PASS (absent) x2 backups | n/a | n/a | n/a |

All checks that expect the nonce to be **absent** report count=0 —
**PASS**, no leakage found in any artifact this gate produced.

One false-positive was caught and corrected during this check: the
combined `10_capability_tier_text.txt` file embeds, for human
readability, the raw `config.json` **this gate itself wrote as input**
to the `GovernorHeld` scenario — which legitimately contains the
credential nonce, since it's the fixture, not `libra-governor`'s output.
A first grep pass against that whole file reported a false "leak"; the
check was corrected to isolate only the actual `libra-governor doctor`
/ `doctor --json` **stdout** sections (everything after the `$
libra-governor doctor` command marker) before re-grepping — that
isolated output contains no nonce. This is documented in
`privacy_leak_check.sh` and its run output rather than silently fixed,
per the no-fabrication invariant.

## Credential-command execution — scope note

`crates/cli/src/doctor_cmd.rs`'s own module docstring states doctor
never reads `gateway.token`'s contents, runs a `credential_command`, or
echoes anything from `config.json` beyond presence/validity — read
directly, not assumed. This gate did not separately stand up a live
gateway proxy against a loopback fake upstream to exercise
`CredentialCommand::resolve()` end-to-end (out of scope for an
install/doctor/lifecycle gate); that resolution path already has its
own real unit-test coverage in `crates/gateway/src/credential.rs`
(exercised by `cargo test --workspace`, see deliverable 5). What this
gate verifies directly is narrower and real: the fake credential nonce
embedded in this gate's own `config.json` fixtures never appears in
`daemon.log`, the ledger, `doctor`'s actual output, or any
`settings.json` backup produced during these runs.

## Release artifact scan

`README.md`'s own "Install" section states: "This repository's CI
(`.github/workflows/ci.yml`) does not publish a release binary or
artifact anywhere." Confirmed by reading `.github/workflows/ci.yml`
directly — no `upload-artifact`, packaging, or tarball step exists.
There is no release artifact to scan for v0.0.1; this is disclosed
rather than skipped silently.

## Verdict

**PASS** on every check performed. No secret/private test data
(credential nonce, prompt content) leaked into any log, ledger, or
`doctor` output produced during this gate.
