# Libra Governor in a composable statusline

Claude Code gives a scope exactly one `statusLine.command`. That is the whole
reason this document exists: Libra is not the only thing worth showing there,
and a product that takes the slot for itself makes every other product — and
whatever the user wrote themselves — disappear.

So Libra does not own the slot. It publishes a **provider document** and lets a
shared host compose it with the user's own statusline and with any other
Horonom product's. The contract is
[`governance/product/statusline-provider-contract.md`](https://github.com/horonomy/.github/blob/main/governance/product/statusline-provider-contract.md)
in `horonomy/.github`.

There are three ways a Libra segment can be on screen today. Two of them are
older than this mechanism and neither is deleted by adopting it.

## The supported surface

```bash
libra-governor statusline provider   # one JSON document on stdout, exit 0 always
libra-governor statusline explain    # the read-only long form, for the host's explain surface
```

`statusline provider` is read-only and host-scoped. It answers from state the
daemon already holds — `Request::Status` and `Request::Doctor` are both
documented as answered without new reconnaissance and without any LLM call — and
it never spawns the daemon. A statusline refreshes on a timer, and a refreshing
statusline that spawns a daemon is a race factory.

It has a 200 ms budget for the whole probe. Both round trips share that one
deadline rather than getting one each, and the `Doctor` half is failure-isolated
from the `Status` half: if the diagnostic is slow or unreachable, the profile
segment is simply absent and the rest still renders.

`statusline explain` has three seconds instead, because there a user has asked a
question and is waiting for the answer.

### What it reports

| Segment | When | What it says |
|---|---|---|
| `task` | always | the task being governed and its replan state, or explicitly that none is |
| `estimate` | while a task is governed | the remaining-work P90 and the confidence *in that estimate* |
| `escalation` | automatic-replan budget spent | that the next material deviation would need a human |
| `profile` | running config no longer matches disk | which policy preset is actually running |

Four segments is the host's per-provider maximum, so this is the whole surface,
not a sample of it.

Spans leave as seconds plus a noun (`P90`), never as `5d4h`: the host owns span
formatting, so two products cannot disagree about what a day is. Glyphs are the
same — the provider emits no emoji at all and the host chooses one, with a
deterministic text fallback.

### What it never reports

Prompts, task or tool content, the estimator's free-text reason, resource or
cost quantiles, a credential, a filesystem path, or an ANSI escape. Every string
in the document is a fixed literal in
[`crates/cli/src/statusline_provider.rs`](../crates/cli/src/statusline_provider.rs)
or a value drawn from a closed set. That is enforced by tests over every
document the provider can emit, not by review.

It is also not a control. Nothing in it lets a reader approve, deny or escalate
anything; `EscalatedAwaitingApproval` is reported as a spent budget, because
nothing is blocked and nothing is waiting on an answer from the reader.

### Enabling it

Enable and disable go through the shared host lifecycle, not by hand-editing
JSON. The host reads the existing `statusLine` object, preserves it whole —
including keys it does not understand — registers your original command as an
upstream provider, and only then routes the slot through the compositor.
Disabling Libra leaves every other provider and your original statusline exactly
where they were.

`libra-governor` itself never writes a shared-host registration. Its `install`
still wires only its own hooks and, if the slot is free, its own legacy
statusline; registering a provider is the host's job and the host's alone.

## Migrating from an external wrapper

### 1. The founder DogFood wrapper

A shell wrapper that calls `libra-governor statusline`, extracts fields from the
plain-text line and composes them with other products' output. This is the
"integration glue, not a product capability" the provider replaces, and it is
where both of the reported readability defects actually lived:

- the broken glyph was a bare U+2696 SCALES emitted by the wrapper. Its East
  Asian Width is `N` and it carries `Emoji_Presentation=No`, so a terminal is
  entitled to draw it as one monochrome column — which is what happened. The
  provider's fix is structural: it emits no glyph, and the host picks one that
  is presentation-safe.
- `pf:high` was assembled by the wrapper from the product's `preflight: high`.
  An unqualified `high` beside a task reads as a risk or priority rating. The
  provider sends `confidence` together with `confidence_of`, and the host
  renders `preflight confidence high`; the contract refuses a bare confidence
  outright, so the ambiguity cannot come back.

To migrate: enable Libra through the host lifecycle, then stop calling the
wrapper from `statusLine`. Nothing deletes the wrapper for you, and nothing
requires you to remove it — the bare `statusline` command it depends on keeps
working (see below).

### 2. `libra-governor install`'s own `statusLine` entry

If `install` wired `~/.claude/settings.json`'s `statusLine` to `libra-governor
statusline`, that entry is a *manual historical configuration* in the shared
host's terms. Enabling Libra through the host registers it as your upstream
command, so it keeps rendering as it does today, composed rather than replaced.

Run `libra-governor doctor` to see which of the two is wired. Nothing here is
removed automatically: a supported mechanism existing is not a reason to delete
configuration a user chose.

### 3. A project-local statusline

Claude Code's project scope **replaces** the global `statusLine` rather than
merging with it, so a project-local entry — including one in
`.claude/settings.local.json` — silently wins wherever it applies. If you have
one, the host's `doctor` will report which scope owns the slot. Libra does not
touch project-scoped files, and no Libra command deletes
`.claude/settings.local.json`.

## The legacy `statusline` command

`libra-governor statusline` still prints today's line, unchanged:

```
libra: task 1a2b3c4d | plan 9f8e7d6c | preflight: high | recon: 2.4s | remaining P90: 447120s | escalated — awaiting approval
```

It is byte-for-byte what it was, deliberately: the founder's wrapper parses its
exact phrases, and breaking it to make a migration tidier would break a working
setup to serve an internal preference.

It is, however, on a migration path to retirement. New setups should use
`statusline provider`. The legacy line cannot be composed with anything, cannot
be degraded to fit a narrow terminal, cannot express "unknown" distinctly from
"nothing", and formats its own spans and its own labels — all four of which the
provider contract exists to fix.

No removal date is set here. Retiring it is a product decision for a later
ticket, and it will not happen silently.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| The Libra segment is absent entirely | The provider is not registered with the host, or the host is not in the slot | The host's `doctor` reports which; it is read-only and safe to run |
| The segment says the state could not be read | The daemon is not running — the provider never spawns one by design | Submit a prompt; `hook user-prompt-submit` spawns it on demand |
| A `Running policy ...` segment appeared | You edited `config.json` after the daemon started, so the edit has not taken effect | `pkill -f "libra-governor daemon run"`; the next hook invocation respawns it |
| The estimate says no local history yet | A genuine cold start — the estimator has no comparable local receipts | Nothing to fix; it resolves as receipts accumulate. `statusline explain` names the basis it did use |
| No confidence is shown beside the estimate | There is no P90 to be confident *in* | Expected: a confidence attached to an absent number would read as a verdict on the task |

For anything about the slot itself — who owns it, what a drift report means, how
uninstall restores your original — see the host's own documentation. This
document covers only Libra's side of the contract.
