# v0.0.4 requirements gap matrix (original Golden Journey vs implementation)

Status: reconciliation record, not a release decision. Jira citations other than HORO-1915 c27743, HORO-1727 c27893 and HVDL-29 c25967 were not re-fetched in review (UNVERIFIED); the claim that per-executor soft limits have no Jira text rests on the recovery agent's searches, not an exhaustive JQL sweep. Tracks HORO-2186; proof work is HORO-2185.
Code reference: `main` at `5fd7209e78eb43b81335e2d9e470cfa48da8c547`. Nothing here is `ProductionObserved`.

## Why this exists

ADR-0025 proposes an Apple Silicon Preview. A Preview that passes its own evidence is **not** proof that the
original founder requirements are met. This matrix compares the recovered requirements with the code.
A deferral counts as approved only when a founder-labelled Jira decision is cited. Agent-written audit text is
not approval. Author names in Jira do not separate founder from agent sessions.

## Original and current Golden Journey

Original: configure one shared budget (and optionally an external quota source) → many sessions and sub-agents
consume the same pool → usage is attributed to the right executor → pacing/admission responds → records persist
safely → no unauthorized egress → authorized export works reliably when enabled.

Current v0.0.4 (the Claude 5h/7d statusLine signals are not captured, per a code comment in `agent.rs` that records a 2026-10-02 decision; no Jira decision comment was cited): the shared pool exists only as a mechanism provisioned by a test fixture (ADR-0023, no production
feed); turn-scoped enforcement, the launch gate and the ledger are real and tested; sub-agent attribution, monthly
caps, soft limits, retention and upload do not exist.

## Founder-sourced decisions that bound the work

| Ref | Decision | Effect |
|---|---|---|
| HORO-1915 c27743 (Option 1, labelled FOUNDER DECISION) | Metered pool is separate and observation stays observation-only; wiring observation into admission is a stated non-authorization; the smallest production-reachable consumer path may be designed | Wiring observation into admission, or promoting a real snapshot into the pool, needs a separate founder decision |
| HORO-1853 | One monthly operator allowance is ONE pool across all authorized sessions/sub-agents; no per-session copies | Original intent for A/C |
| HORO-1727 c27893 (founder decision record, relayed by a session) | Scope-separated budget model adopted (per-turn ceiling, active-task budget, shared pool, host/provider cap); 400k per-turn is the current candidate; 100K/125K/150K are not an unconditional lifetime limit; the default-flip slice S4 was held for a founder packet | Scope model for C is decided; its implementation and the S4 default are not |
| HVDL-29 c25967 | Spend recorded once at narrowest truthful scope, aggregated upward; self vs subtree distinct; unknown lineage stays explicit | Rule for B/K |

## Matrix

Legend: VERIFIED, IMPLEMENTED_UNVERIFIED, PARTIAL, NOT_IMPLEMENTED, DEFERRED_WITH_APPROVAL, OUT_OF_SCOPE_WITH_APPROVAL.
No row is DEFERRED_WITH_APPROVAL or OUT_OF_SCOPE_WITH_APPROVAL except where a founder decision is cited.
Real-host evidence is NONE for every row.

| Cap | Source | Target | Status | Code path | Tests | User-visible | Missing | Ticket | Release impact |
|---|---|---|---|---|---|---|---|---|---|
| A Shared pool | HORO-1853, HORO-1915, HVDL-29 | v0.0.4 | PARTIAL | `crates/ledger/src/shared_pool.rs`, `crates/daemon/src/metered_pool.rs` | `metered_pool_golden_journey.rs` | `metered-pool reserve/settle/release/status` | Production feed (every metered-pool reserve refuses `NoPool`; the separate launch-gate pool works); wiring from hooks/gateway; cross-host | HORO-1915 (Done), HORO-2186 | Core journey not usable end to end |
| B Attribution | HVDL-29 R1/R2, ADR-0008 | v0.0.3-0.0.4 | PARTIAL (refreshed 2026-10-31) | `crates/domain/src/execution_identity.rs`, `agent.rs`; per-`(session, agent)` lanes on the legacy route (HORO-2197) and in the shared budget (`SubagentStop` settles a subagent transcript, ADR-0026 S5/S7) | `execution_owner.rs`, `multi_session_isolation.rs`, `legacy_executor_attribution.rs`, `shared_budget_settlement_hooks.rs` | per-session usage in `status`; per-executor rows in `shared-budget status` | `SubagentStart` not installed (`SubagentStop` is); lineage always Unknown; no parent/child aggregation API; no real-host evidence | HORO-1599/1714/1718, HORO-2185, HORO-2179, HORO-2197, HORO-2216 | Sub-agent accounting partial: attributed per executor, not aggregated by lineage |
| C Monthly cap / soft limits | HORO-1853 (monthly allowance); soft limits NOT FOUND in Jira | v0.0.4 | IMPLEMENTED_UNVERIFIED (refreshed 2026-10-31; was NOT_IMPLEMENTED) | `crates/daemon/src/shared_budget.rs`, `shared_budget_file.rs` (ADR-0026 S4-S8) | `shared_budget_golden_journey.rs`, `shared_budget_hooks.rs`, `shared_budget_settlement_hooks.rs` | `shared-budget status`, budget segment on the statusline, `doctor` checks; soft limits warn only | S11 gateway hard cap and Codex gate not built and must stay off in v0.0.4 (Cooperative Preview); no real-host evidence (`TestOwnedLive` at best) | HORO-2193, HORO-2195, HORO-2196, HORO-2216, HORO-2217, HORO-2186 | Local monthly cap and warn-only per-executor soft limits shipped as the Cooperative Preview; hard-cap guarantee not delivered |
| D Windows | HORO-1749 (hourly, 6h, daily, weekly) | v0.0.4 | PARTIAL | `crates/domain/src/quota_window/` | `quota_window/tests.rs` | `quota explain --replay` (simulation) | No Month kind; no named half-day (12h is expressible as Sliding); no live producer | HORO-1762 (Done) | Simulation only |
| E Extension | HORO-1853..1857 | v0.0.4 | IMPLEMENTED_UNVERIFIED | `crates/quota-observation/` | `conformance_*.rs` | `quota-observation register/approve/enable/doctor/inspect` | Observation-only by founder decision; no admission link; no real private source | HORO-1856 (To Do), HORO-1857 | Preview-only |
| F Owner/units/freshness | HORO-1749, ADR-0023 | v0.0.4 | IMPLEMENTED_UNVERIFIED | `quota-observation/src/freshness.rs`, metered `metered_snapshot_limit` | `shared_pool_adversarial.rs` | fail-closed refusals | Not in any production flow; whether the extension wire path validates `declared_reset_at` ordering is untraced (the `quota-source` provenance validator rejects it) | HORO-1904 | Simulation only |
| G SQLite / retention / privacy | HORO-1124, ARCHITECTURE.md | MVP1 | PARTIAL | `crates/ledger/src/store.rs` | crash tests in `shared_pool_adversarial.rs` | local files, 0600/0700 | No retention/pruning | HORO-1372 (To Do) | Unbounded growth, disclosed |
| H Corporate local-only | HORO-1376; agent claim in HORO-1768 | v0.0.4 | PARTIAL | `crates/evidence-adapter/src/adapter.rs` | `adapter_fixtures.rs`, `no_network_symbols.rs` | evidence export refuses non-local | Export is local-only for every profile; the profile is only a data stamp; gateway/webhook egress not gated by profile | none | Claim must stay narrow |
| I Batch export/upload | no Jira ticket found | unknown | Export PARTIAL / upload NOT_IMPLEMENTED | `evidence_report_cmd.rs`, `dogfood_evidence_cmd.rs` | `evidence_report_privacy.rs` | manual local files | Uploader, retry, resume, dedupe, integrity, retention-aware delivery | none (HORO-2186) | Needs scope decision |
| J Adapters | HORO-1749 | v0.0.4 | PARTIAL | `crates/domain/src/agent.rs` | `adapter_*.rs`, `codex_hooks_integration.rs` | honest Unavailable rows | No real-host proof; Codex gateway/hard budget unavailable | HORO-1714, HORO-1718 (In Progress) | Open |
| K Concurrency / crash | HORO-1763/1779 | v0.0.4 | VERIFIED (TestOwnedLive, synthetic fixture) at session level | `shared_pool.rs` (IMMEDIATE txns) | 37-thread contention, same-key races, duplicate settle, mid-transaction crash; HORO-2185 (`metered_pool_accounting.rs`, merged in 3fe6ae9): 16-process CLI reserve race (12 granted / 4 refused: 4,000 each against a 50,000 pool), 8-process ledger race, per-session totals sum to pool settled total, SIGKILLed-daemon recovery, best-effort client-kill idempotent-retry convergence | n/a | Anything below session level (agent/sub-agent, parent/child, self-vs-subtree); real-host concurrency; hostile same-user session-id spoofing (ADR-0023 known limitation) | HORO-2185 (merged) | Core invariant holds in test; not ProductionObserved |

## Material gaps and proposed handling

1. **Metered pool has no production feed (A).** Promoting a real snapshot into the pool is a stated non-authorization in HORO-1915 c27743; the smallest production-reachable consumer path may be designed.
   Founder decision needed on source of capacity (configured monthly cap vs provider snapshot) and wiring.
2. **No monthly cap, no Month window, no soft limits (C, D).** Smallest honest slice: a configured monthly cap as
   the pool capacity (no provider dependency) plus a Month aligned window; soft limits as a warning-only
   per-executor threshold inside the pool total. Needs the founder's confirmation of semantics.
3. **Sub-agent attribution (B).** Needs SubagentStart/Stop hooks and a native parent id. Neither host supplies a
   parent id today; Libra must not invent one. Coordinate identity primitives with Host Integration (HORO-1599
   family). Today (proved by HORO-2185, PR #207): metered-pool spend is keyed only by an opaque `session_id` under the owner; a request with no established session identity is refused and nothing is charged (there is no Unknown bucket); spend is recorded once per session and per-session totals sum to the pool's settled total. There is no agent, sub-agent or parent/child field on reserve or settle, and no self-vs-subtree API, so per-executor attribution below session level does not exist.
4. **Retention (G).** Smallest slice: configurable age-based prune of closed rows with an explicit audit count.
5. **Batch upload (I).** Not started; needs a design, a consent and destination decision, and an
   idempotency/integrity protocol. Do not build before the destination and policy are decided.
6. **Corporate local-only (H).** Either enforce the profile at every egress point or keep the claim limited to
   evidence export.

## Release scope

v0.0.4 as an Observation Preview remains honest only if it is labelled as such and the promised journey is not
claimed. It must not be called complete against the original Golden Journey. Alternatives, cost and risk are in the
founder packet under HORO-2186.

## Acceptance scope reconciliation (2026-10-27, `main` after PR #231)

The matrix above is a historical snapshot at `5fd7209`. It is superseded for
scope by the founder decisions below. No accepted evidence is changed.

| Decision | Record | Effect on this matrix |
|---|---|---|
| Option 2: local-first Shared-Pool MVP (D1 local monthly USD budget, D3 one global hard budget, per-executor soft limits warn-only) | HORO-2186 c28778, ADR-0026 | Rows A and C move from NOT_IMPLEMENTED/PARTIAL to the ADR-0026 slices S2-S10, shipped as the cooperative pool. Remote upload stays a separate workstream (row I stays NOT_IMPLEMENTED). |
| v0.0.4 ships as a labelled **Cooperative Preview**; S11 gateway cap and the Codex gate are not required to be enabled and must not be enabled in v0.0.4 | HORO-2261 c29248, ADR-0026 "Founder version-scope decision", README | The original hard-cap guarantee is **not** delivered. It is a founder-approved scope adjustment, not an equivalence claim. Row J Codex budget gating stays unavailable. |

Accepted evidence preserved unchanged: HORO-2250 c29205 (PR #227 SCOPED PASS)
and HORO-2258. Nothing is `ProductionObserved`; evidence remains
`TestOwnedLive` at best. Release disclosures required by the founder are in the
README section "Local monthly budget (optional, Cooperative Preview)".

Still open for release readiness (not decided here): remaining original
applicable QA and release gates, the producer score (72/100 provisional), and
the separate founder publication approval. `READY_FOR_RELEASE=false`.

