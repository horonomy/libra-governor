# ADR 0003: The Provider Gateway Is the Hard Enforcement Point, and It Decides Nothing

- **Status:** Accepted
- **Date:** 2026-09-18
- **Ticket:** HORO-1144

## Context

`ARCHITECTURE.md` already names an "optional enforcement gateway" in the
system shape:

```
Claude Code -> deterministic hooks/statusline -> local Governor daemon
            -> optional enforcement gateway -> LLM provider
```

and states the rule that governs it: *"The gateway enforces; it does not
decide."* Up to HORO-1141 that component did not exist. Every gate Libra
had was **advisory**: a hook can print a preflight summary into Claude
Code's context, and `Policy::evaluate` can return `Deny`, but nothing
physically prevented the agent from spending the money anyway. HORO-1141
made the ledger atomic and gave every task a protected Completion
Reserve, but the reservation it takes is a whole-plan *envelope* recorded
at `Preflight` time — it is bookkeeping about work that has already been
authorized to run, not a checkpoint each provider call must pass.

HORO-1144 adds the missing checkpoint. Because this is the first
component in the repository that sits on the wire between the agent and a
paid provider — and the first that holds a real upstream credential —
several decisions need recording, not just the fact that a gateway
exists.

Per ADR 0001's convention, this ADR states which component each part of
the feature belongs to before any of it is built.

## Decision

### 1. A library crate run on a thread inside the daemon process

`crates/gateway` is a **library** crate. It is not a second binary and
not a separate service. `crates/daemon`'s `serve()` starts it on a
`std::thread` running its own Tokio multi-threaded runtime, when and only
when `DaemonConfig.gateway` is `Some`.

Three alternatives were rejected:

- **A second binary.** Two processes means two lifecycles, two crash
  modes, two sets of stale-socket logic, and a new question ("is my
  gateway running?") the user has to answer. The daemon already solves
  spawn-if-absent; reusing it costs nothing.
- **Inside the daemon's existing accept loop.** `crates/daemon`'s server
  is deliberately single-threaded and serial (see its module docs): it
  handles one Unix-socket request completely before accepting the next.
  A streaming Anthropic response is long-lived by design — minutes, with
  deliberate silence during extended thinking. Serving that from the
  serial loop would block every hook and statusline refresh for the
  duration. The two workloads have incompatible shapes and must not
  share a loop.
- **A Cargo feature flag.** Rejected deliberately. A feature flag on a
  security-critical path produces a configuration that is never compiled
  in CI and therefore never tested. The gateway is gated at **runtime**
  by `DaemonConfig.gateway: Option<GatewayConfig>` (default `None`), so
  the enforcement code is compiled, linted, and tested on every build
  whether or not a given user turns it on.

### 2. The gateway holds no policy logic

`crates/gateway` depends on `libra-governor-domain` for value types and
on nothing else of Libra's. It cannot read the ledger, cannot construct a
`Policy`, and cannot decide an admission. It asks, through the
`SpendAuthority` trait it defines, and obeys the answer.

`crates/daemon`'s `LedgerSpendAuthority` is the only implementation. It
is the component that evaluates `Policy` and touches `LedgerStore` —
exactly where `ARCHITECTURE.md` says admission decisions live. This keeps
"the gateway enforces; it does not decide" a *structural* property (the
gateway crate has no dependency through which it could decide) rather
than a convention a later change could quietly break.

The gateway opens its **own** `LedgerStore` against the same SQLite file,
behind an `Arc<Mutex<..>>`, and calls it from `tokio::task::spawn_blocking`.
`rusqlite::Connection` is not `Sync` and the ledger crate's own docs
prescribe one store per thread. HORO-1141's WAL + `busy_timeout=5000` +
`BEGIN IMMEDIATE` design is what makes two connections to one file safe,
and its `reservation_concurrency.rs` tests already prove it.

### 3. Gateway reservations are `OptionalWork`, with `plan_id: None`

Two consequences follow, both deliberate.

**`ReservationClass::OptionalWork`.** An HTTP request body does not tell
the gateway whether the tokens it is about to buy will satisfy a required
`CompletionCriterion` or explore a dead end. Since it cannot know, it
must assume the weaker claim. `OptionalWork` can never draw against the
protected Completion Reserve (see `LedgerStore::reserve`), so "preserve
Completion Reserve" is satisfied *structurally* — by the class tag, not
by a subtraction someone has to get right. The cost is real and accepted:
a genuinely required completion call is refused once ordinary headroom is
gone, even though the reserve exists precisely for such work. Classifying
it `RequiredWork` instead would let any request whatsoever drain the
reserve, which is strictly worse.

**`plan_id: None`.** `handle_tool_invoked`'s replan path calls
`release_active_for_plan(task_id, plan_id, now)`, which releases *every*
`active` reservation carrying that plan id. A gateway reservation tagged
with the in-flight plan would be released out from under a live HTTP
request by a replan firing mid-stream; the later settle would return
`AlreadyFinal` and the spend would silently vanish from the ledger. A
gateway reservation therefore carries no plan id. Its lifetime is one
HTTP request, not one plan.

### 4. Gateway reservations replace the plan envelope; they never stack

When the gateway is enabled, both existing `ledger.reserve(...)` call
sites in `crates/daemon/src/server.rs` (`handle_preflight`'s admission
reservation and `handle_tool_invoked`'s replan re-reservation) are
skipped. They are guarded on `config.gateway.is_none()`.

Reserving a whole-plan envelope *and* a per-request amount for every call
inside that plan double-counts the same spend against one `hard_limit`,
and the task would be denied at roughly half its real budget. The
plan-level envelope is the right instrument when nothing meters the
actual calls; per-request reservation is strictly better information, and
it supersedes rather than supplements.

### 5. Credential custody: the agent never holds the provider key

Claude Code is pointed at the gateway with
`ANTHROPIC_BASE_URL=http://127.0.0.1:<port>` and an `apiKeyHelper` that
prints a **local capability token** — 32 bytes of OS randomness generated
at first gateway start, persisted at `$STATE_DIR/gateway.token` with mode
0600, compared in constant time. That token authorizes use of the local
gateway. It is not a provider credential, has no value off this machine,
and buys nothing by itself.

The real upstream credential is resolved by the daemon from a
user-configured **credential command** (`security find-generic-password
...`, `pass show ...`, `op read ...`) whose stdout is read directly into
a `UpstreamCredential` newtype with no `Display`, no `Serialize`, no
`Deserialize`, and a `Debug` impl that writes `<redacted>`. It is never
written to the ledger, never logged at any level, and never appears in
Claude Code's environment, arguments, or configuration.

`CredentialError`'s `Display` never includes the command's captured
stdout or stderr — a failing credential command is exactly the situation
where a secret is most likely to be sitting in an error stream.

### 6. Fail-closed at the gateway, fail-open for the daemon

These are different questions and get different answers.

- **The gateway's own admission fails closed.** Anything it cannot
  enforce exactly, it refuses: no `max_tokens`, no task binding, an
  unpriced model against a USD budget, a `QuotaPercent` budget, a policy
  `Deny`, insufficient headroom — all become `403` *before* any upstream
  call. Forwarding a request it cannot account for would make the
  enforcement claim false.
- **The daemon's core function fails open.** If gateway config validation
  or credential resolution fails at startup, the daemon logs it, leaves
  the gateway disabled, and keeps serving hooks and the statusline
  normally. A mistyped upstream host must not take away preflight,
  estimation, and the ledger.

`403` is used for every rejection, never `429`. Claude Code treats `429`
as a rate limit and retries with backoff — which would turn one budget
refusal into a retry storm against a boundary that will refuse every
time. The body is Anthropic-shaped (`{"type":"error","error":
{"type":"permission_error","message":"libra-governor: ..."}}`) so the
agent renders it as a real error, and `x-libra-decision` /
`x-libra-request-id` headers carry the machine-readable reason without
leaking any secret.

### 7. SSRF defense: the destination is configuration, never input

The upstream URL is **never** derived from the inbound request. A closed
route table of exact literal paths (`match_route`) maps an inbound path
to a `Route`; the outbound URL is built from the configured upstream base
plus that *route's own* canonical path constant plus the original query
string. No inbound path segment is ever concatenated into an outbound
URL.

`validate()` refuses to start the gateway when the configured upstream
has a non-`https` scheme, a host outside `upstream_host_allowlist`
(default `["api.anthropic.com"]`), userinfo, a non-443 port without
`allow_nonstandard_port`, or any path/query/fragment; when `bind_addr` is
not loopback; or when `PassThroughSubscription` credential mode is
combined with a `Usd`-kind admission policy (see §8). Upstream redirects
are never followed — a `3xx` is relayed to the client verbatim.
`x-forwarded-*` and `forwarded` headers are dropped. The `Host` header
must equal the configured listen authority, checked *before* routing.

**TLS hostname verification is the real destination binding.** A resolved-IP
allowlist is explicitly *not* implemented, and this is a considered
omission rather than an oversight: hyper-rustls validates the upstream
certificate against the configured hostname, so a DNS rebind that points
`api.anthropic.com` at `127.0.0.1` fails certificate validation and the
connection never carries a byte. An IP guard would add a TOCTOU race
(resolve, check, connect — the resolution can change in between) for a
property TLS already guarantees end-to-end.

### 8. Enforcement tiers are configured, never auto-negotiated

`libra_governor_domain::capability` defines three `EnforcementTier`s:

| Tier | Credential custody | Usage accounting | Monetary cap |
|---|---|---|---|
| `GatewayMetered` | Governor-held API/BYOK key | Provider-reported | Enforced, at a pinned `pricing_version` |
| `GatewayObservedQuota` | Agent-held subscription OAuth, forwarded unchanged | Provider-reported | **Not available** — opaque provider quota |
| `HooksOnly` | Not applicable | None | **Not available** — no pre-spend enforcement point |

The tier follows from `GatewayCredentialMode`, which the user sets. It is
never sniffed or negotiated at runtime: a boolean "supported / not
supported" would be a lie in both directions, and a runtime guess about
which tier applies would be a worse lie.

The honesty claim is enforced by **code, not documentation**:
`validate()` refuses to start the gateway in `PassThroughSubscription`
mode when the admission policy's `ResourceKind` is `Usd`. That
combination would advertise a hard monetary cap the system cannot honor,
because a subscription's own accounting is not exposed to us. A
`MonetaryEnforcement::NotAvailable { reason: NoMonetaryCap }` is a value
the system reports, not a caveat in a README.

### 9. Approximation always errs toward reserving too much

Pre-flight, the gateway knows the request body's byte length, the
declared `max_tokens`, and the model. It does not know the tokenizer's
output, and it does not know which cache tier the input will land in.

`CONSERVATIVE_BYTES_PER_TOKEN = 3.0` is a deliberate *lower* bound on
bytes per token, which makes `ceil(body_bytes / 3.0)` an *upper* bound on
input tokens. For a `Usd` budget the reservation prices input at the
**cache-creation** rate — the most expensive input tier — since we cannot
know pre-flight which tier applies. Settlement then corrects downward
from exact provider figures: `message_start.usage` for input,
the final cumulative `message_delta.usage` for output.

If settlement ever observes `output_tokens > max_tokens`, that is an
assumption of ours being violated by reality. It is logged as
`bound_violation` and recorded as `gateway_requests.bound_violated = 1`
rather than silently clamped, so the assumption is self-checking.

### 10. No `Drop` guard for settlement

A `Drop` impl cannot `await`, and settling goes through `spawn_blocking`.
Settlement is therefore explicit on every state-machine exit path.
HORO-1141's `expire_stale_reservations` is the crash backstop, and
`gateway_reservation_ttl_secs` defaults to 600 — shorter than the
plan-level 900 — so a crashed in-flight request's capacity returns
quickly.

### 11. Provenance rows carry scalars only

`crates/ledger/migrations/0007_gateway_requests.sql` records one row per
terminal transition: ids, enum tags, token counts, amounts, a status
code, timestamps. There is **no** column for a request body, a response
body, a header, a prompt, or tool output — the same structural privacy
guarantee `0001_init.sql` makes, extended to the one component that
touches data leaving the machine. There is also no debug body-dump
logging flag, at any level; adding one later would be a change to this
ADR, not a configuration tweak.

`task_id` on that table is **nullable and carries no foreign key**: a
request rejected for a missing task binding, ambiguous credentials, a
route miss, or a `Host` mismatch has no task, and those rejections are
precisely the rows an auditor most wants to see.

## Deviations from the approved design

**Plaintext loopback upstream.** The design states `validate()` rejects
any scheme other than `https`. Implemented as stated for every real
upstream — with one narrow, explicit exception: an
`allow_plaintext_loopback_upstream: bool` config field (default `false`)
permits `http://` **only** when the upstream host is the literal
`127.0.0.1`, `::1`, or `localhost`. Any non-loopback host is refused
regardless of the flag, so the flag alone cannot be turned into an
exfiltration path.

The rationale is that the mandated test surface — `crates/gateway/tests/
fake_upstream.rs`, a local hyper server serving canned SSE and error
responses — cannot exist otherwise. The alternatives were worse: a
`#[cfg(test)]` hatch is unavailable to integration tests (separate
crates), a Cargo feature would reintroduce exactly the untested-security-
path problem §1 rejects, and generating a local CA to serve real TLS in
tests adds a certificate-minting dependency to a security-critical crate
to test a path the certificate machinery is not what we are testing. The
safety of the exception rests on the loopback *host literal* check, not
on the flag.

## Consequences

### Gained

- A real pre-spend boundary. A request that would exceed a hard budget is
  refused before a byte reaches the provider, rather than noticed
  afterward in a receipt.
- Exact settlement. HORO-1141 settles with `usage_known = false` and the
  conservative reserved amount, because Claude Code's hook payloads carry
  no token counts. Gateway traffic settles from the provider's own
  reported usage — the first genuinely exact spend data in the ledger.
- Honest capability reporting, backed by a validation refusal.

### Accepted costs

- **Per-request over-reservation under parallelism.** N concurrent
  subagent requests each reserve their own worst case against one
  `TaskBudget`. Their *sum* can exhaust the budget and cause a refusal
  even though real usage would have fit comfortably. Not solved in this
  MVP beyond the short TTL; a future ticket can pool or predict.
- **Crash loses one request's spend record.** If the process dies
  mid-request, the TTL returns the capacity but that request's actual
  spend is never settled into the ledger. Capacity accounting recovers;
  the spend figure is under-counted by that one request.
- **Pricing is a pinned snapshot, not a feed.** `PRICING_VERSION`
  identifies a static table recorded on every reservation, settlement,
  and provenance row. When upstream prices change, the table is stale
  until someone updates it. A live pricing feed would be a network
  dependency on the enforcement path and a new trust boundary; a pinned,
  versioned, auditable number is the honest claim.
- **Anthropic only.** Bedrock, Vertex, Foundry, and every non-Anthropic
  provider are out of scope. `GET /v1/models` is not implemented (Claude
  Code does not use it by default).
- **Approval is visible, not actionable.** An `ApprovalRequired`
  admission forwards the request and surfaces the fact through
  `GatewayStats`. The proxy has no channel to interrupt a human
  mid-request; building one is a UX ticket, not a proxy ticket.
- **Loopback plain HTTP on the client side.** No TLS or mTLS is offered
  for the Claude-Code-to-gateway hop. Claude Code does not route loopback
  traffic through its own trust store, so a self-signed local certificate
  would fail; the capability token plus a loopback-only bind is the
  boundary.

## Alternatives considered and rejected

- **Enforce through MCP.** Rejected on the standing architecture rule:
  MCP is explain/query/manual-control only and is never a security or
  economic boundary. An LLM choosing to call a tool is cooperation, not
  enforcement.
- **A general-purpose LLM router.** Rejected on `PRODUCT.md`'s explicit
  non-goal. This gateway exists to refuse unaffordable calls, not to
  compare vendors or route by quality.
- **Estimate spend from hook payloads instead of proxying.** This is what
  HORO-1141 already does, and it is why `usage_known` exists. It cannot
  be a *pre-spend* boundary: by the time a hook fires, the money is gone.
