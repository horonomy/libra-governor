# Libra — Product North Star

> Never start work you are unlikely to afford to finish.

## Supporting invariant

Estimate the full Definition of Done before admission, reserve enough
resources to finish, and re-plan only when the expected benefit of
replanning exceeds its own cost, switching cost, and delay.

Every architectural and product decision in this repository is judged
against this North Star and its supporting invariant. If a feature does not
serve estimation, admission, resourcing, or disciplined replanning, it is
out of scope for Libra.

## What Libra is

Libra is a local **Governor** — a data-plane daemon that sits between an
agentic coding tool (Claude Code, Codex, etc.) and the LLM provider it
talks to. It estimates the cost and time-to-complete of a task *before*
admitting it, tracks spend against that estimate as the task runs, and
makes replanning a deliberate, auditable decision rather than an implicit
one.

## Three Golden Journeys

### 1. Preflight worker

A developer stays inside Claude Code or Codex. They describe a real task.
Before any tokens are spent, Libra plans the task and shows a probabilistic
cost and time-to-complete estimate, with a confidence band. The developer
decides whether to proceed — and then executes without ever switching to a
separate app or dashboard.

### 2. Auto-mode power user

A power user configures a policy that prioritizes time over cost, enforces
only a hard quality floor, and allows controlled cost elasticity within
that policy. Normal replans — the kind the policy already anticipates — 
happen automatically, with no interruption. Only a *material boundary
crossing* (a decision the policy did not already authorize) interrupts the
user.

### 3. Engineering management

An engineering manager or team lead looks at Libra's reporting and sees
cost per successful outcome, on-budget/on-time completion rates, the split
between rational and wasteful overruns, avoidable spend, and estimator
calibration over time. They do not see raw token counts as the primary
signal — token counts are an input to the metrics that matter, not the
metric itself.

## Non-goals

Libra is explicitly **not**:

- **A generic token counter.** Token counts are an input to cost
  estimation and ledger accounting, not the product.
- **A generic LLM router or gateway.** Libra's optional gateway component
  exists solely to enforce hard provider-spend limits where traffic can be
  routed through it — it is not a general-purpose multi-provider router or
  a way to compare model quality/pricing across vendors.
- **Another coding-agent chat UI.** Claude Code, Codex, and other existing
  agent UIs remain the host user experience. Libra does not build or
  compete with a chat interface — it plans, estimates, meters, and gates
  the work happening inside those tools.
