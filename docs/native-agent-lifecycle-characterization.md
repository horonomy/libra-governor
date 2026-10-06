# Native agent lifecycle characterization

**Status: characterization only.** This records what the current adapters
implement and what the hosts document. Parser tests use synthetic, explicitly
non-sensitive values; they prove translation behavior only. They do not prove
that a native host emitted a payload or that identity survives a lifecycle
transition.

The Codex host contract follows the [Codex Hooks documentation](https://developers.openai.com/codex/hooks).
It documents `SessionStart` sources `startup`, `resume`, `clear`, and
`compact`; `SubagentStart`/`SubagentStop`; the parent session ID in subagent
hooks; and Codex-specific `turn_id`/`agent_id` fields for subagent hooks.
The current [Claude Code hooks reference](https://code.claude.com/docs/en/hooks)
documents those session sources plus `fork` (since v2.1.214), as well as
`SubagentStart`/`SubagentStop` and `agent_id` in subagent contexts. Neither
documented hook schema supplies `parent_agent_id`. Both hosts document
`transcript_path`; Codex warns that transcript format may change, and Claude
Code notes that the transcript file is written asynchronously and may lag.
This characterization does not use either transcript to infer lineage.

## Current support matrix

| Lifecycle case | Host contract / evidence | Libra behavior in this worktree | Characterization |
|---|---|---|---|
| New session | Codex and Claude Code document `SessionStart.source = startup`. | Only `UserPromptSubmit`, `PostToolUse`, and `Stop` are installed. `SessionStart` is recognized as unwired; no identity is captured from it. | Host event shapes documented; Libra handling tested with a synthetic payload. Native end-to-end acquisition is unverified. |
| Resume | Both hosts document `SessionStart.source = resume`. | `SessionStart` remains unwired. No persisted mapping binds a resumed session to a prior Libra identity. | Host event shapes documented; whether either host preserves its session ID across resume is unverified. |
| Clear | Both hosts document `SessionStart.source = clear`. | `SessionStart` remains unwired. Libra does not reset or reconcile identity on clear. | Host event shapes documented; provider session ID and host state reset behavior are unverified. |
| Fork | Claude Code documents `SessionStart.source = fork`; Codex's documented start sources omit `fork`. | No fork event or parent-agent field is captured. | Claude hook availability is documented, but fork identifier relationships are unknown; Codex fork semantics are undocumented here. No lineage is inferred. |
| Subagent | Both hosts document `SubagentStart` and `SubagentStop` with `agent_id`; Codex also documents `turn_id` and explicitly says subagent hook `session_id` is the parent session ID. | These hooks are not installed and normalize as recognized-but-unwired. The identity builder preserves only IDs supplied to a wired event and always sets lineage to `Unknown`. | Parser behavior is tested with synthetic fields. Native lifecycle acceptance and parent-agent lineage are unverified. |
| Restart | Both hosts distinguish `startup` and `resume`, but their hook contracts do not identify an operating-process restart as a separate source. | No restart hook is installed. Libra's host ID is persisted locally; provider session continuity is owned by the host. | Host session continuity across process restart is unknown. |
| Concurrent terminals | Neither hook contract establishes whether independent terminal sessions share or fork a provider session. | First creation of Libra's `host_id` now uses a complete private temporary file and atomic no-overwrite publication, so concurrent callers in the same state directory resolve the winner's ID. | The resolver has a synchronized thread test. Native session separation and lifecycle behavior across terminals remain unverified. |

## Evidence boundary

The adapter has three wired entry points: prompt submit, post-tool use, and
stop. The normalizer returns `RecognizedUnwired` for `SessionStart`,
`SessionEnd`, `SubagentStart`, `SubagentStop`, `Interrupt`, `PreCompact`, and
`PostCompact`; it does not relay their extra fields to the identity builder.
The capture function preserves only the host ID, provider session ID, and
optional agent/turn IDs it is given. It never derives parentage.

The earlier Codex smoke evidence recorded in
[`integrations/codex/README.md`](../integrations/codex/README.md) was for a
`UserPromptSubmit` task receipt, not these lifecycle transitions. The separate
Codex native positive-control attempt in HORO-1714 was not accepted as
verification. Per that decision, this characterization does not repeat the
host execution or promote parser fixtures to native evidence.

The barrier test establishes same-process concurrent first-use agreement for
`host_id`; it does not exercise independent OS processes, a Codex host, or
provider session continuity. An existing empty `host_id` file is treated as
invalid and left untouched so a failed prior write cannot be silently
overwritten.

## Concrete blocker for native AC2 evidence

Both host CLIs are installed locally: Claude Code `2.1.274` and Codex CLI
`0.160.0`. The current official hook references describe the required hook
families, so local CLI absence is not the blocker. Their schemas cannot prove
the behavior AC2 asks to characterize:

- For **new**, both document a startup event, but no accepted capture shows
  the installed Libra hooks receiving it.
- For **resume** and **clear**, both document event source values, but not
  whether provider session IDs or Libra task bindings persist or reset.
- For **fork**, Claude Code documents a fork source, while Codex does not
  document a fork source; neither schema establishes parent-child session or
  agent identity relationships for the exact acquisition.
- For **subagent**, both document an `agent_id`; Codex says the subagent hook's
  session ID is the parent session ID. Neither documents a parent agent ID or
  proves that the installed hook receives and preserves the same identifiers
  through a native subagent lifecycle.
- For **restart**, neither distinguishes a process restart as its own lifecycle
  event or guarantees session ID continuity after it.
- For **concurrent terminals**, neither schema defines cross-terminal session
  relationships. The host-ID barrier test only covers in-process concurrent
  resolver calls.

HORO-1714's Codex native positive-control attempt was not accepted, and its
recorded decision is not to retry real host execution. No preserved accepted
native artifact for the same harmless input and these seven cases is available
in this worktree. Synthetic parser tests and documentation cannot close this
runtime evidence gap.

**Owner action required to close AC2:** identify an accepted, non-billable
native test harness or separately authorize a controlled local capture that
supersedes the no-retry decision. It must use the same inert input for all
cases and retain only host/version, event name, and supplied identity fields
needed to check continuity; prompts, transcripts, and tool payloads are not
needed. Until that authority and evidence source exist, AC2 remains externally
blocked while this branch can report only implementation and schema facts.
