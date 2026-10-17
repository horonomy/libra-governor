# ADR-0023: Metered shared pool (HORO-1915)

## Status

Accepted. Founder decision: HORO-1915 Jira comment 27743 (Option 1),
synchronized on HORO-1749 comment 27744.

## Context

HORO-1749's enterprise golden journey asks for one authoritative,
provider-denominated (USD/tokens/requests) shared quota pool that
multiple independent sessions/tasks can consume without duplication.
Reconciliation against the shipped codebase found two real, independently
tested, but production-disconnected pieces:

- `crates/quota-observation`: a real, standalone, no-rebuild-required
  extension crate that reads a provider's quota and produces snapshots.
  Published as observation-only — see that crate's own `lib.rs` and
  `docs/quota-observation/README.md`: "never makes an admission, pacing,
  or enforcement decision, and never will under this design."
- `crates/ledger/src/shared_pool.rs`: a real shared-pool ledger
  (`ensure_pool`, `reserve_shared`, `settle_shared`, `release_shared`,
  `expire_stale_shared_pool_reservations`, `ingest_pool_provider_snapshot`)
  with restart/concurrent-reservation/stale-expiry/partial-settlement
  coverage — but, before this ticket, consumed in production only by
  `crates/daemon/src/launch_gate.rs`'s opaque launch-slot pool.

The launch gate's pool cannot be the consumer for provider-denominated
spend: it counts opaque launch-slots (`OpaqueCredit("libra-launch-slot")`)
that are released or expire and are **never settled** (ADR-0020 §1).
Feeding a USD/token/request gauge reading into it would either double-count
(the ledger's `external_overhang` arithmetic only nets out *settled*
Libra usage, and a launch-slot hold is never settled) or permanently wedge
the pool on a unit mismatch.

Separately, wiring `quota-observation`'s real registration/consent/trust
pipeline directly into admission would reverse its own published
observation-only guarantee — a trust-boundary and public-compatibility
change the founder explicitly did **not** authorize in this decision (see
Jira 27743's "Explicit non-authorization" section).

## Decision

Build a new, separately-namespaced pool kind — a **metered pool**
(`metered:<owner>/<alias>`, units `UsdCents | Tokens | Requests`) — on the
existing `quota_pools`/`shared_pool_reservations`/
`shared_pool_provider_snapshots` tables. No new tables, no migration, no
second ledger engine.

### Namespace and unit disjointness

`metered:` and `launch:` differ at the first byte
(`libra_governor_domain::MeteredPoolKey`/`METERED_POOL_PREFIX`), so the two
pool kinds can never collide at the storage layer. On top of that:

- a metered pool only accepts `UsdCents`/`Tokens`/`Requests`
  (`is_metered_unit`) — never the launch gate's opaque credit unit;
- a metered reserve refuses outright if `owner == "libra-launcher"` (the
  launch gate's fixed principal);
- the launch gate's own `handle_release`/`handle_renew` already refuse
  any pool id outside `launch:`, and the metered handlers (`handle_settle`/
  `handle_release`) refuse any reservation whose pool id does not parse as
  a `MeteredPoolKey` with a matching owner/session.

### Fail-closed freshness

A metered reserve is gated on the pool's most recent provider snapshot via
`libra_governor_domain::metered_snapshot_limit`: missing, undisclosed,
without a `valid_until`, stale, or without a declared `limit` all refuse
the reserve outright (`SnapshotUnusable`) rather than treating "unknown"
as "unlimited". When usable, the pool's *effective* capacity is
`min(stored capacity, declared snapshot limit)` — a declared limit never
raises the pool's stored capacity, only ever lowers the effective ceiling.
This mirrors the existing `external_overhang` discipline (`shared_pool.rs`)
that a stale/missing/undisclosed snapshot can never collapse into "not
blocking", applied here to the limit side instead of the overhang side.

An optional per-session cap (`session_cap`) layers on top as a cooperative
self-limit, counting a session's own active+settled total — never a
required allocation, and never enforced against a hostile caller lying
about its own session id (see "Known limitations" below).

### No production feed in this phase

Deliberately, nothing in `crates/daemon/src/metered_pool.rs` or
`crates/cli/src/metered_pool_cmd.rs` ever calls `ensure_pool` or
`ingest_pool_provider_snapshot` for a metered pool. Every request against
an unprovisioned pool refuses with `MeteredRefusal::NoPool`. Provisioning
and feeding a metered pool today is exclusively a test-owned synthetic
fixture (`crates/cli/tests/metered_pool_fixture`), enforced by a static
guard test (`crates/daemon/tests/metered_pool_no_production_feed.rs`).

This is the deferred trust-boundary seam: promoting a real
`quota-observation` snapshot (or any other real provider source) into this
admission path is a separate, not-yet-authorized decision. See HORO-1915
Jira comment 27743's "Explicit non-authorization" section — this ADR
implements Option 1 exactly, and does not itself authorize that
promotion. A future ticket proposing it must bring its own compatibility,
security, and migration-impact packet.

### Enforcement scope

`METERED_POOL_ENFORCEMENT_SCOPE = "libra_reserved_holds_on_this_host_only"`
— carried as data on every status report, not just a doc comment. This
pool enforces only holds *this host's* daemon reserved against its own
SQLite ledger file. It makes no claim about, and has no visibility into,
spend anywhere else (a different host, a different principal's direct API
usage, etc.) — the same honesty discipline `LAUNCH_GATE_ENFORCEMENT_SCOPE`
already established for the launch gate.

## Consequences

- The golden-journey enterprise invariant (one pool, multiple sessions,
  no duplication, no double counting, fail-closed on uncertainty) is now
  provable end to end through the real CLI/daemon path
  (`crates/cli/tests/metered_pool_golden_journey.rs`), not just in ledger
  unit tests.
- Production cannot yet use a metered pool for anything real — every
  request against a never-provisioned pool refuses `NoPool`. This is
  intentional per the founder's explicit non-authorization, not an
  oversight.
- `reserve_shared`'s existing behavior and signature are unchanged
  (`reserve_shared_core`'s metered gate is only consulted when a
  `MeteredGate` is passed, which only `reserve_shared_metered` does) — the
  launch gate needed no changes at all.

## Known limitations

- **Sessions are not authenticated.** The same-OS-user socket is the
  authority boundary (ADR-0017/0019, unchanged here). `session_cap` is a
  cooperative self-limit: it stops one session from accidentally
  exceeding its own stated budget, and the owner/session identity check on
  settle/release stops one session from mutating a different session's
  hold by mistake — but it cannot stop a hostile same-user process that
  lies about its own `session_id` on a fresh reserve.
- **One settle per hold.** There is no incremental/partial settlement
  primitive — a caller doing many small steps of work reserves per step.
- **No renew for metered holds**, unlike the launch gate's `Renew`. A
  step that outlives the fixed TTL (`DEFAULT_METERED_HOLD_TTL_SECS`,
  3600s) is reclaimed by the normal stale-expiry sweep; a late settle
  after expiry is still recorded as real spend (surfacing as overrun)
  rather than silently discarded — the same discipline
  `settle_shared`'s existing `late_after_expiry` handling already applies.
