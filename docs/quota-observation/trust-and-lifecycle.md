# Quota-observation trust and lifecycle (HORO-1856)

See [`README.md`](README.md)'s status banner first. Everything below
describes `crates/quota-observation` (`src/identity.rs`,
`src/registration.rs`) exactly as it exists on `main` — where a claim
below was verified against the actual code rather than assumed from the
original design prose, that's noted explicitly.

## Approval digests

Three separate SHA-256 digests, each formatted as **`sha256:<hex>`**
(never raw `shasum`/`sha256sum` output, which has no `sha256:` prefix
and a different field order than this crate's canonical JSON). **Do
not hand-compute one of these with `jq -S` + `shasum`** to verify or
reproduce a value: `manifest_digest` hashes the *re-serialized*
`Registration` struct — serde's own field defaults, any `None` field
omitted, and the exact compact formatting `serde_json` produces — not
the registration file's raw bytes, and not merely its keys in sorted
order (`jq -S` does sort recursively, but that alone doesn't reproduce
serde's own serialization). This crate deliberately keeps the three
digest functions `pub(crate)` (not exported) so there is only one
implementation of this to ever diverge from — see
`tests/common/mod.rs::approve`'s own doc comment for the fuller
reasoning on why external cross-checking isn't supported.

| Digest | Binds |
|---|---|
| `executable_sha256` | The executable file's raw bytes, hashed directly (`digest_file`). |
| `manifest_sha256` | Canonical JSON of the **entire registration with the `approval` block removed** (`manifest_digest`) — the approval record cannot bind over itself. |
| `launch_tuple_sha256` | The absolute executable path + its own measured digest, the ordered `argv`, the sorted fixed environment map (`LANG`/`LC_ALL` only — no other key has an identity role), and the declared `runtime_files` in their **declared order** with their measured digests (`launch_tuple_digest`). |

**Interpreted extensions** (anything that isn't a single static
native/script binary with no separate runtime dependencies) **must list
their interpreter and any modules they load in `runtime_files`** to
have those files measured at all — the executable digest only covers
the one file named in `transport.executable`. A pure POSIX `/bin/sh`
script with no `source`d modules (like
[`examples/quota-observation-extension/quota-ext.sh`](../../examples/quota-observation-extension/quota-ext.sh))
has nothing further to declare, so its `runtime_files` is empty; a
Python extension that imports a local helper module, for example, would
need that module's path listed so tampering with it is caught.

**Digesting stops at 64 MiB per file**
(`identity::MAX_DIGESTED_FILE_BYTES`) — a file larger than that fails
closed (`IdentityError::FileTooLarge`), it is not silently truncated
and hashed partially.

**Every digest is re-measured from disk on every single invocation**
(`verify_identity`, called unconditionally by `runner::invoke` before
anything runs) — never cached, never trusted from a prior successful
call. `invoke` is the crate's one public entry point specifically so
this can't be bypassed by a caller that skips re-measurement.

### Approval is a drift detector, not authentication

Approval proves "this executable/manifest/launch-tuple is byte-for-byte
what was approved," nothing more. **It is not an authentication
mechanism** — nothing here verifies *who* wrote the registration file
or *who* ran the approval step. The actual trust root is **write access
to the registration file and its directory**: whoever can write a
`*.json` file into the registrations directory (and flip
`approval.approved: true` with digests that match real files on disk)
can get that registration loaded and invoked. Approval's value is
catching an unnoticed *change* after the fact (a file edit, an argv
change, a swapped runtime file), not establishing identity in the first
place.

`approval.endpoint_owner` and `approval.consent` (`consented_by`,
`recorded_at_reference`) are **required, present fields with no digest
binding them at all** — they live inside the `approval` block, which
`manifest_digest` explicitly excludes, and `launch_tuple_digest` never
reads them either. `Registration::validate` requires `endpoint_owner`
to be non-empty (`RegistrationError::EmptyEndpointOwner`), but
`consent`'s two string fields are only required to be *present* in the
JSON — **not** checked for emptiness by anything in this crate today.
Editing either after approval changes no digest and is not detected as
drift.

## E1 — private extensions own their own credentials

The host never gives your extension a credential: `runner.rs` clears
the entire environment and passes nothing but `LANG`/`LC_ALL` (see the
author-guide's runtime contract). If your extension needs to
authenticate to a real provider, **get that credential some other way
your own code controls** — and specifically:

- **Never put a credential in `args`.** `transport.args` is bound into
  the manifest digest (visible to anyone who can read the registration
  file) *and* is visible in a process listing (`ps`) for as long as
  your process runs.
- **Never list a credential file in `runtime_files`.** Doing so binds
  that file's hash into the launch tuple digest — the moment you rotate
  the credential (which you should do routinely), the file's hash
  changes and every future invocation fails with
  `IdentityError::RuntimeFileDigestMismatch` until someone re-approves.
  `runtime_files` exists to catch *tampering* with files your code
  depends on, not to version a secret.

## E2 — `LiveEndpointAuthorization` is not wired in (by design, today)

`crates/quota-source`'s `LiveEndpointAuthorization` concept is **not
connected to this crate's observation path in any way** —
`crates/quota-observation` has **no dependency** on `quota-source` at
all (see `lib.rs`'s own module docs for why the two crates' wire types
don't cleanly fit each other). E2 is a recorded design intent for a
later slice, not a behavior this crate enforces today. Do not describe
it as implemented in anything you write against this crate as it
exists now.

## Pool identity

`PoolId = account_fingerprint/pool_alias` — **and nothing else.**
`provider` (a display label), `registration_id` (a local alias),
`window_uuid` (a local window handle), and `binding_revision` carry
**zero identity role** — none of them may ever affect which pool a
reading belongs to (`registration.rs`'s own module docs call this out
as the exact relabeling attack this invariant closes).

Uniqueness is checked **only among enabled registrations**
(`check_pool_uniqueness` skips any registration with `enabled: false`
entirely) — two *disabled* registrations are free to declare the same
`(account_fingerprint, pool_alias)` pair; only one *enabled*
registration may claim it.

## Disable / re-enable

Set `enabled: false` to disable a registration. `enabled` is a
**top-level registration field, not part of the `approval` block** —
so it **is** covered by `manifest_sha256` (which hashes the whole
registration minus `approval`).

**On this specific point, verify against the code rather than
intuition** — the empirical behavior, confirmed by running
`runner::invoke` against a real approved registration:

1. Approve a registration while `enabled: true`. `manifest_sha256` is
   recorded for that state.
2. Flip `enabled: false` and nothing else. `invoke` now returns
   `IdentityError::Disabled` — checked *before* any digest is even
   measured, so the manifest mismatch (if there is one) is never
   reached.
3. Flip `enabled` back to `true`, with **no other field changed**.
   `invoke` succeeds — the manifest digest is recomputed over the same
   field values it was originally approved under
   (`enabled: true`, unchanged everywhere else), so it matches, and
   `verify_identity` passes without any new approval step.

So: **a plain disable-then-re-enable, with nothing else touched,
requires no re-approval** — toggling `enabled` back to exactly its
approved-time value reproduces the approved-time manifest digest
exactly, since `manifest_digest` is a pure deterministic function of
the registration's current field values. What *does* trip
`ManifestDigestMismatch` is changing **any other field** while
disabled (or re-enabling a registration whose manifest digest was never
correctly computed for *any* state to begin with — see
`shared_pool_anti_vacuity.rs::attack4_invoke_refuses_the_disabled_copy_and_flipping_enabled_in_memory_is_refused_at_the_manifest_gate`,
which demonstrates exactly that second case, not the first, and
`identity::tests::flipping_enabled_without_reapproval_trips_the_manifest_digest`
(`src/identity.rs`), which is the test that actually proves `enabled`
is covered by the digest in the first place).

## Rollback

Restore the **previous executable together with its previous
registration file** — never one without the other. Changing only the
binary (leaving the still-current registration's digests pointing at
the old approval) trips `ExecutableDigestMismatch`, by design: a
rollback is a return to a fully-approved prior state, not a
reinterpretation of the current one.

## Uninstall

1. Delete the registration file.
2. Delete the executable and any declared runtime files.

This crate caches nothing of its own between invocations — no PoolId
binding, no trust state, no digest, persists anywhere outside the
registration file itself. Deleting the registration file is sufficient
to make `load_registrations` stop seeing that pool at all; there is
nothing else to clean up.
