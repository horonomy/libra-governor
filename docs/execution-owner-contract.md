# Execution Identity owner interface v1

HORO-1600 owns this interface. HORO-1714 consumes it; it must not create a
parallel identity, association store, replay ledger or attribution cache.
ExecutionIdentity remains the accepted v1 wire contract from HORO-1598 / Libra
PR55. TaskId remains Libra's separate economic unit.

## IPC and authority

Send `Request::ExecutionOwner { event: ExecutionOwnerRequest }` over the existing
local Unix socket using **IPC protocol 12**, **association_version 1** and
**ExecutionIdentity envelope_version 1**. `Response::ExecutionOwner` returns
`ExecutionOwnerOutcome`. Version mismatch is explicit and commits no effect.
Unhandled identity dimensions are rejected on owner IPC, even where the shared
v1 historical-record parser permits unknown fields. Older binaries require restart; neither missing fields nor version skew may be
interpreted as an empty/host-scoped identity.

The request carries the full identity and `NativeExecutionContext` containing
its own full identity plus an operation. Both identities must describe exactly
the same acquisition, including observation/event values. A trusted local native
acquisition must build both from the same input, including prompt/cwd/model when
present. Diagnostic observations from untrusted/external normalizers are not
native authentication and must not be promoted into these requests.

The trust boundary remains the owner-only socket in the private state directory.
These are claims by its permitted local clients, not cryptographic provider
attestations. This interface does not grant external adapters code trust or
permission to execute. The consumer is responsible for invoking it only from its
existing authorized native acquisition path.

## Scope and results

Session and turn IDs must be concrete, nonempty native values. Agent ID is
genuinely optional (HORO-1714 decision A, 2026-10-10): an agent-absent event is
its own explicitly represented lane, keyed `[host, provider, session, null]` --
JSON `null`, never the string `"null"`, so it can never collide with an agent
literally named that. Unknown lineage is supported as unknown; missing agent
never means root, and any claimed `lineage_status` other than `unknown` without
a reported `agent_id` is refused (`Unsupported`), not silently coerced. All
optional identity dimensions match by presence and value. `correlates_with` is
unsuitable for selection because its partial matching is deliberately broader.

The durable lane is the canonical agent tuple of host/provider/native session/agent.
All richer v1 dimensions, including tool-instance, lineage, parent, session-lineage,
repo and worktree, are exact immutable lane facts. Any presence/value drift returns
ambiguous even for a new turn with the correct predecessor; it cannot create a
second economic lane or rewrite unknown lineage. Each immutable turn stores the
full v1 identity, excluding only observation/event values from position equality.
Persisted position objects sort keys explicitly, so enabling a dependency JSON
map-order feature cannot change association keys across builds or restarts.
Rich identity never falls back to a current/latest host or session result.

- `Prompt { task_hint, cwd, supersedes_turn }` creates/reuses the lane's TaskId and creates a plan;
  a new native turn supersedes the old turn only when its explicitly correlated
  predecessor matches the current persisted native turn. `None` is accepted for
  a new lane, or (HORO-1714 decision B, 2026-10-10) for an existing lane whose
  current turn is already confirmed **finalized** -- a narrowly scoped,
  owner-managed succession for providers with no native predecessor field,
  never inferred from timestamps, cwd, a latest-session lookup, or a
  hook-local cache. If the current turn is still active (no Stop observed),
  `None` is refused as ambiguous, exactly as when a predecessor is required
  and absent. The guard is a compare-and-swap on the lane's `current_turn`
  column inside the one `BEGIN IMMEDIATE` transaction, not merely a prior
  read, so concurrent successors against the same finalized turn resolve to
  exactly one winner. A delayed/unordered prompt returns stale/ambiguous
  without mutation; timestamps and opaque turn IDs never establish order.
  Prompt content is recon input only.
- `Tool { native_call_id, tool_name }` requires an existing active turn. It uses
  the exact current PlanId, including a replan, and returns the resulting target.
- `Stop { model, transcript_path }` finalizes only that active turn and closes its
  association. The existing economic policy and provenance semantics apply.
- `Query` reads the requested active turn's exact TaskId, current PlanId, initial
  PlanId and lineage, plus the ledger budget snapshot. It uses no cross-lane cache.

`Applied` returns the exact target and effect. `Resolved` is read-only.
`Duplicate` returns the original effect's durable target and makes no effect.
`Unavailable` carries `missing`, `ambiguous`, `stale`, `unsupported` or
`replay_conflict`; it commits no economic effect. Internal transaction failures
return a storage error; when commit/rollback status cannot be confirmed, consumers
must retry the exact native key to consult durable replay rather than infer a
no-effect result. SQLite transaction atomicity prevents a partial committed effect. A query of a closed turn is
stale; an exact Stop retry can still return Duplicate before a successor is bound.

## Replay and restart

Replay keys are `(lane, operation, native_ref)`: native turn ID for Prompt/Stop,
and native tool call ID for Tool. Fresh transport/event UUIDs and timestamps are
excluded. A tool reference reused across turns is replay_conflict. Superseded
turns remain stale even when an old event previously applied.

An IMMEDIATE SQLite transaction validates association/replay, applies the existing
economic handler, records the turn transition and replay target, and commits
once. Existing inner economic transactions use savepoints within that boundary;
standalone guarantees remain unchanged. Failure/panic rolls back the whole effect.
Reopening the database preserves duplicate protection. Replan selection reads the
lane's persisted in-flight plan under the same lock as the effect.

## Supported boundaries

The initial owner route returns unsupported for effects with no mutation when gateway or
external business-context/policy callbacks are configured. Those routes cannot
reuse session-only ownership or perform external calls inside the effect lock.
Exact read-only Query remains available in those configurations.
Bounded recon and first-use extension credential validation run before the write lock. Queue writes may join the transaction;
external event delivery remains outside it.

A supplied session transcript path is also unsupported: the current reader
measures a session window and cannot prove agent/turn ownership. With no path,
usage remains unavailable and settlement retains the existing conservative
`usage_known=false` provenance; it is not measured billing. No sibling's usage is
silently attributed. This is a real capability limit for the consumer.

Legacy hooks remain compatible. Legacy `StatusResult.scope` is explicitly
`host_latest_observation`; shared provider labels identify the latest selected
host task/work, which is not this terminal/session or a sum of every active task.
Scoped consumers must use owner Query and must never fall back to Status. They cannot mutate the
reserved internal `execution-owner-v1:` namespace. Owner queries never select
legacy rows, and an existing flat record colliding with that namespace makes
initial association ambiguous. No session/agent/turn is backfilled.

## Migration and cache policy

SQLite schema **15** added `execution_lanes`, `execution_turns`, and
`execution_replays`; it changes no legacy columns. (Migrations have since
advanced past 15 for unrelated reasons; these three tables' own shape is
unchanged by HORO-1714 decisions A/B/C, which are pure application-logic
changes to how existing columns are read and written.) Primary keys index lane/turn
and lane/operation/native-ref, so lookup does not scan host history. Owner queries
read durable state directly; there is no owner cache to become stale on session
switch or restart. Any future cache must key the exact position and invalidate
on the committed turn/plan transition.

Upgrade preserves old rows as unattributed legacy records. Downgrade is an
operator operation: stop newer clients/daemon first and restore a pre-upgrade
backup for full rollback. An older binary can read the unchanged legacy tables,
but cannot consume owner protocol 12 or reinterpret owner state as native session
truth. Do not drop replay history while accepting owner traffic: that would erase
restart protection. No automatic destructive down migration is provided.

## Evidence boundary

`crates/daemon/tests/execution_owner.rs` exercises real socket framing, daemon
policy and SQLite effects with synthetic identity fixtures, including (as of
HORO-1714 decisions A/B/C, 2026-10-10) agent-absent lanes, owner-managed
succession under real concurrent threads/SQLite, and a real-row assertion that
unknown usage settles conservatively and never persists as zero. Native
lifecycle characterization is separately recorded in
`native-agent-lifecycle-characterization.md`. These tests do not claim a real
Codex native positive control or complete HORO-1714 -- real measured
task-level usage and real-host acceptance remain separate, open gates.
