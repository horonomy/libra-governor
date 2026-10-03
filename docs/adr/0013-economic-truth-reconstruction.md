# ADR-0013: Economic truth reconstruction without database inspection

Status: Accepted
Ticket: HORO-1672
Depends on: HORO-1666 (economic attribution), HORO-1668 (hierarchical resource
accounts, ADR-0008), HORO-1669 (progressive estimator, shadow decisions),
HORO-1670 (policy replay).

## Goal

Let an operator ask "which task/session/agent spent this resource, what was
allocated/reserved/settled, what did descendants consume, what was estimated
at the time, and where is attribution incomplete?" without opening SQLite —
via `libra-governor economics explain --task|--session|--account|--principal|--organization <value> [--json]`.

## Two load-bearing honesty findings

**1. The custody tree's depth path has no production writer.** `ensure_child_account`
and `grant_sublease` (`crates/ledger/src/resource_account.rs`) are called only
from tests today. Nothing in `crates/daemon`, `crates/gateway`, or `crates/cli`
calls either in a real run. Consequently, on any real database
`resource_accounts` holds task-level rows only — `account_spend(id, Inclusive)
== account_spend(id, Exclusive)` always, `account_count == 1`, and the
rendered execution tree is a single node. "Proven nested-agent lineage
reconstructable" (the AC) is real and tested, but exercised only against
hand-built fixtures until a separate ticket adds a production writer for who
funds a session/agent account, with how much, under what authority — an
allocation-policy decision, not this ticket's to make unilaterally.

**2. `EconomicEvent` is never persisted.** No migration (0001–0014) created a
table for it; `economic_rollup`/`economic_ingest` are pure libraries with zero
live callers. So "reconcile exclusive/inclusive totals to canonical ledger
events" cannot mean replaying a persisted `EconomicEvent` stream — it means
the real persisted records: `task_budgets`, `reservations`, `gateway_requests`,
`plans`/`receipts`, `shadow_runtime_decisions`. This surface does not
synthesize `EconomicEvent`s from `reservations` rows to manufacture a richer
story — doing so would invent attribution the ledger never recorded and would
collapse the two trees ADR-0008 says must stay cross-referenced, never
unified.

Both findings are stated here rather than papered over, because this is the
last feature ticket before HORO-1673 (the falsification gate) — an explain
surface that looks more complete than the data backing it would defeat the
gate's purpose.

## Architecture: three layers, no migration, no protocol bump

| Layer | File | Contains |
|---|---|---|
| domain | `crates/domain/src/economic_truth.rs` | types, reconciliation arithmetic, category/presence logic, tree assembly — no SQL |
| ledger | `crates/ledger/src/economic_truth.rs` | thin row readers, reusing `account_spend` (HORO-1669), `Reservation::outstanding_draw`/`overrun`/`refunded`, `task_id_for_session`, `get_plan`, `shadow_decisions_for_task` verbatim |
| cli | `crates/cli/src/economics_cmd.rs` | human + JSON rendering, redaction at the display boundary |

No `Request`/`Response` protocol variant, no daemon change, no migration, no
new crate, no `clap`. The CLI opens the ledger directly — the same precedent
`crates/cli/src/dogfood_evidence_cmd.rs` already uses — because this is a
local, read-only, operator-invoked command with no need for the daemon's
in-memory state.

## Scope anti-widening: the property this ticket exists to get right

Many sessions can map to one task; the task-level account is shared. A naive
`--session S` query that resolved S→task and returned the task's
`account_spend` would report every session's spend as S's — the ticket's own
named failure ("a Session A query must never fall back to Session B's
newest value"), in a subtler form. `ScopeResolution` makes this structurally
impossible: a session selector first looks for its own account; absent one,
it falls back to a row-filtered view reported **beside** the enclosing
account's total, never **as** it. Every `ScopedAmount` carries its own
`AmountScope`, and the renderer has exactly one `scope_suffix` function that
every amount must pass through — a widened figure cannot be printed without
its widening label.

## Settlement honesty: observed vs. assumed

`SettledSpend` keeps `usage_known = 1` rows (`observed`, a real reported
figure) structurally separate from `usage_known = 0` rows (`assumed`, the
conservative settle-at-reserved-amount fallback). Today most settlements are
the fallback — the gateway only reports real usage when it actually metered
the call. Collapsing both into one "actual spend" number is the cosmetic-
trustworthiness failure this ADR exists to prevent; the schema already
distinguishes them, this surface just stops hiding that distinction.

## Reconciliation: three checks, reported as data, never equalized

1. **SubtreeAdditivity** — `inclusive(root) == exclusive(root) + Σ exclusive(descendant)`,
   a genuine independent re-derivation (one recursive CTE vs. N point queries).
   `NotApplicable{SingleNodeTree}` on every real database today (see finding 1).
2. **EnvelopeFormula** — re-derives `hard_limit − settled − active − completion_reserve`
   and compares against the independently-written `LedgerStore::available`.
3. **GatewayLedgerAgreement** — the only truly cross-source check: compares
   `Σ gateway_requests.settled_amount` against `Σ reservations.settled_amount`
   for the same `reservation_id`s. This can legitimately disagree in a correct
   system (a settlement recorded in one place and lost in the other,
   `bound_violated`, `settled_after_expiry`) — reported as `Discrepant`, never
   silently equalized.

No reconciliation is attempted between the custody tree and the provider-
proven agent-lineage forest (`economic_rollup`). That absence is itself
reported via `ProviderLineage::NoBasis{NoPersistedEconomicEvents}` — claiming
a reconciliation there would be the ADR-0008 violation.

## Identity redaction

`ExecutionIdentity::display_id` never actually used `&self`; its body is
extracted as a free function `redacted_display_id(field_name, raw_value)` so
this surface can redact a raw provider session id without persisting or
constructing an `ExecutionIdentity`. `TaskId`/`PlanId`/`AccountId`/
`ReservationId` are never redacted — Libra-internal, locally minted, already
opaque (same reasoning `statusline_provider.rs` already documents for
printing task/plan ids in full).

## Statusline: zero changes

This question was already escalated once, for HORO-1667, and already decided
by the founder (ADR-0009, 2026-10-02): the statusline compositor denies
providers the host payload by design, to prevent exactly the dependency this
would create. It is not re-opened here. The AC "current-session statusline
selection uses provider-native execution identity when available" is
satisfied honestly by the existing `SCOPE == "host"`, which already declares
that identity is not available to the statusline. A fifth segment is not
structurally possible (the 4-segment cap is already fully used by
task/estimate/budget/profile). Detailed historical tree/reconciliation
belongs in `economics explain`, exactly as the ticket itself specifies.

## Explicitly out of scope

No migration. No protocol bump. No daemon change. No new crate. No `clap`.
No MCP surface. No statusline change of any kind. No writer of any kind — not
a custody-tree writer, not an `EconomicEvent` table, not an `ExecutionIdentity`
persistence path. Each is a separate ticket with its own semantics.

## Flagged for HORO-1673

This surface will honestly report a single-node tree and `ProviderLineage::NoBasis`
on every real database, because nothing currently produces custody-tree depth.
If the falsification gate needs to evaluate multi-level attribution against
real dogfood data, a writer ticket (who funds a session/agent account, with
how much, under what authority) needs to land first. That is an allocation-
policy decision for the founder, not something this ticket decided
unilaterally.
