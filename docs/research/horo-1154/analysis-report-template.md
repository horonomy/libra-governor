# HORO-1154 — Analysis / report template

**Fill this in only after real evaluator data exists.** Every section
below is a `<placeholder>` — do not pre-fill any of them speculatively,
and do not invent example numbers "just to show the shape." An agent or
person resuming this ticket should populate this template from the real
tracking sheet (`tracking-sheet-template.csv`) and real interview notes
only, cross-checked against `minimum-evidence-threshold.md`.

---

## 1. Recruitment funnel

Report raw counts at every stage — do not skip a stage because it's
unflattering.

| Stage | Count |
|---|---|
| Outreach sent | `<placeholder>` |
| Responded | `<placeholder>` |
| Passed qualification checklist (`recruitment-criteria.md`) | `<placeholder>` |
| Completed consent (`consent-language.md`) | `<placeholder>` |
| Successfully installed + healthy `doctor` | `<placeholder>` |
| Completed the full evaluation window | `<placeholder>` |
| Dropped off (with reasons categorized) | `<placeholder>` |
| Ran `evidence-report consent` | `<placeholder>` |
| Sent back an evidence export | `<placeholder>` |
| Completed the end-of-window interview | `<placeholder>` |

Recruitment target was 10–20 qualified participants
(`recruitment-criteria.md`). Actual qualified count reached:
`<placeholder>`. If below 10, state that explicitly here as a
confidence limitation on everything below.

## 2. Per-evaluator summary table

One row per participant who at least started the evaluation window —
including drop-offs. Do not omit anyone from this table.

| Participant ID | Qualified profile summary | Install/doctor healthy | Window completed | Export received | Key aggregate counts (preflights/admits/denies/replans/completed) | Drop-off? (reason) |
|---|---|---|---|---|---|---|
| `<placeholder>` | `<placeholder>` | `<placeholder>` | `<placeholder>` | `<placeholder>` | `<placeholder>` | `<placeholder>` |

## 3. Aggregate behavioral findings

Computed only from real exported `EvidenceAggregates` data
(`aggregates.*` fields in each participant's export), never estimated:

- Total preflights across all participants: `<placeholder>`
- Admission split (admit / deny / approval-required / unrecorded):
  `<placeholder>`
- Total replans: `<placeholder>`
- Completed tasks / total execution receipts: `<placeholder>`
- Any notable per-participant outliers, with the participant ID and the
  real number: `<placeholder>`

## 4. Willingness-to-pay signals (verbatim, not paraphrased)

List each participant's actual typed answer to the willingness-to-pay
question — **verbatim**, not summarized into a fabricated yes/no. If a
participant declined to answer, say so rather than omitting the row.

| Participant ID | Verbatim WTP answer |
|---|---|
| `<placeholder>` | `"<placeholder — exact quote>"` |

## 5. Qualitative themes

Synthesize real recurring themes from the interview notes
(`interview-followup-questions.md`) — cite which participants said what
rather than presenting an unattributed summary. Cover at minimum:

- Perceived friction / admission latency: `<placeholder>`
- Replan usefulness or annoyance: `<placeholder>`
- Second-use / repeat-use behavior: `<placeholder>`
- Would-route-more-work-through-it signal: `<placeholder>`
- Team / shared-policy demand: `<placeholder>`
- Codex / cross-agent demand: `<placeholder>`

## 6. Minimum-evidence-threshold checklist result

Re-run `minimum-evidence-threshold.md`'s two checklists against the real
data gathered above, with raw N/M counts for every item:

### CONTINUE / pull-signal checklist

- [ ] Not routinely bypassed for being slow/annoying — `<placeholder: N/M, evidence>`
- [ ] Several users recognize a recurring cost/time predictability problem — `<placeholder: N/M, evidence>`
- [ ] Concrete pull for team/shared policy, cross-agent support, or paid discussion — `<placeholder: N/M, evidence>`

### ITERATE / PIVOT / KILL signal checklist

- [ ] Strong-pain users try once and don't return — `<placeholder: N/M, evidence>`
- [ ] Most users prefer free historical tracking and ignore governance — `<placeholder: N/M, evidence>`

## 7. Final recommendation

**To be completed only after real evaluator data exists — do not fill
this in speculatively.**

State one of: **CONTINUE**, **ITERATE**, **PIVOT**, or **STOP**, with
the reasoning tied explicitly to the checklist results and raw counts
above, not to intuition. Include:

- The recommendation: `<placeholder>`
- The strongest evidence for it (cite specific participants/quotes):
  `<placeholder>`
- The strongest evidence against it, stated honestly rather than
  omitted: `<placeholder>`
- Confidence level given the actual sample size reached vs. the 10–20
  target: `<placeholder>`
- If CONTINUE: what the next concrete scope should be
  (team/shared-policy vs. Codex support vs. paid-discussion track) and
  why the evidence points there specifically: `<placeholder>`
- If ITERATE/PIVOT: what should change before re-testing: `<placeholder>`
- If STOP: what should happen to existing evaluators/commitments:
  `<placeholder>`
