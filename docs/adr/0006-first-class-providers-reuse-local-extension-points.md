# ADR 0006: First-Class Providers Are Loopback Processes Against the Existing Extension Contract, Not New Daemon Capability

- **Status:** Accepted
- **Date:** 2026-10-02
- **Ticket:** HORO-1173

## Context

HORO-1174 (ADR 0005) built the Business Context Provider fetch, the
Policy Webhook call, and signed outcome-event delivery as outbound-only,
loopback-HTTP, HMAC-signed integration points, together with one
reference implementation (`examples/local-providers/libra_example_provider.py`)
that proves the contract end to end.

HORO-1173 asks for two *first-class* providers — Jira and GitHub — so a
task's real external tracker and real pull-request/CI state can inform
admission and attest outcomes, instead of only the example's static,
illustrative data. The open design question: does reaching a real
external HTTPS API (Jira Cloud, GitHub REST) require new daemon-side
code, a new trust boundary, or a change to `docs/api/libra-extension-v1.yaml`?

## Decision

**No daemon-side or protocol change.** A first-class provider is exactly
the same kind of process `libra_example_provider.py` already is: an
operator-run, loopback-bound, HMAC-verifying HTTP server that the daemon
calls as a client, per ADR 0005. The only new thing a first-class
provider does that the example provider doesn't is make its own
*outbound* call to a real external API, using its own operator-supplied
credentials, before answering the daemon's inbound request. That outbound
call is entirely the provider process's own business — the daemon never
sees it, never proxies it, and never gains a new dependency on Jira or
GitHub being reachable.

This keeps HORO-1174's trust boundary exactly where ADR 0005 put it: "the
provider informs; the daemon decides" is still a property of the
dependency graph, because a first-class provider still only ever returns
a `BusinessContextResponse`/outcome event shaped exactly like the
example's, validated against the same schema, subject to the same R1/R2
rules (`BusinessContext` never merges into `TaskFeatures`;
`advisory_criteria` never enters `quality_floor`).

### 1. Shared security scaffolding is extracted, and the example provider was updated to actually use it once duplication was measured, not just described as intentional

`providers/common/libra_provider_runtime.py` is new, but it is an
*extraction* of logic `libra_example_provider.py` (HORO-1174, already
merged, already CI-green, already reviewed for exactly this HMAC/replay
surface) proves correct. This ADR's first draft stopped at "extracted,
not an edit to that file" — on the theory that `libra_example_provider.py`
keeping its own copy was an acceptable, documented tradeoff. A
SonarCloud quality-gate failure on this PR's own diff measured that
tradeoff's actual cost (real, flagged duplication between the two
copies) and the right correction was to finish the extraction:
`libra_example_provider.py` now subclasses `BaseProviderHandler` and
calls `resolve_ticket_key` from the shared module, with its own test
suite passing unmodified and a manual signed-request round-trip
confirming identical wire behavior. A second real adapter reusing the
same module means the signature-verification and replay-rejection code
path has exactly one implementation to review, not two that could
silently diverge — now true of the reference provider as well, not just
the new adapters. A third future adapter (Linear, Asana, whatever comes
next) extends this module's
`BaseProviderHandler`, it does not re-derive HMAC verification again.

### 2. Each adapter owns a narrow, named, auditable credential surface

A provider's OAuth/token scope is the smallest the adapter's actual
behavior requires, and is stated once, in the module docstring and in the
Jira evidence comment, rather than left implicit:

| Provider | Scopes required | Why |
|---|---|---|
| Jira | `read:jira-work` | Fetching issue fields for Business Context |
| Jira | `write:jira-work` (only with `--enable-write-back`) | Posting a receipt comment on outcome |
| GitHub | `pull_requests: read`, `checks: read` | Resolving the PR for a branch and its combined check status |

GitHub's adapter is read-only by design — AC "GitHub PR/CI/merge evidence
can attach to the same `TaskIdentity`" is satisfied by reading objective
PR/check state (`merged`, `state`, combined check status), never by
writing to the PR, issue, or any check run. Jira's adapter has exactly
one mutating code path (an outcome receipt comment), and it is opt-in,
off by default, gated behind a CLI flag an operator must pass explicitly.

### 3. The outbound call gets its own SSRF-safety discipline, symmetric to but distinct from ADR 0005's inbound one, and closed against DNS-rebinding and redirect bypass specifically

`providers/common/safe_https_client.py` validates every real request
before it connects: must be `https`, must have no embedded credentials,
and — unless `allow_private_network=True` is passed explicitly for a
genuinely self-hosted Jira/GitHub Enterprise instance — must not resolve
to a loopback, link-local, private, reserved, or multicast address. This
is the mirror image of ADR 0005's loopback-only rule: there, the daemon's
*inbound* target must only ever be loopback; here, the provider's
*outbound* target must only ever be a real public (or deliberately
opted-in private) endpoint.

A "validate the configured URL once, then let the HTTP client do its own
thing" design has two well-known SSRF bypass gaps a review of this ADR's
first draft caught, and both are closed structurally, not by convention:

- **DNS rebinding / TOCTOU.** If validation resolves a hostname and the
  actual connection independently re-resolves it moments later, an
  attacker controlling DNS for that hostname can answer safely the first
  time and unsafely the second. `_resolve_validated_ip` is the **only**
  resolution call in the real request path, and `_PinnedHTTPSConnection`
  connects directly to the IP that call validated — there is no second,
  independent resolution anywhere in between for a rebinding attacker to
  race. (TLS SNI and certificate hostname verification still use the real
  hostname via `server_hostname`, so pinning the socket to an IP does not
  weaken certificate validation.)
- **Redirect-following bypass.** Letting the HTTP client auto-follow
  redirects without re-validating the `Location` target reopens exactly
  the hole validation exists to close — a safe initial URL can redirect
  to `169.254.169.254` or similar. `_request_with_validated_redirects`
  never delegates to automatic redirect handling: it follows redirects
  itself, in a bounded loop, and every hop goes through the same
  validate-then-pin `_request_once` call as the initial request, with no
  exception for "it's just a redirect."

Treating "operator config" as still requiring per-call validation, rather
than a one-time startup check, follows the same discipline this repo
already applies to request-supplied data — a provider that loads its base
URL from a file an attacker could modify
deserves the same protection as one reading from a request body.

### 4. Ticket-key and repo/branch resolution is selector, never value — reusing HORO-1174's proven pattern

Both adapters resolve *which* external resource a request concerns from
the **operator-supplied `--workspace-root`** (a trusted, local CLI
argument fixed at provider startup), matched against the request's `cwd`
only as an equality check — never by parsing or trusting any
request-supplied path as a lookup key. This is the exact "selector, not
value" shape `libra_example_provider.py`'s `resolve_ticket_key` already
established and that shape's SonarCloud command-injection/filesystem-
oracle findings already forced it to adopt. Jira's ticket key comes from
the real current git branch name (`git rev-parse --abbrev-ref HEAD`) at
that trusted root; GitHub's `owner/repo` comes from the real `git remote
get-url origin` at that same trusted root. A request can select *that* an
answer is wanted; it can never select *which* external resource the
answer is about.

### 5. Wire-shape correctness is enforced by reading the real OpenAPI contract, not by analogy

`docs/api/libra-extension-v1.yaml`'s `Priority` enum is closed
(`low`/`normal`/`high`/`urgent`); Jira's own priority scheme names
(`Highest`/`High`/`Medium`/`Low`/`Lowest` by default, but instance-
configurable) are never passed through. `JIRA_PRIORITY_TO_WIRE_PRIORITY`
maps the default scheme explicitly; an unmapped Jira priority name is
left absent from the response rather than guessed, consistent with this
campaign's "never fabricate" rule applied to a schema boundary instead of
an evidence claim. `ExternalRef.kind` is `[jira, github, other]` exactly
as the schema defines it — plain lowercase, not a `snake_case`-rendered
guess at how Rust serde would handle a `GitHub` variant name.

## Consequences

- Zero changes to `crates/extension`, `crates/daemon`, or
  `docs/api/libra-extension-v1.yaml` are required for HORO-1173. The
  entire PR is new Python (two adapters, two shared modules) plus docs.
- A future enterprise custom adapter can be built by reading
  `docs/api/libra-extension-v1.yaml` plus this ADR plus
  `providers/common/`, without forking any of Libra's core Rust crates —
  satisfying this ticket's "documented sufficiently for a future
  enterprise custom adapter without forking core" acceptance criterion.
- `providers/common/libra_provider_runtime.py` becomes the one place
  future adapters extend for inbound HMAC/replay handling; `safe_https_client.py`
  becomes the one place they extend for outbound SSRF-safe calls. Neither
  is a generic HTTP framework — both exist only to keep this one, narrow
  trust boundary from being re-derived per adapter.
