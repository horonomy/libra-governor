> **Status: design proposal, not shipped.** HORO-2261 (S11 gateway governor_held). Line numbers refer to main aea0894. Nothing here is approved for enablement: switching on any enforcement mode, its guarantee label and every item listed as Level 2 need an explicit founder decision. READY_FOR_RELEASE stays false.

# S11 design: gateway per-request hard cap against the local monthly shared budget

Repo: libra-governor @ origin/main aea0894 (read-only review, no builds run).
Legend: **[V]** verified in code/docs at the cited file:line; **[I]** inference, not verified;
**[H]** host assumption, needs real-host or fake-upstream evidence.
All paths are relative to the repo root.

---

## 0. Decision (lead)

1. **Build S11 as a dark, opt-in "replace" mode.** `shared-budget.json` gets an optional
   `"enforcement": "hooks" | "gateway"` field (default `hooks`, `schema_version` stays 1).
   In `gateway` mode the gateway reserves every `POST /v1/messages` against the monthly pool
   *before* forwarding, settles from provider usage, and is the **only** writer of spend for the
   pool. The hook charge paths book nothing. This follows ADR-0003 §4: per-request reservations
   replace the envelope and never stack (docs/adr/0003-gateway-enforcement-boundary.md:113-124).
2. **The hard claim is "Libra-accounted, modelled worst case, gateway-routed".** It is not a bill
   guarantee and it is not tamper-proof against the same OS user. Wording is in §1.4.
3. **Five correctness fixes are prerequisites.** Without them the cap is not hard:
   - truncated streams settle at the reservation, not the partial observation;
   - expired or orphaned gateway rows are booked at the reserved amount, never released;
   - lane and turn queries exclude gateway rows;
   - requests whose input cannot be bounded from the body are refused;
   - one pricing table prices both reserve and settle.
4. **Founder (Level 2) items are listed in §5.** These are enabling the mode or claiming the
   label, the change in counting scope, and refusing server tools (WebSearch). Everything else
   can be built off by default with no change to shipped behaviour.

---

## 1. What is enforceable vs only estimated

### 1.1 What the gateway knows before forwarding [V]
- Body byte length, `model`, `max_tokens`, `stream`. Only these three fields are parsed today
  (crates/gateway/src/proxy.rs:308-323).
- The input upper bound is `ceil(bytes / 3.0)` (crates/gateway/src/cost.rs:42, 73-75). Input is
  priced at `max(input, cache_creation_5m, cache_read)` (crates/gateway/src/pricing.rs:68-72).
  Output is priced at `max_tokens × output` (cost.rs:114-117). Cents are rounded up
  (cost.rs:80-85).
- No positive `max_tokens` means a refusal (cost.rs:99-101, proxy.rs:924-927). An unpriced model
  against USD means a refusal (cost.rs:109-113).

### 1.2 What the gateway knows only after the response [V]
- Exact per-tier usage from `message_start.message.usage` and the cumulative
  `message_delta.usage` (crates/gateway/src/usage.rs:15-23, 167-212).
- The response `message.id` is present in `message_start`. The fixture shows
  `"id":"msg_fake"` (crates/gateway/tests/fake_upstream.rs:90) [V for the fixture, I for real
  hosts].
- `completed_cleanly()` is true only once `message_stop` has been seen (usage.rs:233-238).
  **`pump_and_settle` never consults it.** It settles at `observed()` whenever any usage was
  seen (proxy.rs:1234-1243). A test pins this behaviour:
  `a_stream_cut_off_mid_flight_settles_at_the_last_observed_usage`
  (crates/gateway/tests/proxy_lifecycle.rs:472).
  - On a client disconnect or upstream abort after `message_start`, output is under-counted. In
    the fixture, `message_start` carries `output_tokens: 1` (fake_upstream.rs:90, 115) [V].
    Whether real streams send an intermediate cumulative `message_delta` is [I].
  - **This defeats a hard cap.** Claude Code's ESC interrupt is a client disconnect [I].

### 1.3 Where the "worst case" is not a real upper bound
| Gap | Effect vs reservation | Evidence | S11 treatment |
|---|---|---|---|
| Truncated stream settles at partial usage | under-count | proxy.rs:1234-1243 [V] | Shared path: settle = exact input tiers from `message_start` + `max_tokens × output`, or the full reservation if `message_start` was not seen; `usage_known=false` (S11b) |
| Input tokens > `bytes/3` (dense Unicode or emoji, the API's tool-use system prompt) | actual > reserved, an overrun | cost.rs:37-42 calls 3.0 "deliberately low" but it is a heuristic [V]; tokenizer behaviour [I] | Hard mode uses `bytes / 1.0` (byte-level BPE gives ≥1 byte per token [I]) plus a per-image constant and a tool-prompt constant. Detect and record `input_bound_violated`, mirroring `bound_violated` (cost.rs:172-174) |
| Inline images and documents (base64) are billed by rendering, not bytes [I] | either direction. Base64 screenshots inflate the reservation, so R2 is understated. A small many-page PDF can exceed the bound, which is an overrun | body is not inspected (proxy.rs:308-323) [V] | Hard mode excludes `source.data` payload bytes from the byte bound and adds per-block constants (an image maximum and a per-page PDF maximum, values [I] until S11a/API docs). Inventory both in S11a |
| Content not in the body: `source.type` url/file, Files API, `mcp_servers`, `container`, server tools (`web_search_*`, `web_fetch_*`, `code_execution_*`) | unbounded input, plus per-use fees not in `usage` | body is not inspected beyond 3 fields (proxy.rs:308-323) [V]; field names [I] | Hard mode refuses with `403 unenforceable_request` (a structural check; nothing is retained). **This breaks Claude Code WebSearch [I]**, which is a founder item |
| 1-hour cache-write tier (2× base input, above the 1.25× 5-minute rate) | the reserve under-states the bill; the settle also under-states it (`ephemeral_1h` is not parsed, usage.rs:79-85) | pricing.rs:90-93, ADR-0026:129-131 [V] | Libra-accounted totals stay ≤ cap because reserve and settle use the same model. This is bill drift, not a Libra overrun. Optional S11h models it and bumps `PRICING_VERSION` |
| Fast mode, `inference_geo` 1.1×, long-context premium | bill drift | ADR-0026:132-133 [V] | Disclosed residual. S11a inventories the field names |
| Haiku 5.5 short prompts | over-count by about 5× | pricing.rs:195-199 [V] | Conservative, so no action |
| `output_tokens > max_tokens` | overrun | detected, never clamped (proxy.rs:1250-1253) [V] | Unchanged; reported in Status |
| Stale pinned table | bill drift | pricing.rs:31-41 [V] | Unchanged; `PRICING_VERSION` is recorded on every row |

### 1.4 The exact guarantee S11 can claim
Proposed label constant, separate from the existing one. `SHARED_BUDGET_GUARANTEE =
"admission-bounded-cooperative"` must not be strengthened (crates/protocol/src/shared_budget.rs:4-7, 21) [V].

`SHARED_BUDGET_GUARANTEE_GATEWAY = "gateway-reserved-modelled-worst-case"`

> **Gateway mode (governor_held).** Every `POST /v1/messages` routed through the Libra gateway
> is forwarded only after its modelled worst-case cost is atomically reserved against the
> month's pool. At every admission, settled plus reserved is at most the cap. Spend is
> Libra-accounted, list-price-equivalent USD at the pinned `PRICING_VERSION`. Total spend can
> exceed the cap only when a request's actual exceeds its modelled worst case, which is
> detected and reported (`bound_violated`, `input_bound_violated`). Spend not routed through
> the gateway is **not counted** in this mode. This is not a provider-bill guarantee. Any
> process running as the same OS user can edit `shared-budget.json` or the ledger, so the cap
> is not tamper-proof.

Do not say "hard cap" without the qualifiers "modelled worst case" and "gateway-routed".

---

## 2. Identity

### 2.1 Mapping [V/H]
- The session comes from the configurable header `gateway_session_header`, default
  `x-claude-code-session-id` (proxy.rs:89; crates/daemon/src/server.rs:134-136, 2894) [V].
  - **Nothing in the repo shows that Claude Code sends this header or that it equals the hook
    `session_id` [H].** Treat it the way ADR-0026 treats the unproven `prompt_id` equality
    (docs/adr/0026-local-monthly-shared-budget.md:523-526).
- No agent id exists on the wire [V: the proxy reads only the session header]. Whether subagent
  requests carry a distinguishing header is [H].
  - Gateway rows are therefore **`unattributed` under the session**, using
    `ExecutorAttribution::unattributed(Some(ChargeSource::Gateway))`
    (crates/ledger/src/shared_pool.rs:284-290) [V]. ADR-0026's identity rules apply: a missing
    agent never means root, and an identity is never invented (ADR-0026:153-164).
- No new table is needed. `ChargeSource::Gateway` already exists
  (crates/domain/src/shared_pool.rs:188-210), and the migration 0021 CHECK already admits
  `'gateway'` (crates/ledger/migrations/0021_shared_pool_executor_attribution.sql) [V].
- Per-request rows are **not** tied to the UserPromptSubmit hold. In replace mode there is no
  turn hold (see §3.4). The pool total is what the cap protects, so attribution is
  bookkeeping, not authorization (authority.rs:47-52) [V].

### 2.2 Unattributable requests: recommend fail closed
- Refuse with `403` if the header is missing, empty, over 256 bytes, has a reserved
  `execution-owner-v1:` prefix, or is not valid UTF-8. The existing decision value is
  `task_unbound` (proxy.rs:858-873); add `shared_budget_unattributable`.
- **Why:**
  - It matches ADR-0003 §6, where the gateway's own admission fails closed (0003:151-163).
  - It matches ADR-0026's refuse category (0026:157-160) and `validate_identity`
    (crates/daemon/src/shared_budget.rs:403-418).
  - Booking under a synthetic session would invent an identity, which ADR-0026 forbids.
- **Alternative (rejected):** charge the pool under an explicit unknown session. The cap stays
  hard either way, so this only buys usability, at the cost of an invented identity.
- **Consequence:** if [H] fails and Claude Code does not send the header, gateway mode refuses
  everything. S11a must prove the header before the mode is enabled anywhere.

---

## 3. Reservation and settlement protocol

### 3.1 Where the code lives [V]
- The gateway is a thread inside the daemon process with its own `LedgerStore` connection
  (server.rs:2855-2928; crates/daemon/src/gateway_authority.rs:12-26, 348-356). No IPC
  message is needed for the gateway path.
- The new daemon functions go in `crates/daemon/src/shared_budget.rs` and reuse
  `ensure_period_pool`. The guard allows `ensure_pool(` only there and in launch_gate.rs, and
  exactly one call in shared_budget.rs (crates/daemon/tests/metered_pool_no_production_feed.rs:85-117) [V].
- `gateway_authority.rs` implements a new gateway-crate trait, so the gateway still "decides
  nothing" (authority.rs:1-18).
- `SharedBudgetRuntime` is held by value in `DaemonConfig` (server.rs:183). Change it to
  `Arc<SharedBudgetRuntime>`, which keeps telemetry and the hot-reload cache shared.
  - `Clock: Send + Sync` holds [V shared_budget.rs:88].
  - Its fields are `Mutex`-wrapped (shared_budget.rs:147-156) [V].
  - `Arc<T: Default>: Default` [I], so tests that write `shared_budget: Default::default()`
    should still compile.

### 3.2 Per-request flow (shared path, `enforcement=gateway`, file valid)
1. Existing checks run unchanged: Host, token, route, body cap, semaphore, model, session
   header, task binding, task reservation (proxy.rs:481-1016).
   - **Keep the task gate as an AND.** Relaxing `task_unbound` is tied to the open founder
     decision HORO-1899 (docs/adr/0025-v0-0-4-scope-evidence-and-exclusions.md:53-56).
   - Whether the hook path always creates the `session_tasks` row is unverified; see Q3.
2. **New structural screen.** Refuse server tools, url/file sources, `mcp_servers`, and
   `container` (§1.3). The check reads `type` tags only and keeps nothing.
3. **Shared reserve.** `SharedBudgetAuthority::reserve_shared(SharedGatewayRequest { session_id,
   model, body_bytes, max_tokens, image_count, key })`. The daemon side then:
   - (a) runs `rt.effective()`, which is hot-reloaded on every decision (ADR-0026:178-187). It
     selects the mode per request:

     | File state | Gateway behaviour |
     |---|---|
     | `Absent` | Shared path not consulted, exactly today's behaviour. Deleting the file means the operator removed the budget (ADR-0026:187) |
     | `Valid`, `enforcement=hooks` (default) | Not consulted |
     | `Valid`, `enforcement=gateway` | Enforce: steps (b) to (d) |
     | `Invalid` | The mode cannot be read. **Recommend fail closed** with `403 config_invalid`. Note this changes behaviour for existing governor_held deployments that have a broken hooks-mode file |

     `later_pool` refuses `clock_anomaly`, the same as ADR-0026:166-174;
   - (b) prices the worst case **daemon-side** with **the shared-budget table** (pinned plus the file's
     `pricing_overrides`, ADR-0026:229-231). Overrides in the gateway config are **not** used on
     this path, so one table prices both reserve and settle;
   - (c) runs `ensure_period_pool` and `expire_stale_shared_pool_reservations`, which includes
     the expire-as-charge rule in §4;
   - (d) runs `reserve_shared_metered` with `amount = worst`, `ttl = gateway reservation_ttl_secs`
     (default 600, config.rs:50) [V], attribution `unattributed + Gateway`, and
     `capacity_source = LocalConfigured`. `Insufficient` refuses `shared_budget_exceeded`.
     Admission is atomic under `BEGIN IMMEDIATE` and counts settled plus active
     (shared_pool.rs:700-707) [V].
   - If the shared reserve is refused, **release the task reservation** before answering 403.
4. **Forward.** On connect or TLS failure, or any upstream non-2xx, release **both**. This is
   the same rule as proxy.rs:1022-1062: no tokens were billed [I for 5xx/529].
5. **Stream.** Every `max(ttl/3, 30s)`, the pump issues a non-blocking `renew_shared(id)` through
   `spawn_blocking`. A failed renewal is only logged; safety does not depend on it (§4).
   **Timeouts:** by design there is no upstream response timeout (config.rs:51-55) [V]. Pump
   renewal therefore keeps a stalled stream's reservation alive until the client gives up, and
   the real bound is the client's idle timeout [I]. That costs no capacity: the row stays
   reserved, which is the safe direction. The connect timeout is 30 s (config.rs:55).
6. **Settle (shared row).** The gateway passes raw `ObservedUsage`, `completed_cleanly`,
   `saw_message_start`, `model` and `max_tokens` to the authority. The daemon then calls the
   pure S11b function with the **shared-budget table**, using the reserved amount read from the
   ledger row. The gateway never prices the shared row itself.
   - If `completed_cleanly()` or the body is complete and non-streaming: price the exact tiers
     and settle with `usage_known=true`.
   - Else if `message_start` usage was seen: settle at the exact input tiers +
     `max_tokens × output`, `usage_known=false`, and record `truncated_settle`.
   - Else: settle at the reservation, `usage_known=false`.
   - If observed input is greater than the input bound, record `input_bound_violated`.
   - The amount is recorded unclamped. An overrun is visible, as in ledger
     shared_pool.rs:2034-2036, 2088-2092.
7. **Task row.** It settles as today. The truncated-stream rule for task budgets is a separate
   observation (§7, R6) and is out of S11 scope.

### 3.3 Idempotency keys
- Shared row key: `gw:{boot_id}:{request_uuid}`.
  - `request_uuid` is the existing per-request id (proxy.rs:421, 948) [V].
  - `boot_id` is a random id generated at `start_gateway`. It lets startup recognise orphans
    from a previous process (§4).
  - Uniqueness is `(pool, principal, key)` (migration 0016 index) [V].
- A gateway-internal retry of the ledger call (BUSY) reuses the key and gets `AlreadyGranted`.
  A client retry is a new HTTP request, a new key, and a new reservation. That is correct
  because it is new upstream spend (authority.rs:55-59) [V].
- Settle and release go by reservation id. A non-active row returns `AlreadyFinal`, so there is
  no double charge (shared_pool.rs:2029-2033, 2110-2115) [V].
- If the row was expired while still active (the sleep or wake case), a late settle records the
  actual (shared_pool.rs:2025-2030) [V]. With expire-as-charge the row is already `settled`, so
  the late settle is `AlreadyFinal` and the booked amount stays the conservative one.

### 3.4 Interaction with the UserPromptSubmit turn hold
| Option | Effect | Verdict |
|---|---|---|
| **Stack** (keep the hold, add per-request rows, keep transcript settlement) | The same API calls are priced up to four times: gateway settle, `Stop` (shared_budget.rs:1277), the subagent sweep / `ChargeAgentTranscript` (1487, 1767), and `SettleTail` (1893) | Reject: double-booked spend |
| **Nest** (requests draw down the turn hold) | Needs sub-reservations the ledger lacks. Subagents share the session header, so lanes cannot be told apart | Reject: complexity, and attribution is impossible |
| **Replace, mode-level** (recommended) | `Admit` reserves nothing. It answers from `check()` semantics (shared_budget.rs:774-819): deny `cap_exhausted` when remaining < `turn_hold_usd_cents`, which keeps the UX of refusing early. `Stop`, `ChargeAgentTranscript`, the sweep and `SettleTail` book nothing. `TranscriptReady` answers ready at once | **Choose.** Cost: traffic that bypasses the gateway is not counted. Mitigation: in gateway mode, a `Stop` for a session with no gateway row in the current pool raises an `unrouted_sessions` counter and a log line (detection, not counting) |
| Replace with `message.id` dedupe (hooks keep settling; transcript records whose `message.id` the gateway already booked are skipped) | Keeps coverage of bypass traffic. A race between the gateway's settle and the transcript read double-counts (the safe direction). The flat-hold fallbacks still book on top of gateway rows. A new column is needed | Present to the founder as the alternative. Not recommended for the first slice |

Protocol: replace mode needs a new `Admit` outcome (no `reservation_id`) and a `Refused`/new
variant for the charge paths. That means `PROTOCOL_VERSION` 24 → 25 (ADR-0026:22-24 shows the
bump precedent). A v24 hook against a v25 daemon fails closed at prompt admission
(crates/cli/src/agent/budget_gate.rs:357-363, 386-432) [V].

**Mode flips mid-month** are a residual:
- A hooks-mode `turn_hold` still settles by transcript after the flip, because settlement is
  chosen by the row's `charge_source`.
- After a flip from gateway back to hooks, the first `Stop`'s window starts at the lane's old
  watermark and re-counts records the gateway already booked. That is an over-count, the safe
  direction.
- `doctor` should warn about a flip while holds are live.

---

## 4. Crash recovery

**There is no separate gateway crash.** The gateway is a thread in the daemon process [V
server.rs:2911-2919; ADR-0003 §1]. The cases are:

| Event | Today | S11 rule | Invariant |
|---|---|---|---|
| Process death (SIGKILL, power loss) mid-request | The row stays `active` until TTL, then `expired`. Capacity is freed with unknown spend, an under-count ADR-0003 accepts (0003:313-314) [V] | **Startup orphan sweep.** At **daemon start** (not only in `start_gateway`, because the gateway may be disabled after a restart), every active `charge_source='gateway'` row whose key's `boot_id` ≠ the current one is settled at its reserved `amount`, `usage_known=false`. Only one daemon runs per state dir [I from pidfile.rs] | No capacity is created. Booking at most the worst case is conservative |
| Restart overlap: the old daemon is still draining in-flight streams when the new one sweeps [I] | — | The orphan sweep books the old rows at the reservation, and their late settles are `AlreadyFinal` | Over-count only, so safe |
| Gateway runtime dies, daemon lives (`run_gateway` Err, server.rs:2914-2917) | Rows stay active, then TTL | **Expire-as-charge.** In `expire_stale_shared_pool_reservations` (shared_pool.rs:2340), active rows with `charge_source='gateway' AND is_charge=0` become `settled`, `settled_amount=amount`, `usage_known=0`, instead of `expired` | Same |
| One request task panics | The pump never settles | Renewal stops, so expire-as-charge applies | Same |
| Laptop sleep longer than TTL mid-stream | Expiry frees capacity and the late settle records the actual | Expire-as-charge books the reservation, the late settle is `AlreadyFinal`, and the result is an over-count | Hard |
| Daemon death between task reserve and shared reserve | The task row expires | Nothing to do for the shared side | — |
| Death after shared reserve but before forward | — | The orphan sweep books the worst case even though nothing was sent | Over-count, accepted. The alternative is a `forwarded_at` column, which costs a migration and is not worth it |
| Ledger write fails at settle (`settle_or_release` swallows the error, proxy.rs:1283-1298) | The row stays active, then expiry | Expire-as-charge | Hard |
| Daemon restart during a hooks-mode turn | Unchanged (S5 rules) | — | — |

**Double-charge protection:** state transitions are guarded by `WHERE state='active'`
(shared_pool.rs:2056-2084) [V]. The orphan sweep and expire-as-charge are single `UPDATE …
WHERE state='active'` statements under `BEGIN IMMEDIATE`, and each row moves to a final state
exactly once.

**Required lane-query isolation** (ledger change, failing-first tests). These queries do not
filter `charge_source` today:
- `active_shared_holds_for_session` (shared_pool.rs:1489-1500): **the heartbeat would renew
  gateway rows to 3600 s** (shared_budget.rs:2305-2319) [V];
- `active_shared_hold_for_lane` (1225-1240);
- `shared_lane_holds` (1305-1317), which feeds `pending_hold` (shared_budget.rs:1219-1229).
  `pending_hold` also returns `Expired` rows.

Gateway rows are `unattributed`, so they do not match the `agent_absent` lane today [V]. Add
`AND COALESCE(charge_source,'') <> 'gateway'` as defence in depth anyway.

Retention: `prune` never touches a pool with an active row (ADR-0026:822-826) [V]. Gateway rows
are final within minutes, so no change is needed.

---

## 5. Founder (Level 2) items vs pure engineering

**Level 2: needs one founder packet before the mode is enabled or claimed**
1. Enabling or claiming `enforcement=gateway`, and the label wording in §1.4. ADR-0026 lists
   "gateway `governor_held` (S11, not built)" under *Not decided here* (0026:865-868), and the
   protocol docs forbid strengthening the cooperative label (protocol/src/shared_budget.rs:4-7).
2. **Counting scope change.** In gateway mode the budget counts only gateway-routed spend on the
   Governor-held key (replace), instead of transcript-measured spend from every session. The
   alternative is `message.id` dedupe.
3. **Refusing server tools and url/file sources in hard mode.** This likely disables Claude Code
   WebSearch [I]. The alternative is an operator-configured per-use surcharge, which is a new
   financial policy.
4. **Conservative settlement policy.** Truncated, orphaned and expired gateway requests are
   booked at the worst case. Expect visible over-count on ESC interrupts [I].
5. The input-bound choice for hard mode: `bytes/1` (recommended) vs `bytes/3` vs a
   `count_tokens` preflight (an extra upstream round trip and dependency). This is a
   cap-usability trade-off.
6. The `Admit` fail-closed rule when `enforcement=gateway` but the gateway is absent, invalid or
   not `governor_held`: deny `gateway_unavailable`. This matches the ADR-0026 fail-closed
   pattern, so confirm only.

**Level 0/1: buildable dark and offline now, with no shipped-behaviour change**
- S11b (the shared-path settle rules apply only on the new path), S11c (ledger changes are inert
  until a gateway row exists), and S11d/S11e/S11f behind `enforcement=gateway`, whose default is
  `hooks`.
- Config validation: `enforcement=gateway` together with `PassThroughSubscription` is refused,
  mirroring `SubscriptionModeCannotEnforceUsd` (crates/gateway/src/config.rs:240-244) [V
  pattern].
- No new egress: the gateway and its allowlist already exist (docs/egress-inventory.md:32, 42).
- The trust boundary is unchanged. Governor-held custody is already accepted in ADR-0003 §5.
- Everything is testable offline with the fake upstream (crates/gateway/tests/fake_upstream.rs)
  and `ManualClock` (shared_budget.rs:102-118). Evidence class is `TestOwnedLive`.

---

## 6. Slices

The order is a DAG: **S11a ∥ S11b ∥ S11c → S11d → S11e → S11f → S11g**. S11h is optional and
independent. S11a gates **enablement**, not building.

### S11a: Host evidence for the gateway request shape (no product code)
- **Files:** docs/adr/0026 *Real-host verification checklist* section, plus an evidence note.
- **Work:**
  - Run real `claude` against the gateway with `allow_plaintext_loopback_upstream` and the fake
    upstream: isolated HOME, network-deny sandbox, dummy key, synthetic prompts. This is the
    ADR-0026:584-604 harness.
  - Record header **names** and the equality of the session header with the hook `session_id`.
  - Record any subagent-distinguishing header.
  - Inventory request structure: `tools[].type` values, `source.type`, `cache_control.ttl`,
    speed/geo/beta flags, whether `max_tokens` is always present. Record tags only, no content.
  - Record whether WebSearch uses a server tool.
- **AC:**
  - Each [H] in §2 and §1.3 is marked confirmed or refuted, with the CLI version.
  - No secret, prompt or body content is stored.
- **Proposed title:** "S11a: capture Claude Code gateway request identity and envelope shape
  (TestOwnedLive)".
- **Codex QA:** no (docs only).

### S11b: Shared-path settlement rules and input-bound detection (gateway crate)
- **Files:** crates/gateway/src/{proxy.rs,cost.rs,usage.rs,stats.rs};
  crates/gateway/tests/proxy_lifecycle.rs; crates/gateway/tests/fake_upstream.rs.
- **Work:**
  - Add `settle_amount_for(observed, completed_cleanly, saw_message_start, reserved, max_tokens,
    pricing) -> (amount, usage_known, flags)`. It is pure and unit-tested.
  - Add `hard_input_upper_bound(bytes, image_count)`.
  - Add `screen_unbounded_inputs(body) -> Option<EnforcementGap::UnboundedInput>`.
  - Add stats counters: `truncated_settles`, `input_bound_violations`.
  - These are **not yet called on the task path.** The pinned test at proxy_lifecycle.rs:472
    stays green.
- **Failing-first tests:**
  - A `CANNED_SSE_TRUNCATED` stream (fake_upstream.rs:112-119) settles at input + `max_tokens`
    output, not 17 tokens.
  - A client drop right after `message_start` behaves the same way.
  - No `message_start` settles at the reservation.
  - Emoji-dense synthetic input with reported input > `bytes/3` sets `input_bound_violated`.
  - A body with a `web_search` tool or a url image is refused, and the stub authority records
    zero `Authorize` calls.
- **Proposed title:** "S11b: conservative truncated-stream settlement and unbounded-input
  screen for the gateway shared-budget path".
- **Codex QA:** yes, affected-only (gateway crate).

### S11c: Ledger: gateway rows never free capacity unbooked, and lanes ignore them
- **Files:** crates/ledger/src/shared_pool.rs; ledger tests (the shared_pool adversarial and
  crash suites).
- **Work:**
  - Expire-as-charge for `charge_source='gateway' AND is_charge=0` in
    `expire_stale_shared_pool_reservations`.
  - Add `settle_orphaned_gateway_holds(boot_prefix_keep, now)`.
  - Add `charge_source` exclusion to `active_shared_holds_for_session`,
    `active_shared_hold_for_lane`, and `shared_lane_holds`.
  - No migration: the CHECK already allows `'gateway'`.
- **Failing-first tests:**
  - Reserve a gateway row, advance `ManualClock` past TTL, run the sweep. The row is `settled`
    at `amount` with `usage_known=0`, and remaining is unchanged before and after.
  - A late `settle_shared` returns `AlreadyFinal`.
  - The heartbeat does not renew a gateway row.
  - A gateway row in a session is never returned as the `pending_hold` and never reused by
    `admit`.
  - A row with an old boot id is booked by the orphan sweep; current-boot rows are untouched.
  - The sweep runs twice and books once.
  - A 16-thread race of reserve against expire never lets settled + active exceed the cap.
- **Proposed title:** "S11c: ledger expire-as-charge and lane isolation for gateway
  shared-pool holds".
- **Codex QA:** yes, affected-only (ledger).

### S11d: Daemon shared-budget gateway primitives and the `enforcement` config field
- **Files:** crates/daemon/src/{shared_budget.rs,shared_budget_file.rs,server.rs (Arc runtime)};
  crates/daemon/tests/shared_budget_core.rs plus a new crates/daemon/tests/shared_budget_gateway.rs.
- **Work:**
  - Add `gateway_reserve`, `gateway_settle`, `gateway_release`, `gateway_renew`, and
    `gateway_orphans_at_start` in shared_budget.rs. They use `ensure_period_pool`, the
    shared-budget pricing table, `validate_identity` plus the 256-byte cap, and attribution
    `unattributed + Gateway`.
  - Parse `enforcement` (default `hooks`). An unknown value is `config_invalid`.
  - `DaemonConfig.shared_budget` becomes an `Arc`.
  - `gateway_settle` takes raw usage plus the S11b flags and prices with the shared-budget
    table, using the reserved amount read from the ledger row. The gateway config's
    `pricing_overrides` are never used on this path.
- **Failing-first tests:**
  - Reserve against a file that is `Absent`, `Invalid`, or in clock anomaly: each refuses and
    writes nothing (a whole-ledger diff).
  - Reserve, settle, and release round-trip with exact cents.
  - Pricing uses the file's `pricing_overrides`, not the gateway config's.
  - Month rollover: the row stays in its admission pool.
  - Over the cap: refused, and remaining is unchanged.
  - The `metered_pool_no_production_feed` guard still passes.
- **Proposed title:** "S11d: daemon shared-budget gateway reserve/settle primitives (dark)".
- **Codex QA:** yes, affected-only.

### S11e: Wire the gateway to the shared budget
- **Files:** crates/gateway/src/{authority.rs (new `SharedBudgetAuthority` trait),proxy.rs,
  config.rs,server.rs}; crates/daemon/src/{gateway_authority.rs,server.rs:start_gateway};
  crates/gateway/tests/proxy_lifecycle.rs (stub); crates/daemon/tests/gateway_enforcement.rs
  plus a new crates/daemon/tests/gateway_shared_budget.rs.
- **Work:**
  - The task reserve and the shared reserve are combined with AND, with compensating release.
  - Both are released on upstream failure.
  - Add pump renewal and the S11b settle rule on the shared row.
  - Add `x-libra-decision` values: `shared_budget_exceeded`, `shared_budget_unavailable`,
    `shared_budget_unattributable`, and reuse `unenforceable_request`.
  - Add `boot_id`, and call the orphan sweep at daemon start.
  - The shared row is settled daemon-side from raw usage with the shared-budget table (§3.2
    step 6).
  - Validation refuses `enforcement=gateway` without `GovernorHeld`.
- **Failing-first fault-injection tests (real ledger, fake upstream):**
  1. Cap exhausted: 403 before upstream, and the fake upstream's `received()` is empty.
  2. 8–16 concurrent requests across sessions: granted worst-case total ≤ remaining, and after
     settling, settled ≤ cap unless a bound violation is flagged.
  3. Upstream 529/5xx: both rows released.
  4. Stream truncated or client dropped: booked at the S11b amount.
  5. The authority errors on settle: the row stays active, and after `ManualClock`
     advancement the expire sweep books the reservation.
  6. The gateway runtime is shut down mid-stream and a new `start_gateway` runs with a new
     boot id: the orphan is booked once and remaining is never above its pre-request value.
  7. A missing session header refuses with zero reservations.
  8. The shared reserve is refused after the task reserve was granted: the task reservation is
     released.
  9. No body, header or credential appears in any row (extends gateway_enforcement.rs:602).
- **Proposed title:** "S11e: gateway enforces the local monthly budget per request in
  governor_held mode (dark)".
- **Codex QA:** yes, affected-only (gateway plus daemon).

### S11f: Hook side of replace mode, Status and doctor
- **Files:** crates/protocol/src/{shared_budget.rs,lib.rs} (v25, new Admit outcome,
  `GatewayMetered` refusal, additive Status fields: `enforcement`, `guarantee` per mode,
  `gateway_in_flight`, `truncated_settles`, `input_bound_violations`, `unrouted_sessions`,
  `orphans_booked`); crates/daemon/src/shared_budget.rs (`admit`, `settle_turn`,
  `charge_agent_transcript`, sweep, `settle_tail`, `transcript_ready` mode branches);
  crates/cli/src/agent/budget_gate.rs; crates/cli/src/shared_budget_view.rs; doctor;
  crates/daemon/tests/shared_budget_settlement.rs.
- **Failing-first tests:**
  - In gateway mode, `Admit` creates no row and denies when remaining < hold.
  - `Stop`, `SubagentStop`, `SessionEnd` and the sweep book zero, with a whole-ledger diff.
  - A pre-flip `turn_hold` still settles by transcript.
  - An unrouted session increments the counter.
  - v24 ↔ v25 skew fails closed at admission.
  - In hooks mode, behaviour is byte-identical (the existing suites stay green).
  - `doctor` reports an Error for `enforcement=gateway` without a running `governor_held`
    gateway, and checks that the `ANTHROPIC_BASE_URL` key is present in the `settings.json`
    env (presence only).
- **Proposed title:** "S11f: replace-mode hooks, status and doctor for gateway-enforced shared
  budget".
- **Codex QA:** yes, affected-only.

### S11g: ADR-0026/ADR-0003 amendment and founder packet
- **Files:** docs/adr/0026-local-monthly-shared-budget.md (implementation status, guarantee
  label section, a new *Gateway enforcement delivered in S11* section with residuals);
  docs/adr/0003 header note; docs/requirements-gap-matrix-v0.0.4.md (row C).
- **AC:**
  - The label is stated exactly as approved, with no "hard cap" wording without its qualifiers.
  - Every residual from §1.3, §3.4 and §4 is listed.
  - The founder packet holds the Level 2 items from §5 as ONE packet.
- **Codex QA:** no (docs). The founder decision gates any README or release-notes claim.

### S11h (optional): model the 1-hour cache tier
- **Files:** crates/gateway/src/{usage.rs,pricing.rs,cost.rs}; the daemon transcript pricing
  (`usage/priced.rs`).
- **Work:**
  - Parse the 5-minute and 1-hour cache-creation breakdown. The field names come from the
    S11a/API docs [I].
  - Worst-case input = `max(…, 2×input)`.
  - Bump `PRICING_VERSION`.
- **Codex QA:** yes. This changes every USD path.

---

## 7. Risks and open questions

- **R1 [H]:** the session header is not sent, or does not equal the hook `session_id`. Gateway
  mode is then unusable (fail closed). S11a must resolve this first.
- **R2:** over-reservation near the cap. With `bytes/1` and a 200k-token context, the
  worst-case input alone is about $1–4 per request for Opus-class prices [I, arithmetic on
  pricing.rs:116-121]. N concurrent subagents multiply it. The last few dollars of the month
  are unusable, as ADR-0003:306-310 accepts. A context-window cap on the input bound is a
  possible later refinement.
- **R3:** conservative booking of truncations and interrupts inflates reported spend relative
  to the bill. It is visible as `usage_known=false` rows plus counters.
- **R4:** WebSearch is refused in hard mode [I]. This is a product trade-off for the founder.
- **R5:** spend that bypasses the gateway (a session not pointed at it, or the user's own
  subscription credential) is uncounted in replace mode. It is detected, not counted.
- **R6 (pre-existing, outside S11):** the task-budget gateway path under-counts truncated
  streams (proxy.rs:1234-1243, pinned by proxy_lifecycle.rs:472). It also frees forwarded
  capacity at expiry (ADR-0003:313-314). Recommend a separate ticket, because changing the
  pinned behaviour is a deliberate change to the task path.
- **R7:** the gateway TTL (600 s) is shorter than the heartbeat TTL (3600 s). Long thinking
  streams rely on pump renewal for accuracy, but not for safety.
- **R8:** the same OS user can raise the cap or edit the ledger (ADR-0026:189). That is
  unchanged and stated in the label.
- **Q1:** Does a subagent's API traffic carry any agent identifier? This affects the
  attribution granularity of `unattributed`.
- **Q2:** Does Anthropic bill output tokens generated after a client cancels a stream beyond
  those relayed? This decides whether input-exact plus `max_tokens` is tight or loose. The
  answer cannot change safety, only accuracy.
- **Q3:** Does the hook path always create `session_tasks`, which the gateway's task gate
  requires (crates/ledger/src/session.rs:49-64)? If not, gateway mode refuses `task_unbound`.
  Verify in S11a and S11e integration tests.
- **Q4:** Founder choice between replace and `message.id` dedupe (§3.4).
