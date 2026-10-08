# ADR-0016: Deterministic SUSTAIN/BURST pacing simulator (HORO-1765)

## Status

Accepted.

## Context

HORO-1749 (v0.0.4) needs a way to plan *when* a set of candidate tasks
should start against a declared set of quota windows (HORO-1763's
`crate::quota_window`): either spread out to last a long horizon
(SUSTAIN) or admitted as fast as headroom allows (BURST). The schedule
must be:

- **Deterministic and replayable** — the same scenario run twice
  produces byte-identical output, with no clock read, no RNG, and no
  `HashMap` (whose iteration order is not guaranteed) anywhere in the
  state the schedule is built from.
- **Honest about evidence** — a window whose unit has no corresponding
  estimate, or whose evidence is stale/undisclosed/indeterminate, must
  refuse rather than silently admit a task "for free". This campaign has
  hit that exact bug class (a mismatched-unit hold silently dropped by
  `quota_window::dedup_holds_by_id`-style filtering) more than once, in
  more than one module.
- **Never able to cancel in-flight work** — a principal whose actual
  spend overran its reservation may be throttled on *future* starts, but
  an already-admitted task is never retroactively stopped.

HORO-1727 (a separate, not-yet-resolved ticket) owns the question of how
a live admission path would actually consume a schedule like this one —
whether the daemon calls into it synchronously, whether the gateway
pre-authorizes against it, how it reconciles with the ledger's own
reservation state. None of that is decided yet.

## Decision

1. **`libra_governor_domain::pacing` is pure, serialize-only data and
   functions.** `forecast::earliest_safe_admit` and `step::step` take
   everything they need as parameters and return a value — no `apply`
   function, no daemon/ledger/gateway consumer, no feature flag gating a
   future live path. There is nothing to flag, because there is no live
   path yet.
2. **The forecast reuses `QuotaWindow::evaluate`, never re-derives it.**
   A candidate task's need is injected as a synthetic, never-persisted
   `OutstandingHold` (via the new `pub(crate)`
   `OutstandingHold::projected` seam) and the same
   `BlockingStatus`/`Relief` answer is read back. This is the only way
   to guarantee the forecast can never silently drift from what
   `evaluate` actually enforces.
3. **A Gauge (`OpaqueProviderSnapshot`) window is never probed with a
   synthetic hold.** `evaluate_gauge` ignores holds entirely — it has no
   notion of subtracting a quantity, only a point-in-time
   percentage/threshold reading. The forecast checks a Gauge window's
   own current `blocking` state and nothing else.
4. **`PacerState` is `BTreeMap`/`BTreeSet` only**, and every synthetic id
   this module derives is built by XORing a tag into an already-random
   `Uuid` (a window's own id), never `Uuid::from_u128(small_int)` — the
   latter is exactly the collision class this campaign has hit
   repeatedly when two independent small-integer-seeded id spaces
   overlap.
5. **This PR's state machine (`step`/`simulate`) is scoped to one
   schedule per `Scenario`** — SUSTAIN admits at most one task at a time
   for the whole scenario (not per-principal), and the only "evidence"
   it tracks is the holds this simulator itself created. This is a
   deliberate MVP scope, not an oversight: it is exactly the right size
   for "plan a deterministic schedule for one scenario", and a richer,
   ledger-integrated, multi-principal scheduler is exactly the kind of
   live-wiring decision HORO-1727 is responsible for making.
6. **`try_admit` skips a blocked or backing-off ready task and keeps
   trying lower-priority ones in the same pass, rather than stopping the
   whole pass** — a deliberate backfill choice, not an oversight. The
   alternative (stop at the first task that can't start) would let one
   principal's backoff, or one task's window conflict, starve every
   other ready task in the scenario, which is worse than admitting a
   lower-priority task out of strict order. The trade-off: a
   higher-priority, currently-blocked task can be passed over by a
   lower-priority one that happens to fit, and in SUSTAIN this also
   means a backfilled task updates `last_start`, which pushes the
   head-of-line task's own spacing window back further. This is
   accepted for the same reason as the headline choice: a schedule that
   makes some progress on lower-priority work is better than one that
   makes no progress at all while waiting on the head of the line.

## Consequences

- A caller gets a fully replayable, inspectable schedule (`Proposal`s
  tagged with the limiting window) it can diff, store, or hand to a
  human — before any system commits to acting on it automatically.
- `domain/tests/pacing_not_wired_live.rs` enforces decision #1
  mechanically: it fails the build the moment `daemon/src`,
  `gateway/src`, or `ledger/src` references `pacing` at all. Wiring a
  live consumer therefore requires either deleting or deliberately
  updating that test — never an accidental import.
- The MVP scope in decision #5 means this ticket's acceptance criteria
  are evaluated against a single-schedule scenario, not a live multi-
  tenant admission controller. Follow-up ticket: once HORO-1727 settles
  the live-admission architecture, a new ticket should extend `step` (or
  wrap it) with real ledger-sourced evidence and multi-principal
  concurrency, rather than retrofitting this module's MVP assumptions in
  place.

## Alternatives considered

- **Re-deriving window blocking/relief logic inside `pacing` directly**,
  so the forecast would not need the `OutstandingHold::projected` seam.
  Rejected: this would create a second implementation of
  sliding/fixed/bucket arithmetic that could silently drift from
  `quota_window::evaluate`'s, the exact duplication
  `requirement-zero`-style review exists to catch.
- **Wiring a live consumer now, behind a feature flag.** Rejected per
  HORO-1727 being unresolved — a feature flag still means a daemon/
  gateway/ledger code path exists and must be reasoned about, which is
  the premature decision this ADR explicitly defers.
