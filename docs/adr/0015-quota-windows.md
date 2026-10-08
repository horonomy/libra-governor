# ADR-0015: Versioned quota-window contract

> **Numbering note**: this local number, `0015`, is specific to this
> repo's own `docs/adr/NNNN-*.md` sequence. See
> `0012-policy-replay-and-decision-regret.md` for the precedent on why
> this disambiguation note exists (an unrelated cross-repo ADR series
> happens to reuse small numbers too).

- **Status**: Accepted
- **Ticket**: HORO-1762 (v0.0.4 campaign, epic HORO-1749)
- **Depends on**: v0.0.3's `EconomicEvent`/`ResourceAmount`/`Reservation`
  contracts (unchanged by this ADR)

## Decision

Added `crates/domain/src/quota_window/` — a pure, versioned domain
contract for simultaneously independent quota windows (fixed-aligned
reset, rolling/sliding interval, continuously-refilling bucket, and
opaque provider-declared gauge), each evaluated deterministically from a
shared evidence log (usage, outstanding holds, provider snapshots). See
the module's own rustdoc for the full type-by-type design.

## Why jiff, not `time`

`time` 0.3 (the crate's existing datetime dependency) has no IANA
timezone database support. A fixed-aligned window's reset boundary is a
wall-clock instant in an explicit timezone, not a fixed UTC offset — correctly
handling DST requires stepping calendar dates/times in that zone, which
needs a real tzdb.

[jiff](https://docs.rs/jiff) was added for this. Two choices protect
determinism:

1. **Only the bundled tzdb is ever consulted** — via
   `jiff::tz::TimeZoneDatabase::bundled()`, held in a `OnceLock`. Never
   `jiff::tz::TimeZone::get` or the global `jiff::tz::db()`: if any
   future dependency enables jiff's default features, Cargo unifies
   features across the dependency graph and the global lookup would
   silently start reading the host's `/usr/share/zoneinfo` — breaking
   the guarantee that the same window definition evaluates identically
   on every machine regardless of what system tzdata (if any) is
   installed there.
2. **jiff stays private to the module** (`quota_window::calendar` is the
   only file that touches it). The public API and wire format use
   `time::OffsetDateTime` throughout; conversion happens at that one
   module boundary.

The tzdb version is pinned by `Cargo.lock`. Bumping jiff can legitimately
move a future DST boundary if a country changes its own rules — that is
an upstream data update, not a bug in this contract, and should be
called out explicitly in the PR that bumps it.

## Versioning

`QuotaWindow` and `ProviderSnapshot` each carry a `schema_version` string
tag (`"quota-window-v1"`, following this crate's existing
`policy-v1`/`reservation-v1` convention — a string, not an integer,
because readers only ever test it for equality). Reading happens in two
stages (`decode_quota_window`/`decode_provider_snapshot`): an unrecognized
tag is kept unparsed as `DecodedQuotaWindow::Unsupported`/
`DecodedProviderSnapshot::Unsupported` rather than guessed at, and reports
as `BlockingStatus::Indeterminate(UnsupportedSchemaVersion)`. A recognized
tag is parsed strictly — an unknown variant or a failed validation is
refused, not guessed, matching this crate's existing
`unrecognized_*_is_refused_not_guessed` discipline. A future v2 would add
an explicit `migrate_v1_to_v2` function; it would never reinterpret v1
bytes as v2.

## What this ADR does not do

- Does not persist anything — storage is a later story.
- Does not ingest or reconcile provider snapshots against Libra-observed
  usage — a later story.
- Does not implement unit conversion between `QuotaUnit` variants —
  deliberately out of scope; a dollar figure, a token count, a request
  count, and a percentage of an undisclosed base are different things,
  and converting between them needs an explicit, separately authored and
  separately versioned policy that does not exist yet.
- Does not decide what admission does with a `BlockingStatus::Indeterminate`
  result (fail-open vs. fail-closed) — a later admission/pacing story's
  product decision.
- Does not touch hierarchical custody accounting
  (`resource_account.rs`'s `ensure_child_account`/`grant_sublease`) — out
  of scope per HORO-1694 (research/latent until a genuine multi-level
  product requirement exists). `QuotaSubject::SharedPool` covers the one
  concrete flat shared-pool need this campaign actually has, with no
  membership/hierarchy modeled.

## Evidence trail

- `crates/domain/src/quota_window/` — implementation, 41 passing fixture
  tests covering DST correctness (spring-forward/fall-back/gap/fold
  across three zones), independent-window non-interference (an hourly
  reset does not affect a concurrent rolling-6h window; a weekly reset
  does not affect a concurrent daily window), gauge staleness/undisclosed
  semantics, unit isolation, hold/settlement separation, refill-bucket
  arithmetic, determinism across evidence ordering, millisecond
  normalization, schema versioning, and backward compatibility with
  `EconomicEvent`'s existing wire format.
- HORO-1762 (this ticket), HORO-1763/1764/1765/1767/1768 (unblocked by
  this ticket).
