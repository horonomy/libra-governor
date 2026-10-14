# ADR-0020: Launch-gate admission primitive (HORO-1766)

## Status

Accepted.

## Context

HORO-1749 (v0.0.4) wants "apply pacing through one authorized launcher/
queue" — a way to stop more than a declared number of Libra-launched jobs
from running at once, so a shared resource (a provider seat, a CI
runner pool, a fixed-size worker fleet) is never oversubscribed by
concurrent launches. This ticket is split into two PRs:

- **PR1 (this ADR, this PR)**: the daemon-side admission primitive — an
  IPC surface a launcher calls before starting a job, and again
  periodically while that job runs, and again when the job finishes.
- **PR2 (separate, not started)**: `libra-governor run`, a CLI wrapper
  that actually launches a subprocess and talks to PR1's IPC around it.

This ADR covers PR1 only.

### Why this is not `libra_governor_domain`'s SUSTAIN/BURST simulator

ADR-0016 built a deterministic scheduler for a *different* question —
"when should a batch of candidate tasks start to spread across (or
burst against) a declared set of quota windows" — as pure, serialize-only
data and functions, with **no live consumer**, specifically because
HORO-1727 had not yet settled how a live admission path would actually
consume a schedule like that: synchronous daemon call, gateway
pre-authorization, ledger reconciliation — all still open. A build-time
guard (in `crates/domain/tests/`, scanning `daemon/src`, `gateway/src`,
and `ledger/src` for a live reference to that module) makes wiring a
live consumer a deliberate, reviewed decision rather than silent drift.

That decision is still unmade. This ticket does not make it. The gate
this ADR describes needs none of that module's forecasting machinery —
it answers a much narrower question ("is there a free slot right now,
and if not, when should I check again") using a primitive this
repository already has and already trusts: `libra_governor_ledger::shared_pool`'s
atomic `reserve_shared`/`release_shared`/`expire_stale_shared_pool_reservations`
(HORO-1763), extended in this same change with a matching `renew_shared`.
Every file this ticket touches in `daemon/src` is checked against the
same guard the SUSTAIN/BURST module is checked against, and contains no
reference to it — confirmed by `cargo test -p libra-governor-domain
--test pacing_not_wired_live` passing unmodified.

## Decision

### 1. Occupancy, not a consumable budget

A launch-gate pool is a **slot** pool: "capacity 2" means at most 2
Libra-launched jobs run at once, full stop. It is not a consumable
quota that drains over a billing period and resets — there is no
window, no reset mechanism, and no notion of "how much is left this
hour." A consumable-budget shape was considered and rejected: without a
window or a reset, a consumable pool is a one-way, lifetime-draining
budget with no honest way to credit it back when a job exits normally
(the process that held a unit of budget may not even still exist to
release it in the shape a consumable model expects). An occupancy model
sidesteps this entirely — a held slot is released (or expires) back to
exactly the same capacity, indefinitely, with no accounting drift.

### 2. No pacing consumption — blocked on a separate, not-yet-made decision

This primitive does not forecast, throttle a *rate* of starts, or
reason about a time horizon at all. It answers one question per call:
"is a slot free right now" (and, separately, "keep my slot" / "I'm
done"). Whether a *future* live-admission consumer (of the kind
HORO-1727 is meant to eventually settle) should also gate against this
same `launch:`-prefixed pool family is explicitly out of scope here —
tracked as a follow-up ("PR3" in this epic's own numbering), and is a
founder/HORO-1727-owner decision, not an engineering call this PR makes
unilaterally. Until that decision is made, nothing in `daemon/src`,
`gateway/src`, or `ledger/src` may reference the SUSTAIN/BURST module at
all — enforced by the existing build-time guard, left untouched.

### 3. Off by default, opt-in per pool

No `[launcher]` section in `config.json` means the gate does not exist:
every `Admit` answers `Unavailable { reason: not_enabled }`. An operator
opts a specific alias in by naming it under `[launcher].pools[]` with a
declared `capacity`; each pool can individually be `enabled: false`
without removing its declaration (useful for a maintenance window).
This mirrors ADR-0003 §1's precedent for the enforcement gateway: a
runtime-gated `Option<..>` config section, not a Cargo feature — the
code is compiled, linted, and tested on every build regardless of
whether a given deployment turns it on.

### 4. Enforcement scope is deliberately narrow, and says so

A `Status` reply's `enforcement_scope` field is always the fixed string
`"launcher_owned_direct_children_only"`. This primitive has no mechanism
to observe, let alone throttle, any process this gate was not itself
asked to admit — a job started outside the launcher is invisible to it.
Carrying this as wire *data*, not only a doc comment, means a future
broader scope is a visible protocol change, never a silent
reinterpretation of an unchanged field. PR2 (the actual launcher) owns
the mechanics of ensuring the jobs it starts are the ones this gate
tracks; this ADR does not speak to process-tree/ownership verification
at all.

### 5. TTL-held slots with a keep-alive, not a release-on-exit guarantee

A granted slot (`Start`) carries an `expires_at` and a suggested
`renew_every_secs`. The caller is expected to `Renew` periodically while
its job runs and `Release` when it finishes. If neither happens — the
launcher crashes, is killed, loses network — the slot is reclaimed once
its hold lapses (`expire_stale_shared_pool_reservations`, called at the
top of every `Admit`/`Renew`), never held forever. This is the same
crash-safety shape `libra_governor_ledger::shared_pool` already provides
for every other shared-pool consumer (HORO-1763 AC2) — reused, not
reinvented.

### 6. The idempotency/lost-response trap, and why `Lost` exists

`reserve_shared`'s idempotency key doubles as the ledger `session_id`
here (`launch_id`, minted once per launch attempt by the caller). A
caller that lost a `Start` response in transit can safely retry with the
exact same `launch_id` to discover what actually happened — **but only
if the retry is evaluated correctly**. `reserve_shared`'s idempotency
lookup has no state filter: it matches on `(pool_id, principal_id,
idempotency_key)` alone, so a retried `launch_id` whose original hold
already expired or was released still matches and comes back as
`AlreadyGranted`, carrying a now-stale row. Resolving that stale
`AlreadyGranted` to `Start` would be a silent, unguarded admit against a
pool that may since be fully resubscribed by someone else — exactly the
oversubscription bug this whole primitive exists to prevent. The
`LaunchOutcome::Lost` variant exists specifically to close this: a
replayed `launch_id` whose hold is no longer active and fresh maps to
`Lost`, never `Start`. The practical consequence for PR2: a *new* launch
attempt must always mint a *fresh* `launch_id` — retrying a dead one
forever finds `Lost`, never a fresh grant.

### 7. Release identity check happens in the daemon, not only the ledger

`release_shared` itself only checks the reservation `id` and its state —
it does not check caller identity. The launch gate checks
`principal_id`/`session_id` (and that the reservation belongs to a
`launch:`-prefixed pool at all) **before** calling it, so a `Release`
naming someone else's `reservation_id` is refused outright rather than
freeing a slot it doesn't hold (AC1: a release can never free a slot
still legitimately held by someone else). This read-then-conditionally-
write sequence relies on `crates/daemon/src/server.rs`'s accept loop
being strictly serial (one connection fully handled before the next is
accepted) — documented in `launch_gate.rs` itself, since a future
move to a threaded dispatch model would need to push this identity check
into the ledger's own atomic transaction instead.

### 8. `renew_shared` is new ledger surface, following existing discipline exactly

Added to `libra_governor_ledger::shared_pool` in this same change: an
`IMMEDIATE`-transaction, conditional-`UPDATE`-with-zero-rows-fallback
method matching `settle_shared`/`release_shared`'s existing shape. It
extends `expires_at` only for a row that is, in the same instant
observed: currently `active`, held by exactly the caller's
`principal_id`/`session_id`, and not already past its own stale
`expires_at` (a stale-but-not-yet-swept row is refused, not
resurrected — it must go through the ordinary expire-then-reserve path).
Anything else is refused and the row is left completely untouched.

## Consequences

- A launcher (PR2) gets a crash-safe, race-safe admission primitive with
  no new locking code of its own to get wrong — every correctness
  property traces back to `shared_pool`'s existing transaction
  discipline (HORO-1763) plus this ticket's one addition
  (`renew_shared`), not to anything bespoke in the daemon.
- The gate cannot and does not claim to throttle anything outside jobs
  the launcher itself started — `enforcement_scope` says so on every
  `Status` read, not just in documentation.
- Whether a live pacing/forecast consumer should ever gate against this
  same pool family stays an open, separately-owned question (HORO-1727).
  This PR adds no pressure to resolve it quickly: the gate is fully
  useful as a pure occupancy limiter on its own.
