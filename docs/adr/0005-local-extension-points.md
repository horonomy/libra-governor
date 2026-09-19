# ADR 0005: Local Extension Points Are Outbound-Only and Narrowing-Only

- **Status:** Accepted
- **Date:** 2026-09-19
- **Ticket:** HORO-1174

## Context

Libra's admission and outcome logic is entirely local and entirely
Libra's own — `Policy::evaluate` (HORO-1137), the Completion Reserve
(HORO-1141), and the estimator (HORO-1126/1130) all run inside
`crates/daemon` against `crates/ledger`. Real deployments want three
things this doesn't cover:

1. A team's own business context (a deadline, a priority, a cost center)
   informing admission for a task that already has an external tracker
   reference.
2. A team's own policy authority reviewing an `ApprovalRequired`
   admission automatically, rather than always waiting on a human.
3. External systems (CI, deployment tooling) attesting to whether a task
   actually succeeded — the daemon's own `Finalize` path has no way to
   know a background CI run failed after the agent session ended.

This ticket adds exactly those three integration points, plus the wire
schema and event-delivery infrastructure a real external provider needs
to receive daemon-side lifecycle notifications. Per ADR 0001's
convention, this ADR states which component each part belongs to and
which trust-boundary rules constrain it before any of it is built.

## Decision

### 1. One new crate, `crates/extension`, depending on `libra-governor-domain` and nothing else of Libra's

Same structural argument HORO-1144 (ADR 0003) made for `crates/gateway`,
and the same one this repo's own architecture doc names as its general
principle: a crate that cannot depend on `libra-governor-ledger` cannot
open the ledger; a crate that cannot depend on anything that constructs a
`Policy` cannot decide an admission. `crates/extension` depends on
`libra-governor-domain` (for value types — `Policy`, `Admission`,
`TaskId`, `BusinessContextSummary`, etc.) and nothing else Libra-specific.
An external provider's HTTP response reaches this crate's parsing code,
never the ledger, never a `Policy` constructor. "The provider informs;
the daemon decides" is therefore a property of the dependency graph, the
same discipline ADR 0004 (HORO-1157) explicitly considered and ADR 0003
established for the gateway.

ADR 0004 rejected a new crate/trait for the agent adapter because the two
agent hosts had zero actual variation to abstract — a trait would have
been indirection with nothing to justify it. This is the opposite case:
`crates/extension`'s isolation is not abstracting over variation, it is
*enforcing a trust boundary*. The two ADRs are not in tension; they apply
the same underlying test ("does the separation buy something real?") to
opposite answers because the actual situations differ.

### 2. Outbound-only over loopback HTTP, HMAC-signed; the one new inbound path reuses the existing Unix socket

Three surfaces — Business Context Provider fetch, Policy Webhook call,
signed event delivery — are the daemon acting as an HTTP **client**
against a provider process the operator runs locally. No new inbound HTTP
listener exists anywhere in this repository as a result of this ticket.

The one new *inbound* capability, `Request::RecordOutcome`, is added to
the protocol the daemon already serves over its existing 0600 Unix
socket. A new HTTP listener for outcome push was considered and rejected:
it would duplicate the loopback-security work ADR 0003 already did for
the gateway (host/scheme/allowlist validation, `Host` header checking, a
capability token) for a second listener whose only job is "receive one
attestation" — the existing socket already does exactly that, for every
other `Request` variant, at zero marginal listener surface.

**Honest limitation, stated plainly:** the daemon cannot cryptographically
distinguish two local processes talking to it over a Unix socket. The
boundary is filesystem permissions — socket 0600 inside the state
directory's 0700 mode, the same boundary every other `Request` variant
already relies on. `RecordOutcome`'s `source_id` is a recorded claim, not
an authenticated identity. This is not a regression introduced by this
ticket; it is the same boundary the whole daemon protocol has always had,
now also carrying one more `Request` variant.

### 3. `crates/daemon` has no async runtime; `crates/extension` owns one internally and exposes a blocking API

ADR 0003 §1 deliberately kept async work off the daemon's serial accept
loop. This ticket does not change that. `crates/extension::ProviderClient`
owns a single-threaded Tokio runtime internally; its public API
(`fetch_business_context`, `request_policy_decision`) is ordinary
blocking Rust the daemon calls synchronously inside `handle_preflight`,
exactly like any other function call. The event dispatcher
(`crates/extension::run_dispatcher`) is the one new thread this ticket
adds to the daemon process — the same "one thread per optional subsystem"
shape ADR 0003 established for the gateway.

### 4. Trust boundary R1 — `BusinessContext` is a separate type from `TaskFeatures`

`libra_governor_domain::BusinessContextSummary` lives in its own module
(`crates/domain/src/business_context.rs`), is stored in its own ledger
table (`business_context`, migration `0009`), and no function anywhere in
either crate converts one into the other. The field-set-pinning test
`business_context_field_set_contains_no_task_feature_or_policy_field`
asserts the summary carries no field that maps onto `min_confidence`,
`autonomy`, or a resource ceiling. This is checked by a value-invariant
test on every commit, not asserted as a type-level impossibility — see
§5 immediately below for why that distinction matters and is stated
honestly rather than overclaimed.

### 5. Trust boundary R2 — advisory criteria never reach `Policy.quality_floor`

This is the sharpest rule in the whole feature, and it is deliberately
**not** claimed as structurally impossible in the type-theoretic sense:
`CompletionCriterion::required(impl Into<String>)` exists, so a
`Vec<String>` → `Vec<CompletionCriterion>` conversion is trivially
writable by a future change. What this ticket actually delivers, and what
is actually true today, is:

- **A value invariant, tested on every commit.** Every
  `apply_business_context` output satisfies `derived.quality_floor ==
  base.quality_floor` unconditionally — see the `property_r2_advisory_fields_never_reach_the_policy`
  and `business_context_never_touches_non_time_fields` tests in
  `crates/domain/src/business_context.rs`.
- **A marker test through the real admission path.** The fake example
  provider (§10) returns `advisory_criteria` containing a distinctive
  marker string; a daemon integration test asserts the marker appears
  nowhere in `serde_json::to_string(&result.admission)` while it does
  appear under `result.business_context.advisory_criteria`.
- **The absence of any conversion function**, which is easy to grep for
  and which code review (not the compiler) is what actually keeps this
  invariant intact over time.

Priority, cost center, and advisory criteria are recorded metadata only —
never decision inputs. The **one** real, tested lever business context has
on admission is deadline narrowing (§6). One real, tested lever beats
three decorative ones that merely look like inputs to `Policy::evaluate`
without actually being wired to it.

### 6. The narrowing algorithm, and what it deliberately never touches

`apply_business_context(base: &Policy, deadline: Option<OffsetDateTime>,
now) -> (Policy, bool)`:

```
remaining = max(0, (deadline - now).whole_seconds())
if remaining == 0 { fail open: (base.clone(), false) }
target'   = min(base.time.target_secs, remaining)
hard'     = min(base.time.hard_ceiling_secs.unwrap_or(remaining), remaining)
elastic'  = base.time.elastic_ceiling_secs.map(|e| e.clamp(target', hard'))
deadline' = min(base.time.deadline.unwrap_or(deadline), deadline)
then Policy::validated_at(.., now) — on Err, fail open: (base.clone(), false)
```

Four properties are proven by property tests over every shipped preset
(`balanced`/`deadline_first`/`cost_first`/`strict_budget`) and a wide
range of `remaining` values, not asserted for one hand-picked case:

1. `derived.time.target_secs/hard_ceiling_secs/elastic_ceiling_secs` are
   each `<=` the base's.
2. For any given projected duration, `derived`'s admission is never more
   permissive than `base`'s on the `Admit > ApprovalRequired > Deny`
   lattice.
3. `derived` always independently re-passes `Policy::validated_at`.
4. `derived.resource == base.resource`, `derived.quality_floor ==
   base.quality_floor`, `derived.min_confidence == base.min_confidence`,
   `derived.autonomy == base.autonomy` — always, unconditionally.

**The narrowed policy feeds `Policy::evaluate` only.**
`initialize_task_budget`/`adjust_completion_reserve`/`completion_reserve_for`
in `handle_preflight` continue to receive `config.policy` (the durable,
unnarrowed policy) — never `effective_policy` — on every code path. This
was verified against the real reservation code (HORO-1141), not assumed:
`gateway_authority::LedgerSpendAuthority::authorize` reads
`budget.policy` back from the persisted `task_budgets` row on **every
subsequent gateway request for the life of the task**, and
`initialize_task_budget` is idempotent — a later preflight cannot correct
a wrong value once persisted. Had the narrowed policy ever reached
`initialize_task_budget`, every future per-request gateway enforcement
for that task would have permanently used the narrowed (tighter)
ceilings, which is not itself a safety violation but is a silent,
undocumented behavior change no reviewer asked for. The regression test
`business_context_narrows_evaluate_but_never_the_persisted_budget_policy`
asserts the persisted `task_budgets.policy` equals `config.policy`
exactly, not merely that `hard_limit` is unaffected.

### 7. Trust boundary R3 — only Provider/GovernorLocal outcome attestations are authoritative

`AttestationSource::Agent { .. }.is_authoritative()` is `false` by
construction. An Agent-sourced attestation writes an
`outcome_attestations` row and **never** promotes
`receipts.outcome_json`. No code path in this ticket ever constructs
`AttestationSource::Agent` — `Request::RecordOutcome` always attributes to
`AttestationSource::Provider { provider_id: source_id }`, and
`handle_finalize`'s own outcome event always attributes to
`AttestationSource::GovernorLocal`. `AttestationSource::Agent` exists as a
documented, tested variant for a future path (an agent's own transcript
claiming completion) that this ticket does not wire up — recorded rather
than omitted, so a future caller has the type ready without having to
re-derive the one-way-valve invariant from scratch.

### 8. `receipts.outcome_json` promotion is verified inert to the estimator

`LedgerStore::promote_receipt_outcome` is a materialized-view write:
"the receipt's outcome becomes the latest authoritative attestation's
outcome." Before wiring this in, the design's claim that this is safe was
checked against the real code, not assumed: `receipts_for_estimation`
(`crates/ledger/src/query.rs`) and `calibration_pairs` (same file) both
read every column of a `receipts` row *except* filtering or keying on
`outcome_json` — outcome is loaded into the returned `ExecutionReceipt`
but never inspected to decide inclusion. `libra-governor-estimator`'s own
source (`crates/estimator/src/calibration.rs`) does not reference
`ExecutionOutcome` at all. An outcome push therefore does not
retroactively change which receipts feed the estimator or the
calibration report — a real, checked fact, not an assumption.

### 9. The other one-way valve: `apply_external_approval`

Only `Admission::ApprovalRequired` is affected. `Admit` and `Deny` pass
through the function verbatim — no external verdict can widen a
hard-ceiling `Deny` into an `Admit`, and none can downgrade a clean
`Admit`. `Reject { reason }` replaces (does not append to) the
`ApprovalRequired` request list with a single, new, additive `DenyReason`
variant: `ExternalPolicyRejected { provider_id, reason }`.
`protected_criteria` is copied verbatim from the input `PolicyDecision`,
never recomputed.

**`DenyReason`'s new variant is additive-safe.** `DenyReason` derives
plain (externally-tagged) `Serialize`/`Deserialize`, not
`#[serde(tag = ...)]`; a pre-HORO-1174 `admission_json` row with an old
variant (e.g. `{"ResourceExceedsHardCeiling": {...}}`) still deserializes
under the new enum — verified by the
`deny_reason_deserializes_the_old_variants_after_the_additive_change`
regression test, not merely assumed from "it's additive."

The daemon only calls the Policy Webhook when the *projected* admission
is already `ApprovalRequired` — a hard-ceiling `Deny` issues **zero**
webhook requests, verified with a counting fake endpoint in the daemon
integration suite.

### 10. Signing is per-attempt, not per-enqueue — enforced by the schema, not convention

`SignedHeaders::fresh` mints a fresh `request_id`/`timestamp`/replay
marker and computes a fresh HMAC on **every delivery attempt**, called
from inside `crates/extension::dispatcher`'s per-attempt send — never at
enqueue time. This is not merely a coding discipline: the
`webhook_deliveries` table (migration `0009`) has **no**
signature/timestamp/replay-marker column at all. There is nothing to
persist and reuse across attempts even if a future change wanted to —
signing structurally has to happen fresh, inside the send path, because
the schema gives it nowhere else to live. `event_id` (minted once, at
enqueue via `EventEnvelope::new`) is the one value that *does* stay
stable across attempts, and it is what a receiver's own dedupe should key
on.

`signed_payload = "v1." + timestamp + "." + marker + "." + raw_body`,
signature = `hex(HMAC_SHA256(secret, signed_payload))`, header
`x-libra-signature: v1=<hex>`. A pinned test vector
(`sign::tests::pinned_signing_vector`) fixes secret/timestamp/marker/body
against a fixed expected hex output, so the signing contract cannot
silently drift. Per-attempt freshness is proven directly:
`two_calls_to_fresh_produce_different_markers_timestamps_and_signatures`
asserts two attempts of the identical payload produce different
signatures while the caller's own `event_id` stays fixed.

**Naming note.** The wire header is `x-libra-nonce` — a real, external
protocol term. Every *Rust* identifier in `crates/extension` is spelled
`marker` instead of `nonce`, to avoid a repeat of a real CodeQL
hardcoded-nonce-naming false positive this campaign has already hit on an
identifier name alone (see HORO-1150/1157 PR history). The wire contract
is unaffected — only the Rust-side spelling changes.

### 11. Secret custody mirrors `crates/gateway::credential`, deliberately not sharing code with it

`crates/extension::secret::WebhookSecret` copies
`crates/gateway::credential::UpstreamCredential`'s subprocess discipline
verbatim (stdin `/dev/null`, stderr captured and discarded unread, an 8
KiB cap, a 10s timeout, an error type whose `Display` never quotes
captured output) but is a genuinely different type with a genuinely
different exposure contract: `WebhookSecret`'s **only** accessor is `fn
sign(&self, payload: &[u8]) -> [u8; 32]`. There is no
`header_value()`-equivalent — this type is structurally incapable of
ever being placed literally on the wire, unlike `UpstreamCredential`,
which by design *is* placed on the wire (as a redacted `Authorization`
header). `crates/gateway`'s credential type was not refactored to share
code with this one (rule of three; ~150 lines of accepted duplication for
a real difference in what each type may be used for). `Debug` writes
`<redacted>`, verified both bare and nested inside a containing struct
that itself derives `Debug`
(`debug_never_renders_the_secret_even_when_nested`).

### 12. Loopback-only, no TLS, host literal — not `localhost`

Every configured extension URL's scheme must be the literal `http` and
its host the literal `127.0.0.1` or `::1` — **not** `localhost`, unlike
`crates/gateway::config`'s equivalent check (which does accept
`localhost` for its own plaintext-loopback exception). This is a
deliberate narrowing relative to the gateway's precedent: the gateway's
`localhost` acceptance exists for one specific historical reason (its own
test harness), while every extension surface here is genuinely new
surface with no such precedent to honor, so the literal-only check (no
DNS dependency at all) was the stricter, simpler choice from the start.
SSRF is structurally impossible: the destination comes only from
`config.json`, never from anything request-derived, and a loopback
literal needs no resolution step an attacker could ever influence. This
is also why there is no TLS dependency in `crates/extension` — a
loopback-only destination has no meaningful hostname to validate a
certificate against, and pulling in `hyper-rustls` to encrypt a
connection that never leaves the loopback interface would be security
theater, not security.

### 13. Latency budget — deviation from the ticket's stated caps, with the real numbers

The ticket's original design assumed 500ms/1000ms (default/cap) for the
business-context surface and 750ms/1000ms for the policy webhook, on the
stated assumption that `crates/cli/src/client.rs::REQUEST_TIMEOUT` is 5s
and `crates/daemon/src/recon.rs::ReconBudget::default().max_duration` is
3s (both verified true against the real code). Those caps sum to exactly
`3s + 1s + 1s == 5s` — **zero** slack against the client's own timeout,
before accounting for anything else `handle_preflight` does between the
recon call and the extension calls (estimator bucketing over the full
receipt history, three ledger writes, `reconcile_stale_reservations`).

This is shipped as **500ms/700ms** for the business-context surface and
**700ms/700ms** for the policy webhook — `3.0 + 0.7 + 0.7 == 4.4s`, plus a
named `REQUIRED_SLACK` constant of 500ms, giving `4.9s`, strictly under
the 5s `REQUEST_TIMEOUT` with 100ms of real margin. The
`recon_plus_both_caps_plus_slack_stays_under_the_request_timeout` test in
`crates/extension/src/config.rs` asserts this arithmetic with `<`, not
`<=`, so a future change to any of the four numbers that would violate it
fails loudly at compile-and-test time rather than silently degrading a
production preflight into a client-side timeout.

### 14. Config validation refuses to start extensions in an unenforceable shape; the daemon still fails open

`libra_governor_extension::validate` refuses: any scheme other than
`http`; any host other than the literals `127.0.0.1`/`::1`; userinfo,
query, or fragment in the URL; `timeout_ms` above the hard cap; an empty
`secret_command`; `events.max_attempts == 0`. Every refusal disables
extensions entirely and logs — the daemon keeps serving hooks, the
statusline, and the gateway exactly as before, matching the same
fail-open-for-the-daemon doctrine ADR 0003 §6 established.

**Validation is cached, not re-run per request or per `doctor` call.**
Unlike `crates/gateway::config::validate` (pure, no I/O),
`libra_governor_extension::validate` resolves each configured
`secret_command` by spawning a subprocess as part of validation. Doing
that on every `handle_preflight` call — or on every `libra-governor
doctor` invocation, which a user or a script might poll — would be a real
cost (a slow credential-manager round trip) and a real side effect
(repeated keychain prompts). `DaemonConfig::extension_runtime` is a
`std::sync::OnceLock<ExtensionRuntime>`, forced exactly once by `serve()`
before the accept loop begins (mirroring `start_gateway`'s timing), and
`DoctorResult::extension_config_error` reports that **cached** startup
error rather than a freshly re-derived one. This is a deliberate
deviation from `DoctorResult::config_file_error`'s "fresh re-read every
call" precedent, made because the two validations have genuinely
different I/O costs — `config_file_error`'s re-read is a cheap JSON
parse; extension validation is not.

### 15. `webhook_deliveries` dedupe key collision — a disclosed, accepted simplification

Per the approved design, `dedupe_key = plan_id` for
`admission`/`approval`/`replan` events and `dedupe_key = task_id:plan_id`
for `outcome` events. This means a `governor_local`-sourced outcome event
(enqueued by `handle_finalize`) and a later `provider`-sourced outcome
event (enqueued by `handle_record_outcome`) for the **same** `task_id`
and `plan_id` share one dedupe key — the daemon only ever delivers the
first outcome event enqueued for a given plan; a second, different-source
outcome for that same plan is silently deduped by
`INSERT OR IGNORE` and never reaches the events surface. This is not a
bug introduced during implementation; it is the literal dedupe-key
formula the approved design specifies, and it is disclosed here as a
known limitation rather than silently accepted. A future ticket wanting
"every distinct outcome source delivered" would need to widen the
`outcome` dedupe key to include `source`/`source_id`.

### 16. Never re-fetch business context on replan; no `tool_invoked` event kind

`handle_tool_invoked` makes no HTTP call and enqueues no business-context
fetch — only `handle_preflight` ever calls the Business Context Provider.
A replan works from the same context a task was originally admitted
under; re-fetching per replan would multiply provider load for a value
that changes on the order of a task's business priority, not its
tool-call count.

No `tool_invoked` event kind exists. `Request::ToolInvoked` promises no
perceptible added latency on every tool call
(`crates/cli/src/client.rs::fire_and_forget`'s documented contract,
`handle_tool_invoked`'s own module doc). A durable ledger enqueue on
every tool call — even a cheap SQLite insert — is I/O this path cannot
spare. Every tool call that matters downstream already surfaces through
the `replan` event kind exactly when a material replan actually happens.

## Non-goals (explicit, with the concrete reason each is out of scope)

- **No Team Control Plane / cloud service.** Structurally enforced, not
  just policy: config validation refuses any non-loopback-literal host.
  A later cloud path is a reviewed ADR change, not a config toggle.
- **No multi-tenancy.** N/A at single-daemon, single-machine scope.
- **No Python/TS SDK generation.** The OpenAPI document
  (`docs/api/libra-extension-v1.yaml`) plus the 5-header HMAC scheme plus
  the ~150-line example provider (`examples/local-providers/libra_example_provider.py`)
  is the whole integration contract a real provider author needs.
- **No WASM/plugin ABI.** Nothing loads foreign code into the daemon
  process — every surface is out-of-process HTTP or a CLI subprocess
  invocation (`libra-governor outcome record`).
- **No OPA/Rego/policy DSL.** The Policy Webhook is a plain HTTP callback
  with a three-value verdict, not a policy language.
- **No new inbound HTTP listener anywhere.** Outcome push reuses the
  existing Unix socket — see §2.
- **No outbound outcome *pull* at finalize time.** Push-only. CI/deployment
  evidence essentially never exists yet at `Stop` time (the agent session
  ends long before a CI run does), so a pull at finalize would almost
  always return nothing; a provider-initiated push, whenever its own
  evidence actually lands, is the honest shape.
- **No `tool_invoked` event kind.** See §16.
- **No business-context refetch on replan.** See §16.
- **No policy *relaxation* via business context.** Narrowing only,
  monotone — see §§5–6.
- **No webhook delivery-row pruning.** Accepted accumulation cost,
  disclosed — same doctrine `crates/gateway`'s `gateway_requests` table
  already accepts for its own provenance rows.

## Consequences

### Gained

- A real, tested, narrowing-only lever for external business context to
  affect admission, with the trust boundary enforced by tests that touch
  the real admission path, not merely unit tests of the pure function.
- A real Policy Webhook escalation path that cannot widen a hard-ceiling
  `Deny` and issues zero requests when nothing requires a human decision.
- A real outcome-push path reusing existing infrastructure (the Unix
  socket, the ledger) rather than adding a second inbound surface.
- Signed, replay-resistant, retried event delivery with a schema that
  makes "sign at enqueue time" structurally impossible rather than merely
  discouraged.

### Accepted costs

- The latency-budget caps are tighter than the ticket originally
  specified (§13) — a real, disclosed reduction in how long a
  slow-but-legitimate provider is given to answer before the daemon gives
  up on it.
- `doctor`'s `extension_config_error` can go stale relative to a
  `config.json` edited after daemon startup without a restart — same
  known limitation `running_config_matches_disk` already names for policy
  and gateway configuration, extended here rather than solved differently.
- The `outcome` event dedupe key can silently drop a second, different-
  source outcome event for the same plan (§15) — disclosed, not solved,
  in this ticket.
- `source_id` on `Request::RecordOutcome` is a claim, not an authenticated
  identity (§2) — the same boundary every other local-socket `Request`
  already has, now extended to one more surface.

## Alternatives considered and rejected

- **A new inbound HTTP listener for outcome push**, mirroring the
  gateway's shape. Rejected: the existing Unix socket already provides
  the exact "receive one attestation from a local process" capability at
  zero additional listener surface, and duplicating the gateway's
  loopback-security machinery for a second listener would be new attack
  surface with no new capability behind it.
- **Sharing `crates/gateway::credential`'s type for the webhook secret.**
  Rejected (rule of three) — see §11.
- **Re-validating `[extensions]` fresh on every `doctor` call**, mirroring
  `config_file_error`'s discipline exactly. Rejected — see §14: the two
  validations have genuinely different I/O costs, and re-exec'ing a
  configured `secret_command` (potentially a slow credential-manager
  round trip) on every diagnostic call is a real, avoidable cost.
- **Claiming R2 as a type-level impossibility.** Rejected as overclaiming
  — see §5. The honest claim is a tested value invariant plus the
  absence of a conversion function, and that is what is actually shipped
  and actually tested.
