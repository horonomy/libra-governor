# ADR 0017: Outcome Attestation Authority Requires a Separate Principal

- **Status:** Accepted
- **Date:** 2026-10-10
- **Ticket:** HORO-1727

## Context

HORO-1174 (ADR 0005 §2, §7) shipped `Request::RecordOutcome`: an
operator-pointed Outcome Provider (CI, a deployment system, a human
reviewer's tooling) pushes a completion claim over the daemon's existing
0600 Unix socket, and the daemon records it as an `outcome_attestations`
row. ADR 0005 §7 stated only `AttestationSource::Provider`/`GovernorLocal`
are authoritative, and that `AttestationSource::Agent` has no production
caller — both true at the type level.

HORO-1727's bounded task-budget renewal mechanism (ADR unwritten at the
time; see `crates/ledger/src/renewal.rs`) added a `TaskAlreadyCompleted`
refusal gate that reads `outcome_attestations` for `authoritative=1 AND
outcome_kind='completed'`. This is the first place an attestation's
authority became economically load-bearing rather than purely advisory
— `grant_renewal` has no production caller yet (`renewal_not_wired_live.rs`
guards this), so nothing live changes today, but the gate's correctness
now depends on what "authoritative" actually means.

A disposable canary (`crates/daemon/tests/outcome_attestation_canary.rs`,
PR #112) proved two facts about the real, merged code — not by
inspection, by running real requests against a real daemon dispatch
path:

1. `handle_record_outcome` unconditionally constructs
   `AttestationSource::Provider { provider_id: source_id }` for **every**
   push. There is no branch, anywhere in the real protocol, that can ever
   construct `AttestationSource::Agent`. ADR 0005 §7's own claim that
   `handle_finalize`'s outcome event attributes to `GovernorLocal` is also
   false — `handle_finalize` never constructs an `AttestationSource` at
   all (corrected in PR #109; this ADR corrects ADR 0005 §7 to match).
2. `promote_receipt_outcome` has no ordering or conflict check. Two
   independently-keyed attestations disagreeing about the same task both
   get written to `outcome_attestations` (the audit trail is intact), but
   `receipts.outcome_json` silently reflects whichever push landed last.

The founder's response (HORO-1727 Jira comment 26954) is the binding
decision this ADR implements: the current "every push is authoritative"
behavior is not acceptable as a production trust boundary, and must be
corrected to the smallest compatible design, independently reviewed
before implementation.

## The actual trust boundary, checked against this real deployment

This section states what ADR 0005 §2 called an "honest limitation" more
precisely, because HORO-1727's decision requires it: **can "provider
credentials not accessible to the governed agent" actually hold, on this
specific deployment, today?**

This daemon and the governed coding-agent session that talks to it run
as the **same OS user**, on the **same machine**, with the daemon's state
directory (`~/.local/state/libra-governor/`, confirmed `drwx------`)
owned by and readable/writable only by that one user. Given that:

- **A same-user process can write the ledger directly**, bypassing the
  socket and any wire-level signature scheme entirely:
  `sqlite3 ledger.sqlite3 "INSERT INTO outcome_attestations (...) VALUES
  (..., 1, ...)"` requires no daemon cooperation at all. Migration 0017's
  append-only triggers on `task_budget_renewals` block `UPDATE`/`DELETE`,
  not `INSERT` into a *different* table — they do not protect
  `outcome_attestations` from a direct insert (migration 0009 added that
  table with no triggers at all).
- **A same-user process can read whatever secret the daemon reads.**
  Any `secret_command` (matching the existing outbound extension
  pattern in `crates/extension/src/secret.rs`) that the daemon executes
  to obtain a signing key is a command the governed agent's own shell
  can also execute, as the same user, with the same permissions. A 0600
  secret file is not a boundary against the user that owns it.
- **A same-user process can edit `config.json` and restart the daemon**,
  installing its own trust anchor before the "real" one is ever
  configured.
- **A same-user process can replace the daemon binary** on disk (subject
  only to filesystem permissions that same user already has).

An HMAC scheme has a further, independent problem even ignoring the
above: **whoever can verify an HMAC holds the same key that can forge
one.** A verifier-side secret is not asymmetric; it is not a credential
that can be handed to a daemon process without also handing it to
anything that can read that daemon process's inputs. An asymmetric
scheme (e.g. Ed25519, where the daemon holds only a public key) would
close *this specific* problem — a verifier cannot forge — but would not
close the direct-ledger-write problem above, which requires no
cryptographic material at all.

**Conclusion: on this single-workstation, same-OS-user deployment, no
code-level change can make "provider credentials inaccessible to the
governed agent" true.** A real boundary requires the daemon, the ledger
file, and the provider's signing key to be under a principal the agent
cannot write to — a separate OS user, a container the agent cannot
`exec` into, or a genuinely external/remote service. None of those exist
in this deployment today.

## Decision

### 1. Authoritative production completion stays structurally disabled on this deployment

Per the founder's own fallback clause: if independent provider identity
cannot be enforced here, authoritative completion stays off, proven only
in an isolated test environment. This ADR's implementation (PRs 2-5,
tracked under HORO-1727) makes this the *only* reachable state in
production:

- Every real `RecordOutcome` push becomes non-authoritative
  (`AttestationSource::Agent`, never `Provider`) — see PR 2.
- A verified-signature path (`AttestationSource::Provider`) exists in
  code and is fully tested, but its configuration
  (`ExtensionConfig.outcome_authority`) is parsed nowhere in
  `config_file.rs` — there is no way to turn it on from this daemon's
  real config-loading path. See PR 5. This proves the verification
  *logic* and its fail-closed behavior; it does not and cannot prove
  principal separation, because none exists here. The canary built for
  PR 5 is labeled accordingly — it is not evidence that this mechanism
  is safe to activate on a shared-principal deployment.
- `renewal_not_wired_live.rs` (already shipped) continues to guarantee
  `grant_renewal` has no production caller regardless of attestation
  authority, so this ADR does not change the renewal mechanism's own
  live-wiring status.

### 2. HMAC, not asymmetric signing, for the verification logic that is built (but not activated)

Chosen deliberately, with the trade-off above disclosed rather than
hidden: HMAC-SHA256 matches every other signed surface already in this
codebase (`crates/extension/src/sign.rs`), reuses the exact tested
primitive rather than introducing a second cryptographic scheme, and the
asymmetric alternative would not close the direct-ledger-write problem
this ADR already establishes cannot be closed by any code-level change
on this deployment. When a genuinely separate-principal deployment is
built, that is the point to revisit whether the verifier-held-key
property matters enough to introduce Ed25519 — not before, since no
secret exists yet for either scheme to protect.

### 3. `outcome_attestations` becomes genuinely append-only (not append-only by convention)

Migration adds the same `BEFORE UPDATE`/`BEFORE DELETE` `RAISE(ABORT)`
trigger pattern migration 0017 already established for
`task_budget_renewals`. This does not close the direct-INSERT bypass
described above (nothing can, on this deployment) — it closes the
narrower, always-worth-closing gap that a row, once written, cannot be
silently altered or removed through the daemon's own normal operation or
a future code path that forgets this invariant.

### 4. Attestations bind to a contract revision; conflicting outcomes are detected, never silently resolved

See the Jira decision packet (HORO-1727 comment 26954, Decision 2) for
the full requirement. Implementation: PR 3 (schema + detection), PR 4
(the renewal gate reads the correct revision).

### 5. Decision A (what counts as a new "revision") is an open precondition, not resolved here

Checked against the real code: `handle_preflight_prepared` calls
`contract::draft_contract(previous, &recon)` → `CompletionContract::next_revision`
**unconditionally on every single `Preflight`**, not only on a
meaningful replan. This means "the currently active contract revision"
changes on every ordinary prompt in a session, not only when completion
criteria actually change.

This has a real consequence this ADR records as a **hard precondition
for ever wiring `grant_renewal` into production** (today inert, since
`renewal_not_wired_live.rs` prevents that caller from existing at all):
scoping a `TaskAlreadyCompleted` refusal to "the current revision only"
means a task marked `Completed` at revision N becomes renewable again
after exactly one more ordinary prompt bumps the task to revision N+1 —
not after a deliberate, reviewed replan. Implemented as instructed
(current-revision semantics, per Decision 2) because the alternative
(inventing a narrower definition of "revision" unilaterally) is exactly
the kind of heuristic invention this campaign has been told not to do.
A future ticket must resolve what counts as a meaningful new revision
(e.g., only when completion criteria actually change) before
`grant_renewal` is ever given a production caller.

### 6. Legacy rows (`authoritative=1`, no `contract_revision`, written before PR 3)

Not treated as authority for any specific revision (Decision 1 — no
authority from an unverified row), but a legacy `Completed` row still
refuses renewal under a dedicated `UnboundLegacyCompletion` reason
(Decision 2 — ambiguity must not silently loosen eligibility). No
existing row is rewritten or deleted.

### 7. Precondition for any real (non-Rust) Outcome Provider: the signing format must be reproduced byte-for-byte

`crates/extension/src/outcome_authority.rs::canonical_bytes` is not
`serde_json` of a struct — it is four length-prefixed fields in a fixed
order, documented in that function's own doc comment, with a pinned
cross-language test vector (`pinned_cross_language_signing_vector`) as
the authoritative reference. This matters because a general-purpose JSON
serializer in another language (e.g. Python's `json.dumps`) does not
byte-match `serde_json`'s output by default — different field separators,
different non-ASCII escaping — so a provider that signs "the same data"
through its own JSON encoder would produce a different signature and
fail closed on every claim. Any future real provider integration must
implement `canonical_bytes`' exact encoding (and the `evidence_digest`
sub-format it itself depends on) against that pinned vector before its
signatures can ever verify.

## Consequences

### Gained

- Honest, tested, fail-closed behavior: every reachable production path
  produces a non-authoritative attestation. No silent conflict
  resolution. No stale-revision renewal unblocking.
- A verification mechanism that is ready to activate the moment a
  genuinely separate-principal deployment exists, without redesign.

### Accepted costs

- HORO-1727's calibration evidence gap is **not** resolved by this ADR
  — it is, if anything, more clearly blocked: authoritative completion
  cannot exist in production at all until a separate-principal
  deployment is built. This is a new, genuinely founder-level decision
  (build or acquire such a deployment), recorded as a follow-up, not
  decided here.
- `grant_renewal`'s revision-scoping (Decision 5 above) is weaker than
  an intuitive reading of "current revision" would suggest, because
  contract revisions churn on every prompt. Documented as a hard
  precondition rather than silently accepted.

## Amendments to ADR 0005

- **§2** ("Honest limitation, stated plainly"): superseded by this ADR's
  fuller threat model (direct ledger writes, secret-command readability,
  config/binary replacement) — ADR 0005 §2's filesystem-permission
  framing was accurate but incomplete; this ADR is the complete version.
- **§7** ("Trust boundary R3"): the claim "`handle_finalize`'s own
  outcome event always attributes to `AttestationSource::GovernorLocal`"
  is corrected — `handle_finalize` never constructs an
  `AttestationSource` at all (see PR #109's doc fix on
  `crates/daemon/src/server.rs`). `GovernorLocal` remains a documented,
  unused variant, same as `Agent` was before this ADR's PR 2.
