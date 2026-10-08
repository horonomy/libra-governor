# Quota-source fixture schema (HORO-1764)

Non-numbered doc (deliberately not an ADR) — a parallel ticket in this
same v0.0.4 campaign (HORO-1763) may also be adding an ADR, so this avoids
a numbering collision between two concurrently-developed branches. See
`crates/quota-source/src/fixture.rs`'s own module docs for the
authoritative, versioned copy of this schema; this file is a pointer for
anyone looking in `docs/` first.

## What this is

A local, non-live JSON file an operator supplies to
`libra_governor_quota_source::import_fixture`, representing what a single
provider quota-gauge reading would look like. **Never presented as live
data** — `import_fixture` forces `provenance.origin` to `FixtureImport`
regardless of what the file claims, and the crate ships no code path that
calls a real endpoint at all (see
`crates/quota-source/src/lib.rs`'s module docs on why: this build has no
documented, approved, credentialed internal proxy to call).

## Schema

```json
{
  "schema_version": "quota-window-v1",
  "window_id": "<uuid, must match the target QuotaWindow's id>",
  "observed_at": "<RFC 3339 UTC timestamp>",
  "valid_until": "<RFC 3339 UTC timestamp, optional>",
  "declared_reset_at": "<RFC 3339 UTC timestamp, optional>",
  "reading": { "state": "undisclosed" },
  "confidence": "low",
  "provenance": {
    "capability": "fixture:acme-quota-v1",
    "documented_at": "docs/quota-source-fixture.md",
    "subject": { "kind": "shared_pool", "id": "acme" },
    "trust_owner": "platform-team"
  }
}
```

`reading` may instead be
`{ "state": "used", "used": { "unit": "percent", "value": 4200 }, "limit": null }`
(or any other `QuotaUnit`, with `limit` required for every non-`percent`
unit and forbidden for `percent`) — see
`libra_governor_domain::quota_window::GaugeReading`'s own docs.

`provenance.subject` uses the same shape as
`libra_governor_domain::quota_window::QuotaSubject`:
`{ "kind": "principal", "id": "<PrincipalId>" }`,
`{ "kind": "shared_pool", "id": "<pool name>" }`, or
`{ "kind": "provider", "id": { "provider": "...", "account": null } }`.

Every struct in the fixture's wire shape is
`#[serde(deny_unknown_fields)]` — an unexpected field (a `token`, an
`authorization`, an `api_key`) fails the parse outright rather than being
silently carried along. This is the concrete enforcement for "no
tokens/credentials ever logged, printed, or embedded in any fixture".

## Validation applied on import

`import_fixture` rejects (never silently accepts):

- a `window_id` that does not match the target `QuotaWindow`'s own id,
- a target window that is not an `OpaqueProviderSnapshot` window,
- `provenance.subject` not matching the target window's own
  `QuotaScope::subject` (acceptance criterion 5: incorrect principal
  attribution),
- `observed_at` in the future relative to the evaluation instant,
- `valid_until` or `declared_reset_at` at or before `observed_at`
  (acceptance criterion 5: a bogus reset timestamp).

A reading whose own age (relative to `now`) exceeds the target window's
`max_staleness_secs` is accepted as data but surfaced as `Degraded`
(visible, never trusted for an admission decision) by
`libra_governor_quota_source::response::ingest_response` — the same path
a (currently nonexistent) live adapter's response would go through.
