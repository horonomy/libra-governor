# HORO-1154 — Minimum evidence threshold

Restates the ticket's own pass/fail guidance as a concrete, checkable
checklist. This document does not decide the outcome — it defines what
"enough real evidence to decide" looks like, so `analysis-report-
template.md` can be filled in against a fixed bar rather than a
post-hoc rationalization.

## Raw-denominator reporting requirement

Every finding below must be reported as **N out of M** (e.g. "6 of 14
qualified participants") — never as a bare percentage and never as a
vague "most"/"several" without the underlying count. If fewer than 10
qualified participants were reached (the floor from
`recruitment-criteria.md`/`study-procedure.md`), that must be stated
explicitly as a limitation on the recommendation's confidence, not
silently omitted.

## CONTINUE / pull-signal checklist

Evidence supports continued investment (team/shared-policy, cross-agent
support, or a paid-discussion track) when the real data shows:

- [ ] **Not routinely bypassed for being slow/annoying.** Across
      qualified participants, the product is not being worked around or
      abandoned specifically because admission/replan interruptions felt
      like friction — check the `dropoff_reason` column and the
      "perceived friction" interview answers for this specific pattern,
      not overall drop-off for any reason.
- [ ] **Several users recognize a recurring cost/time predictability
      problem.** Multiple (report the raw count) qualified participants
      independently, without being led, describe the cost/time
      unpredictability problem Libra addresses as something they
      actually experience in their own work — not merely agree when
      asked leadingly.
- [ ] **Concrete pull for team/shared policy, cross-agent (Codex)
      support, or paid discussion.** At least some participants express
      genuine, specific pull toward one of these — not polite
      hypothetical interest, but a concrete "my team would want X" or
      "I'd pay for Y" with enough detail to act on (see the
      "willingness-to-pay" and "team/Codex demand" interview questions).

## ITERATE / PIVOT / KILL signal checklist

Evidence points toward stopping or substantially changing direction
when the real data shows:

- [ ] **Strong-pain users try once and don't return.** Participants who
      independently described a real cost/time pain point before or
      during onboarding nonetheless show up as drop-offs during the
      evaluation window, without a clear external reason (e.g. not
      "I went on vacation" but "I just stopped reaching for it").
- [ ] **Most users prefer free historical tracking and ignore
      governance.** When asked what they'd actually want (the "would
      route more work through it" and "what would you change"
      questions), a majority gravitate toward passive
      observability/history features over Libra's actual admission/
      replan governance mechanism — i.e. they want a dashboard, not a
      gate.

## How to use this at analysis time

1. Go through every box above against the real tracking sheet and
   interview notes — do not check a box from memory; cite the specific
   rows/quotes.
2. Report the raw counts for every box, checked or not.
3. If the checklists point in different directions (some CONTINUE
   signals present, some KILL signals also present), say so explicitly
   in `analysis-report-template.md` rather than picking whichever
   narrative is more convenient — a mixed result is itself a real,
   reportable finding.
4. The final CONTINUE/ITERATE/PIVOT/STOP recommendation in the analysis
   report is made by weighing these checklists against the actual
   evidence gathered — it is not automatically implied by a majority of
   checked boxes on either side.
