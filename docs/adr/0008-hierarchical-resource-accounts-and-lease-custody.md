# ADR-0008: Hierarchical resource accounts and lease custody

## Status

Accepted (HORO-1668, v0.0.3 "Agent Execution Economics" campaign).

## Context

HORO-1141 gave Libra a single-level reservation ledger: one `task_budgets`
row per task, one protected Completion Reserve, and `reservations` as the
atomic reservation ledger against that single envelope. That model is
correct for one task with one holder of capacity at a time, but the North
Star ("never start work you are unlikely to afford to finish") also has to
hold when one person runs multiple concurrent Claude Code/Codex sessions,
and a session spawns nested subagents — each of which must draw only from
capacity its parent actually has, never from a sibling's.

HORO-1666 defined `EconomicAttribution`/`EconomicEvent` — *who owns an
economic fact* — built on top of the shared `ExecutionIdentity` envelope
(HORO-1598/1599), never duplicating it. This ticket is the companion
decision for *how capacity is held and subdivided*: the resource-account
tree and lease custody model.

## Decision

### The account tree

`resource_accounts` represents: organization -> principal -> task ->
session -> agent -> nested-agent sublease. Not every level exists in every
deployment — a task account with no principal/organization ancestor is
fully valid local accounting; missing upper levels never invalidate what
is actually proven, they leave principal/organization rollups reporting
that task in an unattributed/partial bucket rather than guessing an
ancestor.

The task level is deliberately **not** given its own `granted_capacity`/
`protected_reserve` columns. `task_budgets` already owns those numbers —
copying them onto a second row would be exactly the "two copies that can
drift" problem this campaign's economic-truth work exists to eliminate.
`v_account_capacity` is the one place the task-vs-other-level case
analysis lives; every reader goes through it, never through a per-level
`if` scattered across callers. A `CHECK` constraint makes storing a second
copy at the task level unrepresentable, not just discouraged.

### Custody lineage is not provider lineage

A `ResourceAccount`'s `parent_account_id` edge is **Libra-minted**, proven
only by the act of leasing: a parent account asked for a sublease (called
`grant_sublease`), so an edge exists. This is a *different tree* from
`economic_rollup::AgentLineage` (HORO-1666's provider-proven agent
parentage, driven by `ExecutionIdentity::lineage_status`) — which is
**always an all-roots forest today**, because neither Claude Code's nor
Codex's real hook payload exposes a trustworthy `parent_agent_id`-
equivalent field (see `crates/cli/src/agent/identity.rs` module docs).

The two trees may legitimately disagree, and that is not a defect. This is
**not** a violation of HORO-1598's "never reconstruct lineage from timing
or process ancestry" rule: nothing is reconstructed here. A custody edge
exists only where a parent account was explicitly asked to fund a child —
the request itself is the proof, not an inference from when it happened or
what process created it. `provider_lineage_status` is recorded on an
account as separate, non-load-bearing truth, and is never used to
construct or imply a tree edge.

### Concurrency: `BEGIN IMMEDIATE` only, no version/CAS column

This remains a single-file SQLite database. `TransactionBehavior::Immediate`
takes the write lock at `BEGIN`, which is strictly stronger than optimistic
concurrency (a version/CAS column plus a conflict-retry loop) — a second
writer cannot observe a stale read-then-write window at all; it blocks and
then re-reads state that already reflects the first writer's commit. A
version column would add a column, a retry loop, and a new failure mode,
for no additional guarantee. This extends `reservation.rs`'s existing
module docs rather than replacing their reasoning.

Account id minting needs no CAS either: a task account's id is
deterministically its own task uuid (`AccountId::for_task`), and every
other level is minted via `INSERT ... ON CONFLICT(parent_account_id,
level, natural_key) DO NOTHING` followed by a re-read of whichever row
won — the same idiom `initialize_task_budget` already uses for
`task_budgets`.

### The invariant is checked one level deep

For account `A`: `sum(active leases against A) + sum(settled leases
against A) + protected_reserve(A) <= granted_capacity(A) + overrun slack`.
A child account's capacity is the amount of its own funding lease, which
is itself an active lease against its parent. So this one-level check
*inductively implies* the global guarantee — no descendant can hold
capacity its ancestor has not already carved out and accounted for. This
keeps the hot path an indexed `SUM`, never a recursive CTE on the write
path.

### "No broad global lock" — reconciled against what SQLite actually gives

SQLite in WAL mode has exactly one writer per database file — there *is* a
database-wide write lock, and it does serialize unrelated tasks. Engineering
around that (per-task database files, a second store) would be an enormous
complexity increase for a single-machine local tool, so this decision reads
the acceptance criterion as: no *application-level* lock, no lock held
across more than one operation, no lock whose scope is the account tree
itself, and every lease transaction is O(1) indexed work. That is the sense
in which this ticket's AC is met — stated plainly here rather than silently
claimed in the stronger sense.

### TTL clamp makes cascade expiry an emergent property, not a separate walk

A child lease's `expires_at` is clamped at grant time to never exceed the
`expires_at` of the lease that funds its own parent. One consequence,
worth recording because it is not obvious from the code alone: because
this clamp is monotonic down the tree, **every lease in a subtree rooted
at an expired funding lease independently satisfies `expires_at <= now`
too** by the time the root does. `expire_stale_reservations`'s existing
single-pass sweep (`WHERE state = 'active' AND expires_at <= ?1`) therefore
already reaches every level of a crashed subtree in one pass, without a
separate recursive walk — what it additionally does, when the reservation
it is expiring is a `subaccount_funding` lease, is mark the child account
itself `expired` (not just the lease row), so that a further
`grant_sublease`/reservation attempt against that account is refused. This
is a deliberate simplification from a design that specified an explicit
deepest-first recursive-CTE cascade: the TTL-clamp invariant makes that
recursion unnecessary, not merely optional, as long as the clamp is never
bypassed (enforced in `grant_sublease` itself, not by caller convention).

### Late settlement after expiry: record, never silently discard

A real pre-existing defect found while implementing this ticket:
`reservation.rs::settle` returned `SettleOutcome::AlreadyFinal` and wrote
nothing whenever a reservation's state was not `Active` — including
`Expired`. A reservation reclaimed by `expire_stale_reservations` (the
crash/restart recovery path) that *then* reports a real actual cost had
that cost silently discarded. Fixed as its own commit: `settle` now accepts
an `Active -> Settled` transition as before, and additionally an `Expired
-> Settled` transition, marking `settled_after_expiry = true`. Because the
expiry path already restored any `drawn_from_reserve` back onto the task's
Completion Reserve, a late settlement never re-touches `completion_reserve`
— it only records the real spend as evidence, surfacing as a visible
overrun when it exceeds the already-expired amount, exactly like an
ordinary overrun. `Released` reservations remain genuinely final: a release
means the caller itself declared the work never happened, so there is no
"late actual" to honor there.

### Local authority never claims cross-device enforcement

`AllocationAuthority::RemoteLeaseAuthority` and `EnforcementScope::RemoteAuthoritative`
are typed now, refused/unused at construction in v0.0.3 — the same
discipline `ResourceBasis::ImportedAllocationSnapshot` (HORO-1666) already
established for provenance that has no producer yet. Every account this
ticket's code creates has `enforcement_scope = LocalDevice`. The
protocol/domain seam for a future remote lease authority exists (the typed
enum variants), without building the Team SaaS/control-plane (an explicit
non-goal).

## Scope actually shipped vs. the ticket's full ambition

This PR ships: the account tree, task-level account provisioning wired into
`initialize_task_budget`, session/agent-level account provisioning
(`ensure_child_account`), sublease granting with the one-level invariant,
TTL clamp, resource-kind mismatch and idempotent-retry handling, the
emergent cascade-expiry behavior described above, and the late-settlement
fix. It does **not** ship:

- `economic_events` persistence or the ADR-0007 SQL-vs-pure-function
  cross-check test — those belong to HORO-1667 (see the HORO-1666/1668
  coordination decision recorded on both Jira tickets), which has its own
  AC requiring ledger-consumable persisted observations and will reserve
  its own migration number.
- Organization/principal account provisioning (`ensure_organization_account`/
  `ensure_principal_account` convenience constructors). The types
  (`AccountLevel::Organization`/`Principal`, `AllocationAuthority`) exist;
  no code in this ticket creates such an account, because nothing in
  v0.0.3 has an organization/principal context to attach one to (HORO-1667's
  own audit confirms the daemon never builds org/principal identity today).
  `grant_sublease` requires its parent account to have a `task_id`
  ancestor (`AccountError::ParentHasNoTaskLineage`) as a direct consequence
  — a documented gap, not a silent one.
- A live daemon/protocol caller for `grant_sublease`/`ensure_child_account`.
  The protocol (`crates/protocol::Request::{Preflight,ToolInvoked,Finalize}`)
  carries only `session_id`, not an agent handle — so the "nested agents
  can't oversubscribe the parent's lease" acceptance criterion is
  demonstrated via **ledger-level transactional tests**
  (`crates/ledger/tests/hierarchical_lease_concurrency.rs`), not an
  end-to-end hook path. Recorded here explicitly so QA does not expect
  end-to-end evidence that has no producer yet. Zero `crates/daemon`
  edits were made — `reserve()`'s existing 32 call sites are completely
  unaffected; the new hierarchical API is additive.

## Consequences

- Capacity queries for any account go through `v_account_capacity`; a
  caller that queries `resource_accounts` directly for a task-level row's
  capacity will find `NULL` and must know to consult `task_budgets`
  instead, or use the view. This is intentional (single source of truth)
  but is a sharp edge worth remembering when writing a new query.
- `reservations.account_id` has a `NOT NULL`-via-convention but
  schema-nullable FK to `resource_accounts`; every reservation written
  going forward populates it (enforced by `initialize_task_budget` now
  also provisioning the task account in the same transaction `reserve()`
  depends on), but the column itself stays nullable because SQLite's
  `ALTER TABLE ADD COLUMN` cannot add a `NOT NULL` column with a
  `REFERENCES` clause to an existing table without a default that would be
  wrong for the backfill. `COALESCE(account_id, task_id)` in the
  idempotency index is the structural fallback that keeps this safe.
- HORO-1667 inherits the obligation to reserve its own migration number
  (next free after this ticket's `0011`) before it starts writing
  `economic_events` persistence, per the standing migration-ownership
  protocol.
