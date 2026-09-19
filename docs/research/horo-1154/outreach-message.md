# HORO-1154 — Outreach message template

Honest, concise, no overstatement of maturity. Libra is a **v0.0.1
Developer Preview** — say so plainly rather than implying a finished
product.

## Direct message / email template

```
Subject: 20-minute ask — testing a local cost/time governor for Claude Code

Hi <name>,

I'm building Libra, a local tool that sits in front of Claude Code (and
soon Codex): before it spends tokens on a task, Libra estimates the
cost and time-to-complete, and it tracks replanning as a deliberate,
visible decision instead of something that just happens silently.

It's an early v0.0.1 developer preview, not a finished product — I'm
looking for a small number (10-20) of real power users who already use
Claude Code or Codex for recurring, non-trivial engineering work to try
it on their own real work for about two weeks, and then tell me honestly
whether it helped, got in the way, or you just stopped using it.

What this involves:
- Installing a local CLI + daemon (open source, runs entirely on your
  machine — nothing about your prompts, code, or tool output leaves
  your machine as part of how this works).
- Using it for real work, on your own schedule, for about two weeks.
- One short check-in conversation, plus (only if you're comfortable)
  running a local, opt-in export tool that packages a few coarse usage
  counts — never your prompts or code — that you'd review and choose
  whether to send back.
- Full participation is genuinely opt-in and reversible at every step —
  you can stop at any time, and nothing is collected without you
  explicitly consenting to it first.

If that sounds interesting, I'd love 20 minutes to walk you through it.
No pressure either way — even a "I tried it and immediately stopped
because X" is exactly the kind of honest signal I need.

Thanks,
<founder name>
```

## What NOT to say (guardrails)

- Do not call this "production-ready," "enterprise-grade," or imply
  team/Codex features exist yet — they don't (v0.0.1 scope is the local
  Claude Code flow only).
- Do not promise payment, free credits, or swag unless that is actually
  true and approved separately — this template makes no such promise.
- Do not imply participation is anonymous by default — consent language
  in `consent-language.md` is explicit about what is/isn't identified.
- Do not undersell the ask either — be upfront that this requires real
  installation and real usage, not a five-minute demo.

## Short version (for a DM/Slack context where brevity matters)

```
Hey — I'm testing an early local tool that estimates cost/time before
Claude Code spends tokens on a task, and makes replanning explicit. It's
a rough v0.0.1 developer preview. Would you be up for trying it on real
work for ~2 weeks and telling me honestly if it helped or got in the
way? 20 min to walk you through it if you're interested — no obligation.
```
