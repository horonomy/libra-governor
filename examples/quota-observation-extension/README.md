# Quota-observation sample extension (HORO-1856)

A real, runnable reference implementation of a `quota-observation`
one-shot stdio extension — the simplest possible thing that
satisfies `crates/quota-observation`'s runtime contract. See
[`docs/quota-observation/author-guide.md`](../../docs/quota-observation/author-guide.md)
for the full protocol contract this implements, and
[`docs/quota-observation/trust-and-lifecycle.md`](../../docs/quota-observation/trust-and-lifecycle.md)
for how approval/digests/lifecycle work. Mirrors the layout style of
[`examples/local-providers/`](../local-providers/) (a different
extension system — ADR-0005's loopback-HTTP business-context/policy/events
extensions, not this one).

**Status**: observation-only preview, 0.x, no CLI wiring yet — see the
status banner in `docs/quota-observation/README.md` before using this
for anything beyond reading the protocol by example.

## Files

| File | Purpose |
|---|---|
| `quota-ext.sh` | The sample extension itself: a POSIX `/bin/sh` script answering `describe` and `snapshot` against one static, hardcoded response each. |
| `registration.template.json` | A registration template with an **empty `approval` block** — approval is the operator's step, never the extension author's. Every value in it is obviously fake. |

## What this sample actually does (and does not do)

- Reads exactly one JSON request frame from stdin, dispatches on the
  request's own `"operation"` field (`describe` or `snapshot`), writes
  exactly one JSON response frame to stdout, and exits 0 — the full
  one-shot stdio contract, nothing more.
- Answers every `snapshot` request with the same static canned
  reading (`tokens_month`, `used: 600000 / limit: 1000000`,
  `account_fingerprint: fp-smoke-0001`). It does not call out to any
  real provider, read any real credential, or vary its answer based on
  anything except the request's own `operation`/`request_id`/version
  fields it echoes back.
- Uses only `/bin/sh`, `/bin/cat`, `/usr/bin/sed`, and shell builtins
  (`printf`, `case`) — no interpreter modules, no non-stdlib
  dependency. Because of that, this sample's `runtime_files` list is
  empty in its registration (see the author-guide's note on when
  `runtime_files` is required: only when the executable itself needs
  separately-measured interpreter/module files, which a pure
  `/bin/sh` script does not).

## Running it directly

```bash
chmod 755 quota-ext.sh
cd / && printf '%s\n' '{"protocol_version":"libra.quota-observation/0.1","request_id":"r-1","operation":"snapshot","context_selector":"sample-session","constraint_ids":["tokens_month"],"max_age_seconds":300,"budget_ms":3000}' \
  | env -i LANG=C.UTF-8 LC_ALL=C.UTF-8 /absolute/path/to/quota-ext.sh
```

This mirrors exactly how `crates/quota-observation/src/runner.rs`
invokes a registered extension: no `PATH`, no `HOME`, nothing beyond
`LANG`/`LC_ALL`, working directory `/`, one frame in, one frame out.

## Using it with a registration

1. Copy `quota-ext.sh` somewhere stable and `chmod 755` it.
2. Copy `registration.template.json`, fill in `transport.executable`
   with that file's absolute path, set `enabled: true` and
   `approval.approved: true` with the three digest fields still blank
   (`verify_identity` rejects as `NotApproved`/`Disabled` before it
   measures anything, so the digests can't be discovered until those
   two flags are already set this way), then measure the three real
   digests (`executable_sha256`, `manifest_sha256`,
   `launch_tuple_sha256`) from the files now on disk and write them
   in — see `docs/quota-observation/trust-and-lifecycle.md` for
   exactly what each digest binds. **No CLI command performs this
   today** — there is no `register`/`approve`/`observe` CLI surface
   anywhere in this codebase yet (pending HORO-1870); the only way to
   drive this today is through `crates/quota-observation`'s own public
   library functions, the way
   `crates/quota-observation/tests/common/mod.rs::approve` does it in
   this repository's own test suite.
3. Once approved, `crates/quota-observation::runner::invoke` can run
   this registration directly.

## What this sample is not

Not a real provider integration, not a template for handling a real
credential (see the author-guide's E1 section — if you're building a
real extension, your credential never goes in `args` or
`runtime_files`), and not evidence that any CLI wiring exists for this
protocol today.
