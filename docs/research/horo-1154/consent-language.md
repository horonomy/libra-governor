# HORO-1154 — Consent language

This is the informed-consent text an evaluator should read and
explicitly agree to (verbally or in writing — see `study-procedure.md`
for how agreement is recorded) before installing Libra and, separately,
again before running `libra-governor evidence-report consent`. It
mirrors exactly what `crates/cli/src/evidence_report_cmd.rs` actually
does — do not let this document drift from the code's real behavior.

---

## What you're being asked to do

You're being asked to install and use Libra (a local tool that sits in
front of Claude Code) on your own real engineering work for about two
weeks, and then optionally share a small, local export of coarse usage
signals plus your own written feedback with the founder.

## What data is collected, and when

**Nothing is collected automatically.** Libra's hooks and daemon run
entirely on your machine and store data (task/plan/contract structure,
cost/time estimates, execution receipts, admission decisions) in a
local SQLite file under `~/.local/state/libra-governor/`. None of that
is ever transmitted anywhere by Libra itself — see the "Security &
privacy" section of the top-level `README.md`.

A separate, entirely opt-in step exists for this evaluation
specifically: running `libra-governor evidence-report consent` (which
just records your consent locally, with a timestamp) and then
`libra-governor evidence-report`, which builds one local export
containing exactly:

- **Coarse counts**: how many tasks/preflights you ran, how many were
  admitted vs. denied, how many replans happened, how many tasks you
  completed. These are aggregate numbers only — never the text of any
  prompt, any file path, any source code, or any tool output.
- **Integration wiring facts**: whether Claude Code's hooks/statusline
  were wired in at the time of export.
- **Your own typed answers** to a handful of open-ended questions about
  your experience (friction, whether you'd use it more, team/Codex
  demand, willingness to pay). This is exactly what you choose to type
  — nothing is inferred or filled in on your behalf.

That export is written to two local files (JSON and Markdown) on your
own machine. **Nothing is sent anywhere by this tool.** You decide
whether, when, and how to send either file to the founder (e.g.
attaching the Markdown file to an email).

## What is explicitly never collected

Prompt text, source code, file contents, file paths, or raw tool output
— none of this is ever in the local ledger to begin with (it's not a
policy choice this export layer makes; the underlying data store has no
column for any of it), so it cannot appear in the export either.

## How the data will be used

Solely to help the founder decide whether to invest further in
team/shared-policy or Codex support for Libra (Jira ticket HORO-1154).
It will not be sold, shared with third parties, or used for any
marketing purpose without asking you again, specifically, first.

## How to withdraw

You can stop using Libra, decline to run `evidence-report`, or decline
to send an already-generated export at any point, for any reason, with
no obligation to explain why.

To revoke your local consent yourself, at any time, without contacting
anyone: delete `~/.local/state/libra-governor/evidence_consent.json` (or
run `libra-governor uninstall --yes` to remove Libra and all of its
local state entirely). Once that file is gone, `evidence-report` refuses
again until you re-run `evidence-report consent`.

If you've already sent an export to the founder and want it deleted from
their side, contact them at <contact email/channel> and it will be
deleted.

## Questions or concerns

<contact path — email or channel> is the reporting/contact path for any
question about this evaluation, including security concerns. See also
`SECURITY.md` in the repository for the project's general security
contact process.

## Your explicit agreement

By proceeding, you confirm that you have read the above, understand
that participation and evidence export are both entirely optional and
reversible, and consent to installing Libra for the evaluation and (only
if and when you separately run `evidence-report consent`) to generating
a local export as described.

<space for evaluator's name/date/signature or equivalent recorded
acknowledgment — see `study-procedure.md` for how this is tracked>
