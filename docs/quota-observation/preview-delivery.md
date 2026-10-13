# Quota-observation preview delivery (HORO-1857)

See [`README.md`](README.md)'s status banner first: experimental 0.x
preview, observation-only, merged to `main` (HORO-1854/1855), no CLI
exists for this protocol yet. This doc covers platform support,
download/build integrity, and the clean-install procedure the smoke
test in `crates/quota-observation/tests/preview_clean_install.rs`
exercises.

## Platform matrix

| Platform | Supported |
|---|---|
| Linux (`ubuntu-latest` in CI) | Yes |
| macOS (`macos-latest` in CI) | Yes |
| Windows | **No** |

This repository's CI (`.github/workflows/ci.yml`, `build-and-test` job)
runs exactly this matrix — `os: [ubuntu-latest, macos-latest]` — no
Windows runner exists in CI at all. This is not an oversight this doc
is working around: HORO-1854's own PR description states it plainly under
"Explicitly NOT in this slice (by design)": **"no Windows support."**
The reason is structural, not just untested: process-group kill
(`runner::kill_owned_process_group`, and the `process_group(0)` call
that creates the owned group in the first place) is written under
`#[cfg(unix)]` — there is no Windows job-object equivalent implemented
anywhere in `crates/quota-observation`. Treat "Windows unsupported"
as a real constraint on any extension or install tooling built against
this preview, not a gap to quietly work around.

## Integrity — the download checksum IS the approval digest

There is **one** identity model here, reused, not a second one
invented for distribution: the same `sha256:<hex>` format
[`trust-and-lifecycle.md`](trust-and-lifecycle.md) documents for
`approval.executable_sha256` is exactly what you verify a downloaded
extension executable against before ever registering it — and the same
applies to each declared runtime file (`approval.runtime_files[].sha256`
covers that file the identical way). There is no separate "download
integrity" checksum scheme to learn: verify the download, then use that
exact same value as `executable_sha256` (or the matching
`runtime_files[].sha256` entry) when you build the registration's
`approval` block. If the two ever disagree, something changed the file
between whoever published the checksum and your copy of it — treat
that as a hard stop, not a value to silently reconcile.

## Build provenance — deliberately out of scope here

A build-provenance mechanism (for example, GitHub artifact attestations
over a released executable) is a reasonable future addition, but it is
a **separately reviewed decision**, not something this doc proposes or
implements. This doc, and HORO-1857, make **no change to
`.github/workflows/`** — adding provenance attestation would need its
own review of what it actually guarantees (e.g. which identity signed
the build) versus what the approval digest model here already
guarantees (byte-identity of a specific file), and those are different
questions worth keeping separate rather than conflating in passing.

## Clean-install procedure

This mirrors exactly what
`crates/quota-observation/tests/preview_clean_install.rs` automates as
a smoke test — see that file's own module doc comment for what it can
and cannot prove.

1. **Install the executable** (and any declared runtime files) to a
   stable, operator-chosen path. `chmod` it executable
   (`0755` is sufficient; it does not need to be writable by anything
   but the installing operator).
2. **Compute its checksum** and confirm it matches whatever the
   extension's author published (see "Integrity" above) — this is the
   value that becomes `approval.executable_sha256`.
3. **Write a registration** pointing `transport.executable` at that
   installed path, with a real, honest `endpoint_owner` and `consent`,
   and `approval.approved: false` with the three digest fields blank —
   see
   [`examples/quota-observation-extension/registration.template.json`](../../examples/quota-observation-extension/registration.template.json)
   for the exact shape.
4. **Approve it**: measure the real executable/manifest/launch-tuple
   digests from the file now on disk and write them into the
   registration's `approval` block, then set `approved: true`. **No CLI
   performs this step today** (pending CLI slice, ticket TBD) — see
   `tests/common/mod.rs::approve`'s own doc comment for why this
   repository's test suite drives this through the crate's own public
   `identity::verify_identity` function in a loop rather than through
   any command.
5. **Invoke `describe`, then `snapshot`**, through
   `crates/quota-observation`'s own public API
   (`runner::invoke` + `runner::parse_single_frame`, or the full
   reference pipeline in `tests/common/mod.rs::observe`), and confirm
   you get back a parseable response with the coverage you expect.
6. **Exercise the lifecycle**: disable it (`enabled: false` — confirm
   `IdentityError::Disabled`), remove the registration file entirely
   (confirm it no longer loads), and re-register the same pool from
   scratch (confirm it loads cleanly, with no leftover state from the
   earlier registration — this crate persists nothing of its own
   outside the registration file; see
   [`trust-and-lifecycle.md`](trust-and-lifecycle.md)'s "Uninstall"
   section).

`tests/preview_clean_install.rs` automates steps 1–6 above end to end
against the sample extension in
[`examples/quota-observation-extension/`](../../examples/quota-observation-extension/),
using the test-only self-approval helper for step 4 — not a real
operator approval flow, since none exists yet.
