# ADR 0002: Task, Not Session or Token, Is the Economic Unit

- **Status:** Accepted
- **Date:** 2026-09-17
- **Ticket:** HORO-1124

## Context

Libra's North Star is *"Never start work you are unlikely to afford to
finish."* Every estimate, admission decision, and receipt Libra produces
needs a stable anchor to attach to. Three candidates exist in an agentic
coding tool:

1. **A token** — the smallest unit the provider bills on, but too granular
   to reason about: no individual token has a Definition of Done, and
   nothing about "afford to finish" is expressible at that grain.
2. **A session** — one conversation/process lifetime of the host agent
   (Claude Code, Codex, ...). Sessions are the unit these host tools
   already expose, which makes them tempting to reuse directly.
3. **A task** — a unit of work with an objective Completion Contract,
   independent of how many agent sessions it takes to satisfy that
   contract.

A real engineering task routinely outlives a single session: a developer
closes their laptop mid-task, a session crashes and is resumed, a task is
deliberately split across a `/compact` boundary, or a long task is picked
up the next day. If Libra anchored its ledger to session IDs, every one of
those ordinary interruptions would silently fragment one task's estimate,
spend, and outcome into multiple unrelated ledger entries — breaking both
the "did we finish what we estimated" comparison and the "afford to
finish" admission question, since neither can be answered from a partial
session's data alone.

## Decision

`TaskIdentity` (crate `libra-governor-domain`) is the durable anchor every
other domain type attaches to:

- [`CompletionContract`] is versioned per task, not per session.
- [`ExecutionEvent`] variants such as `SessionStarted`/`SessionEnded` carry
  a `session_id` as an *attribute of the event*, while the event itself is
  keyed to the owning `TaskId` — a session is something that happens to a
  task, not the other way around.
- [`ExecutionPlan`] and [`ExecutionReceipt`] both key off `TaskId` plus a
  specific `CompletionContract` revision, so an estimate and its eventual
  actual can be compared regardless of how many sessions ran in between.
- The ledger schema (`crates/ledger/migrations/0001_init.sql`) makes this
  physical: `tasks` is the parent table, and `events`, `contracts`,
  `plans`, and `receipts` all foreign-key to `task_id`, never to a session
  table — because there is no session table. Sessions are not
  first-class storage; they are a field.

Token counts remain a real input — [`ResourceAmount::Tokens`] exists
precisely because some providers only expose usage that way — but a token
count is never itself the unit an estimate, admission decision, or receipt
is keyed to.

## Consequences

- Multi-session tasks (the common case for any task that takes more than
  one sitting) produce one coherent trajectory, queryable via
  `LedgerStore::task_trajectory`, instead of N disconnected fragments.
- The Claude Code hook integration (HORO-1125) must resolve an incoming
  session to the `TaskId` it belongs to (e.g. via an external reference,
  a resumed conversation ID, or explicit user action) rather than
  minting a new task per session by default. That resolution logic is
  out of scope for this ticket; this ADR only fixes the storage-level
  invariant it must respect.
- The estimator (HORO-1126) can compute P50/P90 estimates over historical
  *tasks* with comparable Definitions of Done, rather than over sessions
  whose token counts alone say nothing about whether the underlying task
  was actually comparable.
- Any future "org/billing rollup" reporting (explicitly out of scope for
  MVP 1.0) will aggregate over tasks and their receipts, not over
  sessions or raw token totals, to stay consistent with the North Star's
  "cost per successful outcome" framing in `PRODUCT.md`.

## Alternatives considered

- **Session ID as the primary key**: rejected per the Context section —
  it fragments any task that spans more than one session, which is the
  common case, not the exception.
- **Token count as the unit of admission**: rejected — `PRODUCT.md`
  explicitly names "a generic token counter" as a non-goal; a token has
  no Definition of Done and cannot itself be "completed" or "aborted".
- **Deriving task identity implicitly from conversation continuity
  heuristics** (e.g. treating same-day same-repo sessions as one task):
  rejected for this ticket as too fragile and non-auditable; `TaskId` is
  an explicit, stable value instead, with resolution/attachment logic
  left to the hook integration ticket (HORO-1125).

[`CompletionContract`]: ../../crates/domain/src/completion_contract.rs
[`ExecutionEvent`]: ../../crates/domain/src/execution_event.rs
[`ExecutionPlan`]: ../../crates/domain/src/execution_plan.rs
[`ExecutionReceipt`]: ../../crates/domain/src/execution_receipt.rs
[`ResourceAmount::Tokens`]: ../../crates/domain/src/resource_amount.rs
