# ADR-0022: Task-budget ledger freshness (HORO-1904)

## Status

Accepted.

## Context

HORO-1886/PR #184 closed the *provider-snapshot-freshness* half of "stale
budget": `QuotaWindow::evaluate_gauge`/`GaugeState::trusted_reading`
(`crates/domain/src/quota_window/evaluate.rs`) report whether the
*upstream provider's* reported remaining-quota number is itself stale.
It deliberately left open a second, independent question: whether a
`task_budgets`/`reservations` row that nothing has refreshed recently is
itself stale — independent of whether the provider's own number is
fresh.

Codex-QA's own mutation (HORO-1886 campaign comment #27506) could only
detect this gap by reading Libra's raw SQLite ledger directly in a
bespoke QA script, because no production staleness-evaluation function
existed for this table. This ADR records the production contract that
closes that gap.

## Decision

Add one pure classifier, `libra_governor_domain::ledger_freshness`
(`crates/domain/src/reservation.rs`), that answers "has this envelope's
own ledger row been touched recently enough to trust" — a third,
independent axis alongside:

- **Reservation TTL expiry** (`LedgerStore::expire_stale_reservations`) —
  a lifecycle mutation that reclaims an `Active` reservation past its
  `expires_at`.
- **Provider quota-snapshot freshness** (`GaugeFreshness`/
  `trusted_reading`) — whether the *provider's* reported number is
  stale.

```rust
pub enum LedgerFreshness {
    Fresh { age_secs: u64 },
    Stale { age_secs: u64 },
    Unknown { reason: LedgerFreshnessUnknown },
}

pub enum LedgerFreshnessUnknown {
    NoTimestamp,
    TimestampInFuture,
}

pub fn ledger_freshness(
    last_touched_at: Option<OffsetDateTime>,
    now: OffsetDateTime,
    max_staleness_secs: u64,
) -> LedgerFreshness
```

### Where "last touched" comes from — no schema migration

Nothing in this workspace refreshes an `Active` reservation in place:
every `UPDATE reservations SET` statement is a terminal transition
(settle, release, expire), each stamping `settled_at`/`released_at`. So
for an `Active` row, `created_at` *is* its last touch — there is no
separate heartbeat to read. `Reservation::last_touched_at()` is
`max(created_at, settled_at, released_at)` per row; the envelope's own
last touch is the max of that across every reservation row plus
`task_budgets.updated_at` (which `reserve`/settle-with-restore/release/
expire all bump, but a zero-restore settle deliberately does not —
`settled_at` alone carries that case).

A `last_touched_at` column on `reservations` would only be justified if
a future ticket adds an in-place refresh/heartbeat to an `Active` row;
until then it would be a denormalized copy of values already present,
which is the pattern this repo's own migration history argues against.

`LedgerStore::budget_snapshot_with_last_touch` reads this inside the
same transaction as `budget_snapshot`'s existing reads — the same
single-instant guarantee HORO-1709 established, extended rather than
reopened.

### Threshold

The daemon passes `config.reservation_ttl_secs` (default 900s): a
reading with no ledger write in one reservation lease is stale. This is
a Level 1 choice, not a new constant — a threshold below the TTL would
mark a healthy in-flight reservation stale mid-turn. It can be made
independently configurable later (`ledger_max_staleness_secs: Option<u64>`
on `DaemonConfig`) without changing `ledger_freshness`'s own signature.

### Wire shape — additive only

`BudgetSnapshot` and `BudgetPosture` are unchanged; no existing variant's
meaning changes. `StatusResult` gains `task_budget_freshness:
Option<LedgerFreshness>` (`Some` exactly when `task_budget_amounts` is
`Some`); `ExecutionOwnerOutcome::Resolved` gains the matching
`budget_freshness`. `PROTOCOL_VERSION` moves 16 → 17: an `Option` field
would not fail an old peer's decode on its own, but silently defaulting
to `None` would misreport a genuinely stale reading as merely absent —
the version check rejects that exchange instead (same precedent as the
10 → 11 bump, HORO-1709).

### Observation only — never a mutation

Per this ticket's own binding contract: reading `ledger_freshness`, however
stale the result, never itself expires, releases, debits, or reassigns a
reservation. `crates/ledger/src/reservation.rs`'s own test
(`reading_last_touch_never_mutates_an_active_reservation`) proves this
directly — an `Active` reservation aged well past the staleness threshold
but nowhere near its own TTL classifies `Stale` while
`expire_stale_reservations` at the same instant reclaims nothing.

### Read paths touched

- `crates/daemon/src/server.rs::budget_reading` (the `Status` reply) and
  `crates/daemon/src/execution_owner.rs`'s owner `Query` route both now
  call `budget_snapshot_with_last_touch` and classify with
  `ledger_freshness`, sampling `now` *after* the ledger read (each
  `LedgerStore` opens its own connection; sampling before the read could
  make a row written between `now` and the read look like it was
  touched in the future).
- `crates/cli/src/statusline_provider.rs` qualifies the `Remaining`
  posture's label/state/reason_code only when freshness is not `Fresh`,
  and always emits the structured `ledger_freshness`/`ledger_age_seconds`
  fields. Every other posture (`Exhausted`/`Uncommitted`/
  `NotEstablished`/`Unreadable`) keeps its existing label unconditionally
  — none of them describe a live measured share to begin with.
- `crates/cli/src/statusline.rs` (the legacy grep-compatible line) is
  unaffected by design, and its own compatibility test now loops over
  every `LedgerFreshness` value to prove it.
- `crates/daemon/src/gateway_authority.rs` and every admission/finalize
  write path are **not** touched: they already fail closed through
  `ReserveOutcome::Insufficient`/`NoBudget` at write time, and adding a
  staleness gate there would be a guarantee change, not an observability
  addition — explicitly out of scope here (see below).

## Explicitly out of scope

Whether a stale ledger reading should ever block or pause admission or
gateway spend is **not decided by this ADR**. Nothing here changes
admission, reservation, settlement, release, expiry, gateway
authorization, the completion-reserve formula, or shared-budget
semantics. If that question is raised later, it is a founder decision
(a material user-visible budget guarantee change), not an extension of
this contract.

Also out of scope: a configurable threshold independent of the
reservation TTL, and staleness for `shared_pool_reservations` (which has
its own renewal path).

## Consequences

- A renderer can now tell a reader "this share is current" from "this
  share is the last one recorded", for the ledger-row half of staleness,
  the same way `GaugeFreshness` already does for the provider-snapshot
  half.
- A long-lived pre-HORO-1904 daemon must be restarted after upgrading
  (`PROTOCOL_VERSION` 16 → 17), same precedent as every earlier protocol
  bump.
- `tools/codex-host-probe` (in `horonomy/internal-docs`) can now assert a
  deliberately-aged reservation reports stale via Libra's own CLI/daemon
  call (`StatusResult.task_budget_freshness` /
  `ExecutionOwnerOutcome::Resolved.budget_freshness`), closing the gap
  that previously required reading raw SQLite directly.
