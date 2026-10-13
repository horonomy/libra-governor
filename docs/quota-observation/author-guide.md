# Quota-observation author guide (HORO-1856)

See [`README.md`](README.md)'s status banner first: experimental 0.x
preview, observation-only, depends on unmerged PRs #135/#137, no CLI
exists for this protocol yet.

This guide documents the one-shot stdio runtime contract and
`describe`/`snapshot` wire operations exactly as
`crates/quota-observation` (PR #135) implements them — field names and
rules below are taken directly from `crates/quota-observation/src/runner.rs`
and `crates/quota-observation/src/wire.rs`, not from the original design
doc's prose, so where the two differ this guide follows the code.

## Runtime contract (`crates/quota-observation/src/runner.rs`)

The host invokes your extension's executable as a one-shot subprocess
with:

- **Environment cleared entirely**, then set to exactly `LANG=C.UTF-8`
  and `LC_ALL=C.UTF-8`. No `PATH`, no `HOME`, nothing else — not even
  an environment variable your own process happened to have. This
  means:
  - **Use absolute paths for every external command you invoke**,
    including your own shebang interpreter (`#!/bin/sh`, not
    `#!/usr/bin/env sh` — the latter needs `PATH` to resolve `env`
    itself).
  - You cannot read `$HOME`-relative config or credentials. See
    [`trust-and-lifecycle.md`](trust-and-lifecycle.md)'s E1 section for
    how a real extension should get its own credentials instead.
- **Working directory fixed to `/`.** Never assume you run from any
  particular directory.
- **stderr discarded** (`Stdio::null()`). Nothing downstream reads it —
  do not rely on stderr output for anything, including debugging in
  production.
- **Exactly one request JSON frame** written to your stdin, which is
  then closed. **Exactly one response JSON frame** must be written to
  your stdout — optionally followed by one trailing `\n`, nothing
  else — before your process exits.
- **Exit code must be 0.** A non-zero or signalled exit is treated as a
  failure regardless of whether you wrote a valid response frame.
- **Request frame capped at 16 KiB; response frame capped at 64 KiB**
  (`runner::MAX_REQUEST_BYTES`/`MAX_REQUEST_BYTES`/`MAX_RESPONSE_BYTES`).
  An over-cap response triggers an immediate kill, not a graceful
  rejection.
- **`timeout_ms` must be `100..=10000`** (`registration::TIMEOUT_MS_RANGE`
  enforced by `Registration::validate`). This bounds the whole
  operation — process start, your stdin read, your processing time, the
  stdout write, and your exit — together, not any one phase alone.
- **On timeout (or host-side cancellation), the host kills your entire
  owned process group**, not just your direct process. If you
  background a child process (`&`) and exit, that child is still in
  your process group and still gets killed.
  - **Known limitation, not a guarantee**: a descendant that calls
    `setsid()` (or otherwise detaches itself from your process group)
    **escapes the group kill entirely**. The host has no way to reach
    it — `run_one_shot`'s own module docs call this out explicitly, and
    a regression test in `crates/quota-observation/src/runner.rs`
    (`escaped_setsid_grandchild_cannot_block_the_return_past_deadline_plus_grace`)
    exists specifically to prove the host's own deadline still holds
    *despite* this escape, not to claim the escaped process gets
    killed. **Do not background a detaching child and assume it will
    be cleaned up** — it will not be.
  - A second, separate limitation (not about `setsid`): on the
    **success path** (your process exits 0 on its own), the host does
    not kill your process group at all before reaping you — only
    timeout/cancel paths do. A backgrounded ordinary child that is
    still running when you exit 0 can outlive the host's `invoke()`
    call returning. This is a documented, not-yet-fixed limitation in
    `runner.rs` ("success-path stragglers survive") — write your
    extension so it does not background anything that must not outlive
    it, rather than relying on the host to clean up after you.

## Operations

### `describe` (bootstrap)

Request (`BootstrapRequest`, `src/wire.rs`): `bootstrap_version`,
`request_id`, `operation: "describe"`, `supported_versions`.

Response (`BootstrapResponse`): `bootstrap_version`, `request_id`, then
exactly one of:
- `result` (`BootstrapResult`): `extension_id`, `extension_version`,
  `supported_versions`, `capabilities` (a list of capability strings;
  this crate ships exactly one meaningful value today,
  `"quota.snapshot"`).
- `error` (`WireError`, shared with `snapshot` — see below).

Every struct is `#[serde(deny_unknown_fields)]`: an extra field is a
hard parse failure, not a warning.

### `snapshot`

Request (`SnapshotRequest`): `protocol_version`, `request_id`,
`operation: "snapshot"`, `context_selector`, `constraint_ids` (must be
non-empty), `max_age_seconds`, `budget_ms` (must be `<= 10000`).

Response (`SnapshotResponse`): `protocol_version`, `request_id`,
`extension_id`, `extension_version`, then exactly one of `result`
(`SnapshotResult`) or `error` (`WireError`).

`SnapshotResult` fields:
- `context` (`ObservedContext`): `selector`, `handle`, `principal`,
  `epoch`. The host checks `selector`/`handle`/`principal` against its
  own registration and rejects the **whole response** on any mismatch
  (`freshness::verify_context_binding`) — `epoch` has no registered
  counterpart and is not checked.
- `authority`: also checked against the registration's own, whole-response.
- `context_valid_until`: optional; strict expiry (`now >=
  context_valid_until` is already expired, no tolerance).
- `coverage`: `"complete"` (requires `missing` empty and `constraints`
  non-empty) or `"partial"` (requires `missing` non-empty). Every id in
  the request's `constraint_ids` must appear in exactly one of
  `constraints`/`missing` — not both, not neither
  (`SnapshotResult::validate_wire_coverage`).
- `constraints[]` (`Constraint`): `id`, `scope`, `metric`, `unit`,
  `window`, `reading`, `observed_at`, `valid_until` (optional),
  `upstream_enforcement`, `account_fingerprint`.
- `missing[]` (`MissingConstraint`): `id`, `reason` — one of
  `permission_denied`, `unavailable`, `timeout`, `rate_limited`,
  `unsupported_metric`, `unsupported_unit`, `inconsistent_reading`,
  `source_time_unknown`, `account_fingerprint_mismatch`.

#### `scope` (`Scope`, tagged by `kind`)

`principal`, `account`, `team`, `organization`, `project`,
`shared_pool`, `provider` — each carries a single `handle: String`.

#### `metric` (`Metric`)

`total_tokens`, `input_tokens`, `output_tokens`, `requests`, `spend`,
`credit_usage`, `utilization`.

#### `unit` (`Unit`, tagged by `kind`)

`tokens`, `requests`, `usd_cents`, `percent_basis_points`, or
`opaque_credit` (which additionally carries a required `namespace`
string — every other variant must *not* carry `namespace`, even as an
explicit JSON `null`, or the parse fails).

#### `window` (`Window`, tagged by `kind`)

Exactly one variant today: `opaque`, with an optional
`declared_reset_at` RFC 3339 timestamp.

#### `reading` (`Reading`, tagged by `kind`)

- `used { value, limit }`
- `remaining { value, limit }`
- `utilization { used_basis_points }`
- `undisclosed` (no fields)

Every field not owned by the matched variant is rejected, including a
present-but-`null` value — see `wire.rs`'s own module comment for why
this is a manual (not derived) `Deserialize` impl.

#### `Quantity` — canonical decimal string

Every numeric reading value (`value`, `limit`, `used_basis_points`) is
a [`Quantity`]: a JSON **string**, not a number, matching `0` or
`[1-9][0-9]*` exactly — no sign, no leading zero, no exponent, no
fraction, bounded by `i64::MAX`. This is the crate's canonical,
wire-stable representation; it is never reformatted or re-parsed
through the numeric value when re-serialized.

#### `upstream_enforcement` (`UpstreamEnforcement`)

`hard`, `soft`, or `unknown`. The host never retypes a `soft` reading
as `hard` on its own — the extension's own declared value is the only
source of truth for this field.

### Error envelope (`WireError`, shared by both operations)

`code` (`ErrorCode`): `auth_required`, `permission_denied`,
`unavailable`, `timeout`, `incompatible_protocol`,
`unsupported_capability`, `invalid_configuration`, `rate_limited`,
`context_changed`.

`retry` (`RetryKind`): `never`, `after_user_action`, `after_delay`.
`retry_after_seconds` is **required** when `retry: after_delay` and
**forbidden** otherwise (`WireError::validate`).

## Real CLI examples (extension-side only — no Libra daemon or CLI involved)

These drive `crates/quota-observation/tests/fixtures/fictional-ext/quota-ext.sh`
— the crate's own already-validated black-box test fixture — directly,
substituting each scenario fixture from
`crates/quota-observation/tests/fixtures/scenarios/*.json` verbatim as
the canned response template. Every command and its output below was
actually run against this repository's own checked-in files, not
hand-typed.

### `describe`

```bash
cd / && printf '%s\n' '{"bootstrap_version":"libra.quota-bootstrap/1","request_id":"r-describe-demo-1","operation":"describe","supported_versions":["libra.quota-observation/0.1"]}' \
  | env -i LANG=C.UTF-8 LC_ALL=C.UTF-8 /abs/path/to/crates/quota-observation/tests/fixtures/fictional-ext/quota-ext.sh \
      ok /abs/path/to/crates/quota-observation/tests/fixtures/scenarios/describe-response.json
```

Output (`describe-response.json`'s own content, `request_id`
substituted in):

```json
{"bootstrap_version":"libra.quota-bootstrap/1","request_id":"r-describe-demo-1","result":{"extension_id":"examplecorp.quota","extension_version":"0.1.0","supported_versions":["libra.quota-observation/0.1"],"capabilities":["quota.snapshot"]}}
```

### `snapshot` — scenario A (exact `used`/`limit`, single constraint)

```bash
cd / && printf '%s\n' '{"protocol_version":"libra.quota-observation/0.1","request_id":"r-scenario-a-demo-1","operation":"snapshot","context_selector":"profile_a","constraint_ids":["tokens_month"],"max_age_seconds":300,"budget_ms":3000}' \
  | env -i LANG=C.UTF-8 LC_ALL=C.UTF-8 /abs/path/to/crates/quota-observation/tests/fixtures/fictional-ext/quota-ext.sh \
      ok /abs/path/to/crates/quota-observation/tests/fixtures/scenarios/a-response.json
```

Output (`a-response.json`'s own content):

```json
{"protocol_version":"libra.quota-observation/0.1","request_id":"r-scenario-a-demo-1","extension_id":"examplecorp.quota","extension_version":"0.1.0","result":{"context":{"selector":"profile_a","handle":"context_a","principal":"principal_a","epoch":"epoch_b"},"authority":"authority_a","coverage":"complete","constraints":[{"id":"tokens_month","scope":{"kind":"principal","handle":"principal_a"},"metric":"total_tokens","unit":{"kind":"tokens"},"window":{"kind":"opaque","declared_reset_at":"2027-02-01T00:00:00Z"},"reading":{"kind":"used","value":"600000","limit":"1000000"},"observed_at":"2027-01-18T12:00:00Z","valid_until":"2027-01-18T12:01:00Z","upstream_enforcement":"unknown","account_fingerprint":"acct-fx9k2m1p"}],"missing":[]}}
```

See `tests/fixtures/scenarios/b-response.json` (two constraints,
separate units/windows, no cross-unit conversion performed) and
`c-response.json` (a value past `2^53`, `opaque_credit` with a
namespace, `coverage: partial` with one `missing` entry) for the other
two design-v2 example scenarios, and
`tests/README.md`'s P02–P04 rows for which conformance test exercises
each.

### An error envelope

```bash
cd / && printf '%s\n' '{"protocol_version":"libra.quota-observation/0.1","request_id":"r-auth-demo-1","operation":"snapshot","context_selector":"profile_b","constraint_ids":["team_budget"],"max_age_seconds":300,"budget_ms":3000}' \
  | env -i LANG=C.UTF-8 LC_ALL=C.UTF-8 /abs/path/to/crates/quota-observation/tests/fixtures/fictional-ext/quota-ext.sh \
      ok /abs/path/to/crates/quota-observation/tests/fixtures/scenarios/auth-required-response.json
```

Output (`auth-required-response.json`'s own content):

```json
{"protocol_version":"libra.quota-observation/0.1","request_id":"r-auth-demo-1","extension_id":"examplecorp.quota","extension_version":"0.1.0","error":{"code":"auth_required","retry":"after_user_action"}}
```

### A minimal sample you can write yourself

[`examples/quota-observation-extension/`](../../examples/quota-observation-extension/)
is a from-scratch sample (not a copy of the fictional test fixture
above) implementing this same contract against one static canned
response, with a registration template showing how the identity set
(`extension_id`/`context`/each constraint's
`id`/`scope`/`metric`/`unit`/`account_fingerprint`) must agree exactly
between the extension's own response and its registration — a mismatch
on any of these re-homes that constraint into `missing`
(`account_fingerprint_mismatch`), per
[`trust-and-lifecycle.md`](trust-and-lifecycle.md).

## Version compatibility

Two protocol identifiers exist: `libra.quota-bootstrap/1` (the
`describe` envelope) and `libra.quota-observation/0.1` (the `snapshot`
envelope). **Neither `bootstrap_version` nor `protocol_version`
negotiation is implemented today, and `BootstrapResult.capabilities` is
never cross-checked against what a `snapshot` call actually requires**
(gap **G9**, `crates/quota-observation/tests/README.md`'s compat-row 7).
`0.1` is a label on this one implemented wire shape, not a
compatibility guarantee the host enforces — the host does not reject a
`snapshot` call because a prior `describe` declared a different
`supported_versions` list or omitted `"quota.snapshot"` from
`capabilities`. Do not build an extension that depends on the host
performing any such negotiation; there is none.
