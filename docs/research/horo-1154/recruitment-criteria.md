# HORO-1154 — Recruitment criteria

Target: **10–20 qualified participants**. This document defines
"qualified" concretely enough that a prospect can be checked off, not
judged by vibes.

## Qualification checklist (all must be true)

A prospect qualifies if they can honestly answer "yes" to every item
below. Record the answer per item on the tracking sheet
(`tracking-sheet-template.md`) rather than a single yes/no gut call.

1. **Recurring, real usage.** Uses Claude Code and/or Codex on at least
   3 separate days per week, for at least the 4 weeks before recruitment
   — not a one-time trial.
2. **Non-trivial engineering work.** The work is on a real production
   codebase or a substantial side project (not a toy/tutorial repo) —
   multi-file changes, real tests, real CI, or real users depending on
   the code.
3. **Autonomy exposure.** Has used, or is willing to use during the
   evaluation window, some form of extended/auto-mode agent execution
   (multiple tool calls per turn without re-approving each one) — this
   is the population HORO-1154 actually needs signal from, since Libra's
   value proposition concentrates there.
4. **Cost/time visibility already matters to them.** Either (a) their
   own or their employer's usage is metered/paid in a way they are aware
   of, or (b) they have independently expressed frustration about
   runaway sessions, unpredictable task duration, or "the agent went off
   the rails" — self-reported, recorded verbatim on the tracking sheet,
   not inferred.
5. **Willing and able to install real tooling.** Comfortable installing
   a Rust CLI binary and editing `~/.claude/settings.json` (or willing
   to follow `evaluator-onboarding.md` step by step) on a machine they
   actually do real work on — not a disposable VM they won't return to.
6. **Willing to give explicit consent.** Has read and can affirmatively
   agree to `consent-language.md` before any local export is generated.
7. **Reachable for a follow-up.** Available for at least one
   interview/follow-up conversation during or after the evaluation
   window (see `study-procedure.md` for cadence).

## Explicit disqualifiers (any one disqualifies)

- Has only tried Claude Code or Codex once, or used it for fewer than 4
  distinct sessions total.
- No real recurring engineering work — student exercises, one-off
  scripts, or "I opened it once to see what it does."
- Cannot or will not install real tooling on a machine they actually
  work on (e.g. only willing to describe hypothetical usage).
- Works exclusively in an environment Libra cannot run in yet (no Unix
  socket support, no local filesystem access — e.g. a fully browser
  sandboxed environment with no CLI access).
- Employed by or closely affiliated with a competing agent-governance /
  cost-management product, unless disclosed and explicitly accepted as
  an informed edge case (record the disclosure on the tracking sheet).
- Unwilling to give explicit consent for local evidence export — a
  non-consenting user can still be a useful qualitative conversation,
  but does not count toward the 10–20 quantitative target.

## Sourcing pool (where to look, not a promise of yield)

<placeholder: list of candidate communities/channels the founder
actually has access to — e.g. specific Discord/Slack communities,
personal network, Twitter/X threads about Claude Code power usage,
existing beta testers if any exist>

## Target composition (soft guidance, not a hard quota)

Aim for a mix across at least two of the following axes, so the evidence
isn't dominated by one usage pattern:

- Individual/side-project users vs. company-paid/team users.
- Claude Code-only vs. Codex-only vs. both.
- Heavy auto-mode users vs. mostly manual-approval users.

Record actual composition achieved in `analysis-report-template.md`'s
recruitment funnel section — do not force the mix if the real applicant
pool doesn't support it; report the real skew instead.
