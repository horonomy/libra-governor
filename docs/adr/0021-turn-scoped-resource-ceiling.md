# ADR-0021: Turn-scoped resource ceiling (HORO-1727 Decision B)

## Status

Accepted.

## Context

`Policy::resource` (`ResourceBound`) enforces a bound over a task's whole
*lifetime*: `used() + reserved() + requested <= hard_limit`, frozen into a
reservation at the task's first preflight and separately re-enforced by
the gateway (ADR-0003). That promise is load-bearing for every other
caller of this envelope and is explicitly out of scope to touch here.

A prior architecture investigation for HORO-1727 found that a single real
Claude Code turn (one Stop-to-Stop receipt window) can consume far more
than a sane per-turn amount while the task's *lifetime* envelope still has
plenty of headroom — nothing in the lifetime check catches this, because
it only ever sees the running total, never one turn's own contribution in
isolation. Two designs were considered:

- **(A) Reshape the lifetime bound into a per-turn-only check**, dropping
  lifetime spend from the gate. Rejected outright: this weakens an
  existing hard-deny and changes the ADR-0003 promise other callers rely
  on. Out of bounds without founder sign-off, and not attempted here.
- **(B) Add a second, independent, turn-scoped ceiling**, calibrated from
  measured per-turn data, checked only at preflight, additive to the
  existing lifetime envelope. Chosen and implemented by this ticket.

This ADR covers (B) only. `gateway_authority.rs`, ledger `reserve`/
`available`, `shared_pool.rs`, `quota_window`, and `pacing` are untouched.

## Decision

### Two separate scopes, never merged

| | Lifetime (`Policy::resource`) | Turn (`Policy::turn`, new) |
|---|---|---|
| Measures | `used() + reserved() + requested` | this turn's own projected request only |
| Enforced by | preflight admission **and** the gateway (ADR-0003) | preflight admission only |
| Modes | Hard / Elastic / Approval | binary: Admit or Deny, no elastic band |
| Can be extended | yes, via renewal (HORO-1727 bounded renewal) | no — see "Why no elastic band" below |

[`libra_governor_domain::policy::TurnBound`] adds exactly two fields:

```rust
pub struct TurnBound {
    pub cold_start_request: ResourceAmount,
    pub ceiling: ResourceAmount,
}
```

`Policy::turn: Option<TurnBound>` is `None` by default
(`#[serde(default)]`), so every existing policy — and every policy that
never opts in — behaves byte-identically to before this ticket.
`Policy::turn_fallback_request()` returns `cold_start_request` when a turn
bound is configured, else `resource.target` (the pre-existing fallback);
`Policy::evaluate_turn(projected_turn)` returns `Admit` when no turn bound
is configured, else `Deny(ResourceExceedsTurnCeiling)` iff
`projected_turn > ceiling`, else `Admit`.

`PolicyDecision::with_turn_outcome` folds a turn outcome into an
already-computed lifetime decision: a turn `Deny` always wins (it turns
`Admit`/`ApprovalRequired` into `Deny`, appending its reason onto an
existing `Deny` rather than replacing it); a turn `Admit` never changes
`admission` at all. `PolicyDecision.turn_outcome` records the raw
per-constraint outcome for rendering, independent of the fold.

### Why no elastic band / `ApprovalRequired` for the turn check

The lifetime bound's `Elastic`/`Approval` modes exist because *additional
pre-authorized spend* is a meaningful answer to "the lifetime running
total is higher than expected" — more budget, same kind of work. That
reasoning does not transfer to a single turn blowing through a sane
per-turn ceiling: by the time one turn is projected to need that much,
the likely cause is an unbounded loop or a runaway tool-call chain, not
legitimate work that would be well served by asking a human for more
per-turn headroom and then continuing the same pattern. A ceiling with no
band keeps the signal simple: a turn either fits, or something is
already wrong enough that continuing within this turn should stop.

### Why a ceiling derived from the same class p80 used as the estimate was rejected

An earlier idea considered deriving the turn ceiling from the same
per-task-class p80 the estimator already produces (`Estimate::resource_p80`)
— i.e., "the ceiling is whatever this task class normally needs." This was
rejected: a ceiling that tracks the estimate it is meant to check against
can never produce a `Deny` in the case that matters (a turn that is
anomalously large *for its own class*), because the ceiling moves with
the very quantity it is supposed to bound. The chosen ceiling is instead a
fixed, cross-class figure derived from the *overall* measured
distribution of real turns (see "Derivation" below) — high enough not to
trip on ordinary heavy work, low enough to still catch a genuinely
runaway turn.

### Where it is checked

`crates/daemon/src/server.rs`'s `handle_preflight_prepared`:

```rust
let turn_outcome = admission_policy.evaluate_turn(requested)?;
decision = decision.with_turn_outcome(turn_outcome);
```

immediately after the main `admission_policy.evaluate(projected, ...)`
call and *before* the Policy Webhook block — a turn `Deny` therefore
issues zero webhook calls (the webhook only ever fires on
`ApprovalRequired`) and cannot be overridden by an external `Approve`.
`requested` is this turn's own projected request (the same local already
used for the work-envelope reservation), never `projected` (which adds
committed lifetime spend on top) — the turn check and the lifetime check
read different quantities by design.

The replan path (`handle_tool_invoked`'s material-event replan) does not
re-evaluate policy at all today — it carries the prior plan's admission
forward (`was_not_admitted`) rather than calling `evaluate`/`evaluate_turn`
again. This ticket changes only that path's fallback-request figure
(`policy.turn_fallback_request()` in place of `policy.resource.target`),
matching `handle_preflight_prepared`'s own fallback; it does **not** add a
turn gate to the replan path, since that would be new behavior beyond
this ticket's scope and inconsistent with the replan path's existing
"no fresh policy evaluation" design.

### Business Context narrowing

`apply_business_context` narrows only `Policy::time` and calls
`Policy::validated_at`, which always produces `turn: None`. Left
unpatched, a deadline-narrowed policy would silently lose its turn bound.
This ticket re-attaches `base.turn` (if any) onto the narrowed policy
after `validated_at` succeeds — re-validation against the unchanged
`resource` bound can never fail in practice, since `with_turn_bound`'s
only two checks (`ResourceKind` match, `ceiling >= cold_start_request`)
depend only on fields this narrowing never touches.

### `completion_reserve_for`'s fallback

`crates/domain/src/reservation.rs::completion_reserve_for` already falls
back to the policy's one always-available resource figure when no
kind-matching estimate quantile exists. That fallback is now
`Policy::turn_fallback_request()` rather than `resource.target` directly
— with no turn bound configured (every policy before this ticket, and
every policy that never opts in), this is byte-identical to before.
`CompletionReserveBasis::PolicyTarget`'s variant name is kept as-is (it is
a persisted ledger column value) even though it no longer always means
literally `resource.target`; its doc comment is updated to say so.

## Derivation (cold-start request and ceiling)

### Why not the pre-fix architecture-investigation numbers

The original per-turn evidence (two transcripts, p80 487,836 /
552,391 fresh tokens) was measured **before** HORO-1882 fixed a
2.14x–2.39x transcript token double-count (Claude Code writes one
transcript line per *content block* of a turn, repeating the same
`message.id` and usage on each; summing every line summed the same usage
once per content block). Using those numbers, or even the stated
correction factor applied to them, directly would be building on data the
HORO-1882 PR's own findings call "most likely carries the same
double-count itself" (the factor was measured on data already produced by
the bug it was correcting for).

### Method actually used: direct recomputation, not a correction factor

Real local Claude Code transcripts were available on this machine
(`~/.claude/projects/**/*.jsonl`), so this derivation recomputes the
distribution directly under the now-corrected scanner, rather than
estimating it from stale numbers:

1. Every `*.jsonl` transcript under `~/.claude/projects` was scanned
   (4,795 files at measurement time, spanning many projects/worktrees —
   not just this repository's own sessions, since a cross-class,
   general-purpose per-turn distribution is what the ceiling needs to
   generalize over).
2. Within each file, assistant `message.usage` records were deduplicated
   by `message.id`, taking the **per-field maximum** across repeats — the
   exact rule `crates/daemon/src/usage.rs::scan` applies post-HORO-1882.
3. Records were segmented into "turns" at each real (non-tool-result)
   user message — an approximation of one Stop-to-Stop receipt window.
   Every usage record between one such boundary and the next belongs to
   the same turn.
4. Each turn's **fresh tokens** (`input + cache_creation + output`,
   excluding cache reads — matching
   `crates/daemon/src/usage.rs::TurnUsage::fresh_tokens`) was summed and
   recorded. No transcript content was read or retained beyond `type`,
   `timestamp`, `message.id`, and `message.usage.*` — the same fields
   `usage.rs` itself reads.

Over 16,554 such turns:

| Percentile | Fresh tokens |
|---|---|
| p50 | ~16,268 |
| p80 | ~120,700 |
| p90 | ~236,740 |
| p95 | ~385,407 |
| max | ~13,991,391 (one extreme outlier — a long batch/looped session) |

(A narrower scan restricted to this repository's own `libra-governor`
worktree transcripts alone produced a much lower p80 (~43,396 over 487
turns), but that subset's per-file turn counts were suspiciously close to
1 turn/file on average — a strong sign many of those worktree sessions
were short-lived or mostly idle, not a representative general-purpose
sample. The full, much larger, cross-project corpus above was used as the
primary evidence for exactly that reason.)

### Constants chosen

```rust
pub const DEFAULT_TURN_COLD_START_REQUEST_TOKENS: u64 = 120_000;
pub const DEFAULT_TURN_CEILING_TOKENS: u64 = 400_000;
```

- **`cold_start_request = 120_000`**: just under the measured p80
  (120,700), and — deliberately — just under `default_admission_policy`'s
  own `balanced` preset elastic ceiling (100,000 × 1.25 = 125,000). A
  cold-start turn request at or above that elastic ceiling would make
  every brand-new task's *very first preflight* trip the **lifetime**
  elastic band before the turn ceiling is ever reached — a behavior
  change to the lifetime check this ticket must not make, since Decision
  B is additive-only. `120_000 < 125_000` keeps a fresh task's first
  preflight (`requested == cold_start_request`, no estimate history yet)
  admitting cleanly under the shipped default, exactly as before this
  ticket.
- **`ceiling = 400_000`**: above the measured p95 (~385,407), rounded up
  to a clean round number for headroom above genuinely heavy single
  turns (a large refactor, a wide multi-file search), while remaining far
  below the ~13.99M outlier — which is exactly the kind of turn this
  ceiling exists to catch (a loop or runaway tool-call chain), not
  ordinary heavy work.

### A known, stated limitation: the turn ceiling never binds under the shipped default

`default_admission_policy()` is `Policy::balanced(100_000 tokens)`: target
100,000, elastic ceiling 125,000, hard ceiling 150,000. Both
`DEFAULT_TURN_COLD_START_REQUEST_TOKENS` (120,000) and, more importantly,
`DEFAULT_TURN_CEILING_TOKENS` (400,000) sit **above** that 150,000
lifetime hard ceiling. Under the shipped default, the **lifetime** check
denies first for any turn large enough to ever reach the turn ceiling —
the turn gate added by this ticket cannot bind, and is not claimed to add
protection, under `default_admission_policy()`'s own numbers. It becomes
load-bearing only for a `config.json` deployment with a larger
`resource_target_tokens` (e.g. `strict_budget`/`cost_first` style presets
configured generously, or `balanced` at a materially larger target),
where the lifetime hard ceiling sits above 400,000 and the turn ceiling
can actually trip first. This is a real, stated trade-off rather than a
hidden gap: tightening `default_admission_policy`'s own lifetime numbers
to make the turn gate bind there too would be a lifetime-envelope change,
which is out of scope for this ticket (Decision A, rejected above).

## Cache-read exclusion — not a guarantee of total billing cost

`fresh_tokens()` (the quantity the turn ceiling is measured in) excludes
`cache_read_input_tokens` by construction (see `crates/daemon/src/usage.rs`
module docs). A turn with enormous cache-read volume but modest
fresh-token volume can legitimately bill far more than this ceiling's own
number while still admitting cleanly under it, because that re-read spend
is simply not what this check measures. This is documented on
`DEFAULT_TURN_CEILING_TOKENS`'s own doc comment and must be surfaced
wherever a turn-based bound reaches a human: it is a bound on *newly
ingested or produced* work this turn, not a guarantee of this turn's total
upstream billing cost.

## Rendering / disclosure surface (additive only)

This repository does not currently render `Policy`/`PolicyDecision`
figures into human-readable prose anywhere in the real preflight/hook
path — `PreflightResult.admission: Option<PolicyDecision>` is exposed
only as structured JSON over the daemon's own wire protocol
(`libra-governor-protocol`), and no CLI/hook render site turns it into
text today (confirmed by grep: no occurrence of `ConstraintOutcome`,
`ApprovalRequired`, or `hard_ceiling` anywhere under `crates/cli/src/`
outside the `quota_cmd` pacing-gate simulation subsystem, which is a
separate, already-existing `quota explain`/`quota gen-fixtures` tool, not
the live admission path). Given that, this ticket's disclosure obligation
is satisfied additively at the two places that actually exist:

1. **`PolicyDecision.turn_outcome: Option<ConstraintOutcome>`** — carries
   the turn check's own outcome independent of how it was folded into
   `admission`, so a future render/explain surface can show it without
   re-deriving it.
2. **Doc comments on `DEFAULT_TURN_CEILING_TOKENS`, `TurnBound`, and
   `DenyReason::ResourceExceedsTurnCeiling`** state the fresh-vs-cache-read
   distinction and the "turn ceiling is a separate check from the
   lifetime envelope" fact in the one place a future renderer (or a
   person reading `cargo doc`) will find it before building a UI on top of
   these types.

Building a new human-text renderer where none exists today, for a field
nothing currently displays, was judged out of proportion to this ticket's
scope; the structural hooks above are the additive, low-risk form of the
disclosure requirement given the actual code found.

## Cross-references

- ADR-0002: task (not session) as the economic unit — the lifetime
  envelope this turn ceiling sits alongside.
- ADR-0003: gateway enforcement boundary — the separate, harder
  enforcement point for the lifetime envelope; untouched by this ticket.
- `crates/daemon/src/usage.rs` module docs — the HORO-1882 dedup fix this
  derivation's method directly reuses, and the fresh-token/cache-read
  definition this ceiling's units are built on.

## Consequences

- A second, independent `Deny` path exists, additive to every existing
  admission outcome. Nothing that previously admitted under the lifetime
  check alone can newly deny solely because of this ticket when no
  `TurnBound` is configured (`Policy::turn: None` is behavior-preserving
  by construction).
- `default_admission_policy()` now carries a `TurnBound`, but — per the
  "known, stated limitation" above — it does not change any real
  admission outcome under that policy's own current numbers; it exists so
  the mechanism is live and testable, and so a future retuning of
  `default_admission_policy`'s lifetime numbers inherits a turn gate
  already in place rather than having to add one later.
- `config.json` users (`resolve_policy`) get the same default turn bound
  attached automatically; no new config surface was added for overriding
  the two constants, since no concrete need for a different value was
  identified during this ticket (a future ticket can add
  `[policy].turn_cold_start_request_tokens`/`turn_ceiling_tokens` to
  `RawPolicyConfig` following the existing `resource_target_tokens`
  pattern, if needed).
