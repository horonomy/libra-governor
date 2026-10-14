# Private extension lifecycle walkthrough (HORO-1857)

> **Scope label — read this before anything else below.** Everything in
> this document uses a `libra-governor` binary built locally with
> `cargo build --locked` (exactly what `scripts/install.sh` already
> does), against a from-scratch, independently authored sample
> extension installed in a scratch directory outside this repository.
> **This is test-owned, locally-built evidence — it is not a published
> release artifact, and it does not satisfy HORO-1857 AC1's
> prebuilt-binary-with-pinned-checksum-and-provenance requirement.**
> AC1, the "install public binary" half of AC4's smoke journey, and
> AC6's publish-gated items remain open, pending a separate
> release-pipeline decision — see the "What remains open" section at
> the end.

This walkthrough provides real, executed evidence for:

- **AC2** — a private extension is installable, versioned, and
  upgradeable without rebuilding Libra.
- **AC3** — explicit registration/approval, identity inspection, and
  drift detection requiring reapproval.
- **AC4 (partial)** — install external package, validate, review/approve
  binding, static doctor, explicit inspect, upgrade/invalidate/reapprove,
  rollback/unregister — everything except the prebuilt-public-binary
  install step, which is out of scope here (no release pipeline exists).
- **AC5** — existing Claude Code/Codex hooks, host-adapter registry,
  local state, and unrelated config are proven untouched.

Every command and its output below was actually run in this session
against a real `libra-governor` binary and a real, from-scratch sample
extension — nothing here is reconstructed from memory or templated from
the existing docs.

## Environment

- Repository commit under test: this branch
  (`v0.0.4/HORO-1857/private_extension_lifecycle`), fast-forward merged
  with `origin/v0.0.4/HORO-1857/inspect_render_fix` (commit `30f2436`,
  the `inspect`-output human-readable renderer fix) before anything
  below was run, so `inspect`'s output reflects the fixed renderer.
- Binary: `cargo build --locked -p libra-governor-cli --bin libra-governor`
  built into an isolated `CARGO_TARGET_DIR` (this machine's shared
  target dir is contended by other concurrent sessions — see
  `~/CLAUDE.md`'s disk-pressure incident notes).
- Sample extension project: a **separate directory tree outside this
  repository**, under this agent session's own scratch space (an
  ephemeral per-session temp directory, not a path meant to persist —
  the absolute paths below are quoted verbatim from the real run, not
  sanitized, since they are the actual evidence):
  `.../scratchpad/acme-quota-connector/`. This simulates a genuinely
  separate third-party operator's own extension project: a Python
  script written from scratch against
  [`author-guide.md`](author-guide.md)'s runtime contract, **not** a
  copy of `examples/quota-observation-extension/quota-ext.sh` or the
  crate's own test fixture.
- State directory: `LIBRA_GOVERNOR_STATE_DIR` pointed at
  `.../scratchpad/acme-quota-connector/state` for every command below —
  entirely outside both this repository and this machine's real Libra
  Governor state directory. Nothing below ever touched the real state
  dir or anything in this repository.

## 0. Pre-walkthrough validation baseline (AC5, "before")

```
$ cargo fmt --all -- --check
(no output — clean)

$ cargo clippy --workspace --all-targets -- -D warnings
cargo clippy: No issues found

$ cargo test --workspace --no-fail-fast
cargo test: 1973 passed, 43 ignored, 1886 filtered out (115 suites, 145.44s)
```

All three clean before the walkthrough starts. The workspace includes
`integrations/claude-code` and `integrations/codex` (the hooks this AC
cares about) and `crates/daemon` (host-adapter registry, local state) —
their test suites are part of this one `--workspace` run.

## 1. v1 of the extension — a minimal one-shot stdio script

Written from scratch (`quota-connector.py`), implementing `describe` and
`snapshot` against one static canned response each, following the exact
runtime contract in author-guide.md: shebang at an absolute interpreter
path (`/opt/homebrew/bin/python3` — no `/usr/bin/env`, since there is no
`PATH` at invocation time), no reliance on `$HOME` or any other ambient
environment variable, no output on stderr, exactly one JSON response
frame on stdout, exit 0. Every JSON object emitted contains only fields
the corresponding wire struct in `crates/quota-observation/src/wire.rs`
actually owns (`deny_unknown_fields`-safe) — verified directly against
the struct definitions before writing a single response field.

Self-test (direct invocation, cleared environment, cwd `/`, **no Libra
CLI involved yet** — this is the author proving their own contract
compliance before ever registering):

```
$ cd / && printf '%s\n' '{"bootstrap_version":"libra.quota-bootstrap/1","request_id":"r-test-1","operation":"describe","supported_versions":["libra.quota-observation/0.1"]}' \
  | env -i LANG=C.UTF-8 LC_ALL=C.UTF-8 /path/to/acme-quota-connector/quota-connector.py
{"bootstrap_version": "libra.quota-bootstrap/1", "request_id": "r-test-1", "result": {"extension_id": "acmecorp.quota-connector", "extension_version": "1.0.0", "supported_versions": ["libra.quota-observation/0.1"], "capabilities": ["quota.snapshot"]}}
```

```
$ cd / && printf '%s\n' '{"protocol_version":"libra.quota-observation/0.1","request_id":"r-test-2","operation":"snapshot","context_selector":"acme-session","constraint_ids":["requests_today"],"max_age_seconds":300,"budget_ms":3000}' \
  | env -i LANG=C.UTF-8 LC_ALL=C.UTF-8 /path/to/acme-quota-connector/quota-connector.py
{"protocol_version": "libra.quota-observation/0.1", "request_id": "r-test-2", "extension_id": "acmecorp.quota-connector", "extension_version": "1.0.0", "result": {"context": {"selector": "acme-session", "handle": "acme-handle", "principal": "acme-principal", "epoch": "epoch-v1-canned-batch-001"}, "authority": "acme-authority", "coverage": "complete", "constraints": [{"id": "requests_today", "scope": {"kind": "account", "handle": "acme-account"}, "metric": "requests", "unit": {"kind": "requests"}, "window": {"kind": "opaque", "declared_reset_at": "2027-03-01T00:00:00Z"}, "reading": {"kind": "remaining", "value": "4200", "limit": "5000"}, "observed_at": "2027-02-14T09:00:00Z", "valid_until": "2027-02-14T09:05:00Z", "upstream_enforcement": "soft", "account_fingerprint": "fp-acme-0007"}], "missing": []}}
```

Both operations exit 0 with exactly one well-formed response frame.

## 2. Install v1

```
$ cp acme-quota-connector/quota-connector.py acme-quota-connector/install/quota-connector.py
$ chmod 755 acme-quota-connector/install/quota-connector.py
$ shasum -a 256 acme-quota-connector/install/quota-connector.py
2d42667cbd834fdef1ec0e8b88ac1abf0aa2c0c3dba208e84523b31cc68995ce  acme-quota-connector/install/quota-connector.py
```

## 3. register — and proving `register` never trusts a self-declared approval

A tampered copy of the registration, with `approval.approved: true` and
fabricated digests pre-set (simulating a malicious or careless hand-edit
of a registration file before it's ever registered), is registered
first:

```
$ libra-governor quota-observation register registration-tampered.json
registered: wrote .../state/quota-observation/registrations/acme-quota-connector.json

$ libra-governor quota-observation doctor
acme-quota-connector — extension acmecorp.quota-connector 1.0.0 — enabled=true approved=false
```

`doctor` shows `approved=false` despite the candidate file claiming
`approved: true` — `register` force-clears the approval block on every
write, exactly as `trust-and-lifecycle.md` and the `register` doc
comment in `mod.rs` describe. This is the real security property PR
#142 added (commit `46f6de0`, "Never trust a registration candidate's
self-declared approval").

```
$ libra-governor quota-observation uninstall acme-quota-connector
uninstalled "acme-quota-connector" (removed .../registrations/acme-quota-connector.json)

$ libra-governor quota-observation doctor
no quota-observation registrations found in .../registrations
```

Now the real (clean, `approval.approved: false`) registration is
registered for the rest of the walkthrough:

```
$ libra-governor quota-observation register registration.json
registered: wrote .../state/quota-observation/registrations/acme-quota-connector.json
```

## 4. approve — measuring real digests, not trusting self-declared values

```
$ libra-governor quota-observation doctor
acme-quota-connector — extension acmecorp.quota-connector 1.0.0 — enabled=true approved=false

$ libra-governor quota-observation approve acme-quota-connector
approved "acme-quota-connector" (extension "acmecorp.quota-connector" "1.0.0")

$ libra-governor quota-observation doctor
acme-quota-connector — extension acmecorp.quota-connector 1.0.0 — enabled=true approved=true
```

## 5. doctor (passive) and doctor --probe (describe-only round trip)

```
$ libra-governor quota-observation doctor --probe
acme-quota-connector — extension acmecorp.quota-connector 1.0.0 — enabled=true approved=true
  --probe: extension_id=acmecorp.quota-connector extension_version=1.0.0 supported_versions=["libra.quota-observation/0.1"] capabilities=["quota.snapshot"] — this host's protocol version is supported
```

Passive `doctor` (no `--probe`) never starts the extension process at
all — confirmed above by its output appearing instantly and identically
whether or not the extension binary is even executable; `--probe`
performs exactly one `describe` bootstrap call, never a `snapshot`.

## 6. inspect — a real snapshot round trip, fixed renderer

```
$ libra-governor quota-observation inspect acme-quota-connector
coverage=Complete context.selector=acme-session authority=acme-authority
  constraint requests_today scope=account:acme-account metric=requests unit=requests reading=4200 remaining of 5000 upstream_enforcement=Soft observed_at=2027-02-14T09:00:00Z [stale]
```

`coverage` and `reading` render as human text (`reading=4200 remaining
of 5000`), not a `Debug`-formatted struct dump — this is the fixed
renderer from the merged `inspect_render_fix` branch. `[stale]` is a
real, not staged, artifact: the extension's canned `observed_at`
(`2027-02-14T09:00:00Z`) is in the future relative to when this was
actually run (2026-10-14), so `src/freshness.rs`'s `is_fresh` correctly
refuses to call the reading fresh — the same effect the author-guide's
own verified example documents for its sample extension.

## 7. Upgrade to v2 — without re-approving, then with

The extension's own script content is modified: `EXTENSION_VERSION`
bumped `"1.0.0"` → `"2.0.0"`, and the canned response's `epoch` field
value changed (one additional byte of the response body), with **no**
re-approval yet:

```
$ diff v1-backup-quota-connector.py install/quota-connector.py
31,32c31,32
< EXTENSION_VERSION = "1.0.0"
< RESPONSE_BODY_TAG = "v1-canned-batch-001"
---
> EXTENSION_VERSION = "2.0.0"
> RESPONSE_BODY_TAG = "v2-canned-batch-002"

$ shasum -a 256 v1-backup-quota-connector.py install/quota-connector.py
2d42667cbd834fdef1ec0e8b88ac1abf0aa2c0c3dba208e84523b31cc68995ce  v1-backup-quota-connector.py
3d035eb26a0bf8aa10b1b9fd1d7659a00bd2195b5f0157e29883bdff74903a4e  install/quota-connector.py
```

```
$ libra-governor quota-observation doctor --probe
acme-quota-connector — extension acmecorp.quota-connector 1.0.0 — enabled=true approved=true
  --probe failed: identity verification failed: executable digest mismatch: approval expects sha256:2d42667cbd834fdef1ec0e8b88ac1abf0aa2c0c3dba208e84523b31cc68995ce, measured sha256:3d035eb26a0bf8aa10b1b9fd1d7659a00bd2195b5f0157e29883bdff74903a4e

$ libra-governor quota-observation inspect acme-quota-connector
libra-governor quota-observation inspect: identity verification failed: executable digest mismatch: approval expects sha256:2d42667cbd834fdef1ec0e8b88ac1abf0aa2c0c3dba208e84523b31cc68995ce, measured sha256:3d035eb26a0bf8aa10b1b9fd1d7659a00bd2195b5f0157e29883bdff74903a4e
(exit code 1)
```

**This is the AC3 "any code drift requiring reapproval is detected"
property, proven, not asserted**: both the passive-escalation `--probe`
path and the live `inspect` path independently refuse to run against
the upgraded binary until it is re-approved, each reporting the exact
expected-vs-measured SHA-256 pair. Note `--probe` *lists* the
registration (it still shows `approved=true` from the stale on-disk
state) but reports the live-call failure separately — it never silently
treats a failed probe as a pass.

The registration's own `extension_version` field is updated to `"2.0.0"`
(the realistic operator action when upgrading — the manifest should
describe what's actually installed) and `approve` is run again:

```
$ libra-governor quota-observation approve acme-quota-connector
approved "acme-quota-connector" (extension "acmecorp.quota-connector" "2.0.0")

$ libra-governor quota-observation doctor --probe
acme-quota-connector — extension acmecorp.quota-connector 2.0.0 — enabled=true approved=true
  --probe: extension_id=acmecorp.quota-connector extension_version=2.0.0 supported_versions=["libra.quota-observation/0.1"] capabilities=["quota.snapshot"] — this host's protocol version is supported

$ libra-governor quota-observation inspect acme-quota-connector
coverage=Complete context.selector=acme-session authority=acme-authority
  constraint requests_today scope=account:acme-account metric=requests unit=requests reading=4200 remaining of 5000 upstream_enforcement=Soft observed_at=2027-02-14T09:00:00Z [stale]
```

**This is AC2's "upgradeable without rebuilding Libra" property, proven
end to end**: no `cargo build` of `libra-governor` happened anywhere in
this step — only the extension's own file changed, plus one `approve`
call.

## 8. Rollback — a return to a fully-approved prior state, not a reinterpretation of the current one

The v1 script bytes (kept as a backup copy before the v2 edit) are
restored verbatim, with **no** re-approval yet:

```
$ cp v1-backup-quota-connector.py install/quota-connector.py
$ shasum -a 256 install/quota-connector.py
2d42667cbd834fdef1ec0e8b88ac1abf0aa2c0c3dba208e84523b31cc68995ce  install/quota-connector.py

$ libra-governor quota-observation inspect acme-quota-connector
libra-governor quota-observation inspect: identity verification failed: executable digest mismatch: approval expects sha256:3d035eb26a0bf8aa10b1b9fd1d7659a00bd2195b5f0157e29883bdff74903a4e, measured sha256:2d42667cbd834fdef1ec0e8b88ac1abf0aa2c0c3dba208e84523b31cc68995ce
(exit code 1)
```

Rolling back the binary alone **also fails closed** — the currently
approved digests still point at v2's content, not v1's, exactly as
[`trust-and-lifecycle.md`](trust-and-lifecycle.md)'s "Rollback" section
describes ("changing only the binary ... trips `ExecutableDigestMismatch`
... a rollback is a return to a fully-approved prior state, not a
reinterpretation of the current one"). The registration's
`extension_version` is restored to `"1.0.0"` to match, and `approve` is
run again against the rolled-back state:

```
$ libra-governor quota-observation approve acme-quota-connector
approved "acme-quota-connector" (extension "acmecorp.quota-connector" "1.0.0")

$ libra-governor quota-observation inspect acme-quota-connector
coverage=Complete context.selector=acme-session authority=acme-authority
  constraint requests_today scope=account:acme-account metric=requests unit=requests reading=4200 remaining of 5000 upstream_enforcement=Soft observed_at=2027-02-14T09:00:00Z [stale]

$ libra-governor quota-observation doctor --probe
acme-quota-connector — extension acmecorp.quota-connector 1.0.0 — enabled=true approved=true
  --probe: extension_id=acmecorp.quota-connector extension_version=1.0.0 supported_versions=["libra.quota-observation/0.1"] capabilities=["quota.snapshot"] — this host's protocol version is supported
```

## 9. Uninstall

```
$ libra-governor quota-observation uninstall acme-quota-connector --note-executable
uninstalled "acme-quota-connector" (removed .../registrations/acme-quota-connector.json)
note: the extension's executable was left in place at ".../install/quota-connector.py" — delete it yourself if it is no longer needed.

$ libra-governor quota-observation doctor
no quota-observation registrations found in .../registrations
```

## 10. AC5 coexistence check

**Before** this whole walkthrough (section 0 above) and **after** it
(below), the exact same three validation commands were run against the
exact same repository commit, with no code changes in between — only
this new document was added afterward:

```
$ cargo test --workspace --no-fail-fast
cargo test: 1973 passed, 43 ignored, 1886 filtered out (115 suites, 132.93s)
```

Identical pass/ignored/filtered counts before and after (`1973` /
`43` / `1886`, `115` suites both times — only wall-clock time differs,
as expected from machine load). `cargo fmt --all -- --check` and
`cargo clippy --workspace --all-targets -- -D warnings` were re-verified
clean as well (see the PR description for the exact final run used for
the PR itself).

Beyond the test suite, `git status --porcelain` in this repository
printed **no output** (a fully clean working tree) both immediately
before step 1 and immediately after step 9 — the entire walkthrough ran
with `LIBRA_GOVERNOR_STATE_DIR` pointed at a scratch directory outside
this repository and outside this machine's real Libra Governor state
directory, so nothing in this repository, any Claude Code/Codex hook
config, the host-adapter registry, or any real local state was ever
read from or written to by any command above. The only filesystem
footprint of this entire walkthrough is the scratch
`acme-quota-connector/` tree and this one new documentation file.

## What this proves

- **AC2** — a private extension was installed, versioned (`1.0.0` →
  `2.0.0` → rolled back to `1.0.0`), and upgraded, with zero
  `libra-governor` rebuilds at any point.
- **AC3** — registration/approval happened before any execution;
  `register` never trusted the candidate's self-declared approval;
  identity (executable-digest) drift from both an upgrade and a
  rollback was detected and refused, each with a real, specific,
  expected-vs-measured digest-mismatch error, not a generic failure.
- **AC4 (partial)** — install external package, validate, review/approve
  binding, static `doctor`, explicit `inspect`, upgrade/invalidate/
  reapprove, and rollback/unregister were all exercised against actually
  built/measured artifacts (never `SKIP`).
- **AC5** — the full workspace test suite (including
  `integrations/claude-code`, `integrations/codex`, and
  `crates/daemon`'s host-adapter registry and local state coverage)
  passed identically before and after, and the repository's own working
  tree was provably untouched throughout.

## What remains open (explicitly out of scope for this document)

- **AC1** — a prebuilt, publicly-distributed Libra binary with pinned
  checksum/provenance via a release CI pipeline. **There is no release
  pipeline in this repository today** (`scripts/install.sh`'s own header
  states this plainly). Not addressed here; a separate, founder-gated
  release-pipeline decision is required first.
- **AC4's "install public binary" half of the smoke journey** — the same
  reason as AC1: there is no publicly distributed artifact to install.
  This document only exercises the *extension* half of AC4's smoke
  journey, using a locally built `libra-governor` binary honestly labeled
  as such throughout.
- **AC6** — publishing anything (sanitized evidence, artifact manifest,
  PASS/FAIL/UNVERIFIED matrix as a release checklist, operator-guide
  final publication) is gated on separate release/security approval.
  This document is itself exactly the kind of "retain sanitized local
  CI/build evidence" artifact AC6 asks to be kept pending that approval —
  it is evidence, not a publication.
