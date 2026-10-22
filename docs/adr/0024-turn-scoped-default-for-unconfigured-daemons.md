# ADR-0024: Turn-scoped default for unconfigured daemons (HORO-1727)

## Status

Accepted for v0.0.4 (the slices S1-S4 and D4 are merged on `main`; S5-S8 are in
progress). Founder decisions: HORO-1727 Jira comment 28055 (D1-D5) and the
decision packet in comment 28296 (F2, F3). Amends
[ADR-0021](0021-turn-scoped-resource-ceiling.md). **Not part of v0.0.3**:
`release/v0.0.3` keeps the lifetime-bounded default and migrations 0001-0015
unchanged.

## Context

ADR-0021 added a turn-scoped resource ceiling next to the lifetime envelope of
the one built-in policy (`balanced`: 100k target, 125k elastic, 150k hard) and
left the lifetime envelope as the binding limit. ADR-0021 measured real per-turn
fresh-token cost at p80 ~120,700 and p95 ~385,407, so two ordinary turns
already exceed the 150k lifetime hard ceiling, while each single turn sits
inside the 400k turn ceiling. A fresh install therefore stopped working after
roughly one real task even though no individual turn was out of bounds.

## Decision

1. **Scope is a property of the task budget, not of the policy.** A task budget
   is `LifetimeBounded` or `TurnScoped`, persisted in
   `task_budgets.lifetime_bounded` (migration 0020, default 1). `Policy` and
   `policy_json` do not change, so the limits keep a single record and a
   binary that ignores the column still enforces the stored 150k `hard_limit`
   (fail-closed rollback). A policy with no turn bound can never be turn-scoped.
2. **Only a daemon with no `config.json` at all is turn-scoped (D2).** A named
   preset without targets, any config file (even empty or misspelled), a
   rejected config and trusted-principal mode (D1) stay lifetime-bounded.
3. **A turn-scoped budget is gated on the turn bound, not the lifetime bound.**
   Ledger reservations are gated on `turn.ceiling - active` (so concurrent
   holds are capped at the turn ceiling); preflight skips the lifetime
   resource constraint but still enforces time, confidence, resource kind and
   `evaluate_turn`. The ceiling stays 400k (D3). `effective_hard_limit`,
   `available()` and the reconciliation arithmetic keep lifetime semantics.
4. **The persisted scope is authoritative.** Gates read the row, never the
   daemon's current configuration. A task created turn-scoped stays turn-scoped
   after a config is added (the doctor flags the stale running config, S5); a
   bounded task stays bounded when the config is removed.
5. **Token budgets settle on fresh tokens (D4).** Input + cache-creation +
   output; `cache_read_input_tokens` is excluded and stays recorded separately.
   USD budgets still price each tier. Rows settled before the change keep their
   cache-inclusive amounts and are not re-credited (fail-closed).
6. **No new migration for the flip (D5).** Rollback is a binary downgrade or
   reverting the seam commit; see F2 below.

## Consequences

- **S5 gates the v0.0.4 release (F3, recommended YES).** Until the display is
  scope-aware, statusline/doctor/explain would present a 150k lifetime budget
  for a turn-scoped task and show "exhausted" past 150k while admission admits.
  S5 adds `turn_ceiling` to the budget snapshot, a turn-scoped posture, an
  optional configured lifetime ceiling and a doctor scope comparison, and bumps
  `PROTOCOL_VERSION` 18 -> 19. Version skew already fails closed.
- **Spend is not capped in total for a turn-scoped task**: only per turn (400k)
  and by concurrent active holds (400k). Settled overruns are not capped.
  Operators who want a lifetime cap add a `config.json`, which governs tasks
  created afterwards.
- **Rollback (F2).** Reverting the seam changes only new tasks; existing
  `lifetime_bounded=0` rows stay turn-scoped for their remaining life. A
  downgraded binary reads the column but ignores it, so those rows are bounded
  at the stored 150k. No repair `UPDATE` is shipped (it would add a mutating
  path on a column nothing else mutates); an explicit opt-in re-bound command
  is a possible follow-up if the founder wants one.
- **Known limitations (fail-closed, documented):**
  - `gateway_authority` still evaluates the persisted lifetime bound, so a
    turn-scoped task that a mid-task `[gateway]` config would route through the
    gateway can be denied where preflight would admit. Whether the gateway
    should honour the persisted scope is an open founder decision.
  - `grant_sublease` and `v_account_capacity` still cap at the lifetime
    `hard_limit`; nothing calls them in production. Changing the view would
    need a migration.
  - Counterfactual replay reads `policy_json` and so treats turn-scoped tasks
    as bounded.
  - `running_config_matches_disk` flags a config added under a turn-scoped
    daemon, but not the reverse (trusted-principal mode shares dispatch and has
    no `config.json`).
- **Shared pools are unaffected.** The launch-gate pool and the metered shared
  pool (ADR-0023) read only `quota_pools` and `shared_pool_reservations`; a
  turn-scoped task cannot bypass a pool cap and no pool draw is capped at 150k.

## Release boundary

v0.0.3 is `release/v0.0.3` (ADR-0014 Option B). Migrations 0016-0020, this
default and the fresh-token settlement are v0.0.4-only and must not be
backported without a separate founder decision.
