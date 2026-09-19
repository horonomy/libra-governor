# HORO-1154 — Study procedure

The overall protocol the founder runs. This document is the operational
glue between `recruitment-criteria.md`, `outreach-message.md`,
`consent-language.md`, and `evaluator-onboarding.md`.

## Recruitment target

**10–20 qualified participants** (see `recruitment-criteria.md` for
qualification). Recruitment continues until either 20 qualified
participants have started the evaluation window, or a decision is made
(and recorded on the tracking sheet) that the reachable pool is
exhausted below 10 — in which case, per `minimum-evidence-threshold.md`,
proceed with what was gathered and report the real denominator honestly
rather than waiting indefinitely.

## Evaluation window length: 2 weeks per participant

Justification: HORO-1154's questions are about *repeat use* (does a
strong-pain user try it once and abandon it, or does cost/time
visibility actually change how they work over time) and *replan/policy
behavior under real, varied tasks* — both need more than a single
session to show up. A window shorter than a week risks catching only
novelty-effect usage; a window much longer than two weeks risks losing
participants to attrition before the evaluation completes and delays
the whole gate unnecessarily. Two weeks is the shortest window long
enough to plausibly observe a second-use decision (keep using it vs.
quietly stop) without making the ask so large that few people finish
it.

If a specific participant's real work cadence doesn't fit two
calendar weeks (e.g. they only touch the relevant codebase a few days a
month), extend their window to their next 8–10 real working sessions
instead of forcing a two-week calendar window that would mostly measure
their absence. Record the adjustment and reason on the tracking sheet.

## Check-in cadence

- **Kickoff**: the onboarding walkthrough itself (Step 1–3 of
  `evaluator-onboarding.md`), ideally live or synchronous, to catch
  install problems immediately rather than losing a participant to a
  broken `doctor` output they never report.
- **Midpoint (around day 7)**: a brief async check ("still using it? any
  blockers?") — not a full interview, just enough to catch a silent
  drop-off early enough to ask why while it's fresh.
- **End of window (around day 14)**: the full check-in — the evaluator
  runs `evidence-report` (Step 4 of onboarding) and the founder conducts
  the `interview-followup-questions.md` conversation.

## What "drop-off" means operationally

A participant is recorded as **dropped off** (not simply "still in
progress") when either:

- They explicitly say they're stopping, at any point, for any reason
  (record the stated reason verbatim if given — this is itself
  evidence, especially if the reason is "too much friction" or "I just
  forgot about it"), or
- No response to two consecutive check-in attempts (midpoint + end-of-
  window) spanning at least 7 days past the scheduled end-of-window
  check-in.

A dropped-off participant still counts in the recruitment funnel
denominator (`analysis-report-template.md`) and their drop-off reason
(explicit or inferred-from-silence) is itself a data point for
`minimum-evidence-threshold.md`'s kill-signal checklist — do not simply
omit them from the final report.

## How qualification is verified and recorded

Not vibes: each qualification-checklist item from
`recruitment-criteria.md` is recorded as a specific yes/no (with a short
note where relevant, e.g. "paid usage via employer, self-reported") on
the participant's row in `tracking-sheet-template.md`, filled in during
or immediately after the kickoff conversation — not retroactively
reconstructed at analysis time.

## Consent record-keeping

Two separate consent events per participant, both recorded on the
tracking sheet with a date:

1. Agreement to `consent-language.md` (verbal is acceptable, but record
   that it happened and when).
2. Confirmation that `libra-governor evidence-report consent` was
   actually run on their machine (self-reported by the participant, or
   visible in the timestamp inside their exported JSON if they share
   it).

## End of study

Once recruitment and evaluation windows for all participants have
concluded (or the pool is exhausted per the recruitment target section
above), move to `analysis-report-template.md` and fill it in using only
real collected data — that template explicitly refuses speculative
completion.
