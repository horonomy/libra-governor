# ADR-0014: v0.0.3 GO decision — promoted scope and explicit exclusions

> **Numbering note**: this local number, `0014`, coincides with an
> **unrelated** cross-repo ADR series already cited in this codebase's own
> comments — `crates/cli/tests/write_lock_cross_process.rs`,
> `crates/cli/src/codex_hooks_file.rs`, `crates/cli/src/claude_settings.rs`
> all cite an "ADR-0014" belonging to HORO-1380's cross-process write-lock
> design, a *different*, cross-repo numbering scheme. This document is
> strictly `docs/adr/0014-v0-0-3-go-scope-and-exclusions.md` in *this*
> repo's own local `docs/adr/NNNN-*.md` sequence (following
> `0013-economic-truth-reconstruction.md`) — see the identical disambiguation
> note in `0012-policy-replay-and-decision-regret.md` for the precedent.

- **Status**: Accepted
- **Ticket**: HORO-1689 (evidence), founder verdict recorded 2026-10-04
- **Depends on**: HORO-1673 (falsification gate), HORO-1689 (real DogFood
  evidence), the consolidated founder decision packet (Claude Docs artifact
  linked from HORO-1689)

## Decision

The founder's verdict on the v0.0.3 Agent Execution Economics campaign is
**GO, narrowed to the evidence-supported scope.** This ADR is the durable,
in-repo record of exactly what that scope is, so that no later doc, release
note, or capability description can drift into overclaiming what has not
been evidenced.

### Promoted (production-trusted as of v0.0.3)

- **Duration estimation** — the progressive/bucketed estimator
  (`libra-governor-estimator`), its calibration reporting
  (`libra-governor calibration report`), and the quantile-coverage
  methodology behind it.
- **Admission** — preflight cost/time estimation and the admission-replay
  policies that gate on it.
- **Flat / single-tenant resource accounts** — the `task`-level resource
  accounting path exercised by every real production account today.

This scope is backed by real evidence, not synthetic fixtures alone: 127
real qualifying calibration pairs (`N_total=141`, pre-registered predicate
per `docs/research/horo-1673/promotion-criteria.md` §2), real cross-agent
confirmation (Claude Code and Codex), real concurrent-session confirmation,
and two real adversarial-recovery cases (a killed task's reservation
correctly expiring; a killed daemon correctly self-healing with no data
loss). Full evidence: `experiments/v003_1689_dogfood/README.md` and the
consolidated founder decision packet.

### Explicitly NOT included in this GO

**1. Shadow-decision / early-warning decision-quality claims.** The
proactive Stop/Degrade shadow-policy mechanism (ADR-0011, HORO-1669) has
**zero qualifying evidence — real or synthetic — across two separate
evidence-gathering phases** (`N_classifiable=0` against a floor of 20 for
both false-stop and false-degrade rate; `N_observations=0` against a floor
of 10 for early-warning lead time). This is tracked as an unresolved
evidence/instrumentation gap (HORO-1693), not as a disproven mechanism and
not as something merely "not yet measured" — it has been actively measured
twice and both times produced zero classifiable samples, which itself is
informative: the cadence condition is not firing at the rate real usage
would need.

**2. Multi-level / multi-tenant hierarchical custody accounting.** The
custody-tree hierarchy (organization → principal → session → agent,
ADR-0008, HORO-1668) has **never been exercised by real data, and no real
code path can reach it today.** Verified directly: every real production
`resource_accounts` row (21/21 as of this evidence round) is flat
`task`-level with no `parent_account_id`; `ensure_child_account` and
`grant_sublease` have zero callers anywhere in `crates/daemon` or
`crates/cli` (confirmed already in ADR-0013's own honesty findings). This
remains **research/latent** per HORO-1694: do not add production wiring for
this path merely to satisfy a future evidence gate — only build it against
a genuine multi-level product requirement, when one exists.

### What this means operationally

- Documentation, release notes, marketing copy, and capability descriptions
  must describe only the promoted scope above as production-validated.
  Mentioning the shadow-decision mechanism or hierarchical custody
  accounting as *existing, implemented design* (which they are — both
  shipped and are tested against synthetic fixtures) is accurate; describing
  either as *evidenced in production* or *ready for real-usage reliance* is
  not, until the respective tracking ticket closes with real evidence.
- `PRODUCT.md`'s "Three Golden Journeys" remains the product's aspirational
  vision and is unaffected by this ADR — Journey 2's shadow-decision
  behavior is a target experience, not a claim about today's evidence
  state. This ADR constrains *status claims*, not product vision.
- Nothing in this ADR authorizes weakening, editing, or re-litigating
  `docs/research/horo-1673/promotion-criteria.md`'s pre-registered floors.
  Both exclusions above are real measurement outcomes, not threshold
  disputes.

## Reversibility

This scope can widen with new evidence (closing HORO-1693 and/or
HORO-1694's conditions) without requiring any code to be undone — the
promoted-scope code and the excluded-scope code already coexist safely; the
exclusion is a documentation/trust boundary, not a build-time feature flag.
It can also narrow further (a future PIVOT/STOP on a promoted item) without
affecting the excluded items' already-latent status.

## Evidence trail

- Founder decision packet (Claude Docs, consolidated): linked from HORO-1689.
- `experiments/v003_1689_dogfood/README.md` — full real evidence.
- HORO-1673 (preserved baseline), HORO-1689 (real evidence, Done),
  HORO-1690 (model/provider instrumentation, open, non-blocking),
  HORO-1693 (shadow-decision gap, open, tracked by this ADR),
  HORO-1694 (custody-tree latent status, open, tracked by this ADR).
