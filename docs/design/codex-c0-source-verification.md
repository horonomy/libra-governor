# Codex hooks vs rollout ids: source verification (C0)

> **Status: source-derived findings, HORO-2280 (C0).** Read from the public openai/codex source at tag `rust-v0.160.0` (commit a956835d020762cb2b570053af06f643a11c0ecc) by a read-only agent; the two load-bearing id claims (hook and rollout `turn_id` and `session_id` share one source) were independently re-checked by the owner at that commit. Nothing was executed and no real Codex host was run: runtime behaviour is **not** verified. "codex-cli 0.160.0" is assumed to mean this tag.

## Consequences for the Codex budget gate design (docs/design/codex-shared-budget-gate.md)

- **Id risk retired by source (not by run):** hook `turn_id` equals the rollout `token_usage_record.turn_id`, and hook `session_id` equals the record `session_id`. C2's measured path may proceed; it still needs fixture and, later, real-host evidence before any claim.
- **`session_id` is the shared root identity, not `thread_id`.** Subagent hooks carry the parent's `session_id` plus `agent_id`; the design already refuses to admit `agent_id` turns.
- **Stop and UserPromptSubmit can fire more than once per turn with the same `turn_id`** (steered input, Stop-block continuation). `SettleTurn` must be idempotent per turn and the C4 true-up must book only later deltas.
- **Subagent usage lives in the child's own rollout** (`agent_transcript_path`), never in the parent's. Subagent spend stays NOT COVERED (C6) with no double-count risk against the parent rollout.
- **Block semantics are source-confirmed:** a blocked prompt is not recorded in history and the model request is skipped when no other input is accepted. Lifecycle events for a blocked turn are UNKNOWN.
- **Flush ordering is inferred, not tested:** the final usage record is very likely on disk when Stop runs, but a runtime probe is still required before the under-count residual is dropped.


Tag: rust-v0.160.0 (exists; tag object 79b1b666f2e8551f8abbbca34957227f67f3f553; commit a956835d020762cb2b570053af06f643a11c0ecc).
Note: rust-v0.160.1 also exists (not used). Paths below are repo-relative; all under codex-rs/. Source read from the tag tarball only; nothing executed.
"codex-cli 0.160.0" is assumed to correspond to rust-v0.160.0 (not independently verified).

## Q1. hook turn_id / session_id source
- turn_id = `turn_context.sub_id` for both UserPromptSubmit and Stop. session_id = `sess.session_id()` = `services.agent_control.identity()` (root-shared id for root + all descendant threads; NOT thread_id).
- core/src/hook_runtime.rs:684-690 (UPS), :448-452 (Stop); core/src/session/session.rs:651-654.
```
UserPromptSubmitRequest { session_id: sess.session_id().into(), turn_id: turn_context.sub_id.clone(), ...
StopRequest { session_id: sess.session_id().into(), turn_id: turn_context.sub_id.clone(), ...
pub(crate) fn session_id(&self) -> SessionId { self.services.agent_control.identity() }
```
- Same turn_id UPS vs Stop within one turn: YES by source (same TurnContext.sub_id; Stop is invoked at core/src/session/turn.rs:655 within the same turn loop). Caveat: UPS can fire more than once per turn (steered input, turn.rs:439-446) with the same turn_id; Stop can fire multiple times (stop_hook_active continuation, turn.rs:672-689). Test core/tests/suite/hooks.rs:1378-1396 asserts stop turn_ids non-empty/consistent.

## Q2. rollout token_usage_record field sources
- core/src/session/mod.rs:4788-4799 (record_observed_response_completed), core/src/state/session.rs:215-249, protocol/src/protocol.rs:2264-2273.
```
record_token_usage(self.thread_id, &turn_context.sub_id, self.session_id(),
  turn_context.turn_metadata_state.root_turn_id().unwrap_or_else(|| turn_context.sub_id.clone()), response_id, usage)
```
- thread_id = Session.thread_id; turn_id = turn_context.sub_id; session_id = same agent_control identity as hook session_id; root_turn_id = turn metadata root_turn_id, falling back to sub_id (root turn -> equals turn_id).
- hook turn_id == rollout turn_id: YES (both turn_context.sub_id). hook session_id == rollout record session_id: YES (same fn). hook session_id == thread_id: NOT guaranteed; equals root thread's id (session.rs:909-910 comment "session_id is equal to the root thread's ID") so equal for root threads, different for subagent threads. Resumed legacy subagent rollouts: session.rs:904-907 special-case.

## Q3. UserPromptSubmit block / exit 2
- hooks/src/events/user_prompt_submit.rs:199-209 (decision block) and :227-243 (exit 2 + stderr reason; exit 2 with empty stderr = Failed, not blocked). hooks/src/engine/output_parser.rs:266-288 (block with empty reason = invalid, not blocked). Only Sync hooks apply control effects (engine/mod.rs:154).
```
} else if parsed.should_block { status = HookRunStatus::Blocked; should_stop = true; stop_reason = parsed.reason.clone();
Some(2) if handler.can_apply_control_effects() => { if let Some(reason) = trimmed_non_empty(&run_result.stderr) { status = Blocked; should_stop = true;
```
- Effect: core/src/session/turn.rs:883-913 + :368-378: blocked input is NOT passed to record_pending_input (not recorded in history); if no other accepted user input, function returns true and run_turn returns `Ok(None)` before any sampling request => model request skipped. Additional context from the blocking hook is still recorded (turn.rs:889). Mixed batch: other accepted inputs still go through (test core/tests/suite/hooks.rs:2819). Not verified: whether TurnStarted/TurnComplete lifecycle events still appear in rollout for blocked turn (UNKNOWN).
- `continue:false` also sets should_stop (:183-186), same skip path.

## Q4. Subagent usage persistence and hooks
- Subagents (ThreadSpawn) are separate threads with their own Session/rollout (own thread_id, own sub_ids). Their record_observed_response_completed persists TokenUsageRecord via the child's own live_thread (session/mod.rs:4799). No code found copying child usage into the parent rollout (not exhaustively proven; UNKNOWN for aggregation elsewhere, e.g. app-server).
- root_turn_id propagation: spawn handler passes `turn.turn_metadata_state.root_turn_id()` (core/src/tools/handlers/multi_agents/spawn.rs:132) -> TurnStartOptions -> set_root_turn_id (core/src/session/turn_input.rs:185-188). root_turn_id() is `OnceCell.get()` = None unless set (turn_metadata.rs:359-361). So for a plain CLI root turn it is None, and the child record's root_turn_id falls back to the CHILD's own sub_id (inference from source, not run). Do not assume child root_turn_id == parent turn_id; UNKNOWN whether app-server clients set it.
- Hooks for subagent turns: UserPromptSubmit fires (hook_runtime.rs:677-697) with `subagent: thread_spawn_subagent_hook_context` -> payload agent_id = child `sess.thread_id()` and agent_type = role (hook_runtime.rs:1054-1061); session_id = shared root identity. Stop does NOT fire as "Stop" for ThreadSpawn children: hook_runtime.rs:400-433 dispatches SubagentStop (agent_id, agent_type, agent_transcript_path = child rollout) with transcript_path = PARENT thread's rollout path. Other SubAgent sources (internal/synthetic): no Stop at all (:434-436). Compare to MemoryConsolidation: runs Stop with own transcript.

## Q5. Stop hook vs final token_usage_record flush
- Order in turn loop: response.completed -> record_observed_response_completed (turn.rs:2954-2960) -> persist_rollout_items = live_thread.append_items awaited (mod.rs:4542-4550) -> later `!needs_follow_up` -> run_turn_stop_hooks (turn.rs:653-661). So the record is enqueued before Stop runs.
- Writer is async: append sends RolloutCmd::AddItems into an mpsc channel (rollout/src/recorder.rs:1035-1047), writer task adds to pending and flushes if materialized (:1942-1945). run_turn_stop_hooks computes transcript_path via hook_transcript_path() (hook_runtime.rs:445, mod.rs:5078-5090) which calls ensure_rollout_materialized -> RolloutCmd::Persist awaited via ack (recorder.rs:1946-1948); channel is FIFO so earlier AddItems are written before Persist acks. Inference: record is on disk before the Stop command spawns (no explicit flush_rollout call there; explicit flush happens for SessionEnd, hook_runtime.rs:496-498). Caveat: writer I/O failure leaves items pending (recorder.rs:1768-1772), and a Stop-block continuation yields further records after the hook. Status: source-derived, not tested at runtime.

## Q6. transcript_path vs rollout file
- transcript_path = `sess.hook_transcript_path()` = `current_rollout_path()` of the same session (mod.rs:5078-5090), i.e. the live thread's rollout JSONL that receives RolloutItem::TokenUsageRecord (same live_thread). YES for UPS and root Stop. Exceptions: None if thread store is non-local/no path (test session/tests.rs:11372-11377); SubagentStop uses the PARENT's rollout path as transcript_path and the child's as agent_transcript_path (hook_runtime.rs:404-432), so for subagents the token records live in agent_transcript_path, not transcript_path. Rollout file lives under ~/.codex/sessions/YYYY/MM/DD (recorder.rs:1727-1741).
