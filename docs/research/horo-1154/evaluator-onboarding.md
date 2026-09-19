# HORO-1154 — Evaluator onboarding

Step-by-step instructions to hand a qualified, consented evaluator.

## Before you start

- You should already have read and agreed to `consent-language.md` with
  the founder (or whoever recruited you).
- This installs real software that runs alongside Claude Code on your
  machine. Use a machine you actually do real engineering work on.

## Step 1 — Install Libra

Follow the "Install" and "Quickstart (5 minutes)" sections of the
top-level [`README.md`](../../../README.md) in the `libra-governor`
repository. In short:

```bash
cargo build --workspace --release
libra-governor install
```

Then verify it's wired up correctly:

```bash
libra-governor doctor
```

You should see `hooks and statusline wired` with no error-severity
findings. If you see an error, see the "Troubleshooting" table in the
README before continuing — this evaluation only produces useful signal
if the install is actually healthy.

## Step 2 — Give explicit consent for the evidence export (separate from installing)

This is a second, separate consent step from installing the tool —
installing it does not imply you've agreed to export anything.

```bash
libra-governor evidence-report consent
```

This just records a local timestamped marker. It does not collect or
send anything by itself.

## Step 3 — Use it for real work

Use Claude Code (and Codex, if you use it) as you normally would for
about **two weeks** (see `study-procedure.md` for exactly why two
weeks, and what to do if that's not realistic for your schedule). Don't
change your workflow to "test" Libra artificially — the honest signal
this evaluation needs is what happens when you use it the way you'd
actually use it.

A few things worth paying attention to as you go, since you'll be asked
about them later (see `interview-followup-questions.md`):

- Did the preflight/replan interruptions feel useful, or did you find
  yourself working around them?
- Did you keep using it, or did you quietly stop and just use Claude
  Code/Codex directly?
- Did anything about cost/time visibility change how you worked?

## Step 4 — Before your scheduled check-in, run the export

```bash
libra-governor evidence-report
```

This will:
1. Refuse if you haven't run the consent step above.
2. Print a handful of open-ended questions and wait for you to type an
   answer to each (press Enter to skip any you don't want to answer).
3. Write two local files (a `.json` and a `.md`) under
   `~/.local/state/libra-governor/evidence-reports/` and print their
   paths.

**Nothing is sent automatically.** Open the `.md` file, review it
yourself, and only if you're comfortable with what's in it, send it to
the founder (e.g. as an email attachment) ahead of or during your
check-in.

## Step 5 — Check-in conversation

Per `study-procedure.md`'s cadence. Bring the exported file if you're
comfortable sharing it; either way, the conversation itself (using
`interview-followup-questions.md` as a starting point) is the primary
evidence — the export is supporting data, not a replacement for talking
to you.

## If you want to stop at any point

Just stop. Uninstall with `libra-governor uninstall --yes` if you want
to fully remove it. No explanation needed, and — per
`consent-language.md` — this itself (a strong-pain user tries it and
doesn't come back) is exactly the kind of signal the evaluation needs
to capture, not something to feel bad about.
