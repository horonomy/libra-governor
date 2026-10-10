# ADR 0018: Separate-Principal Deployment Options, and the Suppression Gap in ADR-0017

- **Status:** Accepted
- **Date:** 2026-10-10
- **Ticket:** HORO-1727 / HORO-1749

## Context

The founder authorized investigating a genuinely separate-principal
Outcome Provider deployment — closing the gap ADR-0017 identified rather
than leaving verification logic permanently dormant. This ADR records
that investigation's findings. **It changes no code.**
`DaemonConfig::outcome_authority` stays `None` in every real deployment;
the static guard (`crates/daemon/tests/outcome_authority_not_wired_live.rs`)
is untouched. This document amends ADR-0017 and records two founder
decisions as still open, independent prerequisites.

## The deciding question: who owns the ledger of record, not just the key

Evaluating a candidate principal requires answering three separate
questions, not one:

- **Q1:** Who holds the signing authority (the key)?
- **Q2:** Who decides a completion claim is actually true (judges the
  evidence)?
- **Q3:** Who owns the ledger of record, the verifier, and the pinned
  trust anchor every consumer reads?

Today every consumer of "authoritative" trusts a bare SQL column.
`crates/ledger/src/renewal.rs` (`grant_renewal`'s revision-scoped
completion check, around the `current_contract_revision_tx`/
`authoritative_terminal_kinds_tx` calls) and `crates/ledger/src/extension.rs`
(`record_outcome_attestation`'s conflict-resolution query) both read
`authoritative = 1` directly from `outcome_attestations`. The signed
claim itself (`SignedOutcomeClaimWire`) is never persisted — only its
*verdict* is, as the `authoritative` boolean and `source`/`source_id`
strings — and migration 0009's schema has no column for a claim or its
signature. So even a perfect answer to Q1 (the key lives somewhere
genuinely separate) does nothing if Q3 — the ledger those verdicts land
in — is still owned by the same machine, same OS user, as the governed
agent.

## Finding: ADR-0017 under-weighted suppression

ADR-0017 focused on forgery: can the governed agent construct a valid
authoritative claim it shouldn't be able to construct. `grant_renewal`
only ever *refuses* a renewal based on an authoritative `Completed` row
— forging one can only make the agent's own renewal path stricter, never
looser. So forging authority doesn't pay against the one place authority
is economically load-bearing today.

The profitable attack is the mirror image: **suppression**. Drop the
migration-0018 append-only trigger, `DELETE FROM outcome_attestations`
the one genuine `completed` row, or replace `ledger.sqlite3` outright
with a copy missing that row. A signature over a claim that was never
asked for can't prove the *absence* of a row that should exist. No
signing scheme — HMAC, asymmetric, or otherwise — closes this on its
own; it requires the ledger of record itself to live with the separate
principal (Q3), not just the key (Q1).

## Options evaluated against this real deployment

Checked read-only, against this actual machine and repo, not in the
abstract:

| Option | Q1 | Q2 | Q3 / suppression | Verdict |
|---|---|---|---|---|
| Local signing helper or socket (launchd/systemd/container) | moves | **fails** — it signs whatever the agent asks it to, or "judges" by running tests the agent itself wrote | fails | Security theater: an oracle the agent controls the input to. Running agent-written tests as a more-privileged process also opens a privilege-escalation path, not closes one. |
| Separate OS user owning both the daemon AND the ledger | yes | only if completion is asserted through a trusted human path (e.g. `sudo libra-governor attest ...`); every automated input is still asserted by the agent's own hooks | yes, if the ledger file itself is unreadable/unwritable by the agent's OS user | Real, but a multi-PR re-architecture — see escape hatches below |
| Remote service holding the key and an append-only log | yes | only if it independently judges evidence the agent cannot touch; otherwise it's still an oracle | yes, if it owns the log, not just the key | Real, but needs real hosting/ops cost and a secrets-provisioning decision |
| CI via GitHub OIDC / artifact attestations (Sigstore transparency log) | yes in principle | narrow: proves "workflow W at protected ref, commit S, ran and passed" — never "task T is done"; the task-to-commit mapping is still asserted by the agent | the LOCAL consumer (this daemon/ledger) can still be forged by the same local suppression attack; this only helps an off-machine auditor who independently re-verifies | **Same principal as the agent, today** — see facts below |

### Facts checked this session (read-only, exit codes/metadata only, no destructive action)

- `gh api repos/horonomy/libra-governor/branches/main/protection` → 404,
  `main` is not protected.
- `gh api repos/horonomy/libra-governor/rulesets` → `[]`, no rulesets.
- `gh api orgs/horonomy/memberships/Chisanan232 --jq .role` → `admin` —
  the GitHub credential this agent session inherits belongs to an org
  owner. An agent with this credential can push a workflow change
  straight to `main` or edit any ruleset, so CI is not a separate
  principal from the agent on this repository today.
- `dseditgroup -o checkmember -m bryant admin` → yes (the operating
  user is a local admin).
- `sudo -n true` → exit 1 (no cached/NOPASSWD sudo in this agent's
  shell context) — necessary but not sufficient for the separate-OS-user
  option, since the agent can still edit `~/.zshrc`/shell rc files to
  capture the human's next real sudo password.

### Escape hatches a separate-OS-user option would have to close

All read-only findings, no code changed:

- The binary install path (`crates/cli/src/install_cmd.rs` references
  `~/.local/bin`) is writable by the governed agent's own OS user.
- `LIBRA_GOVERNOR_STATE_DIR` / `XDG_STATE_HOME` environment overrides
  let a caller redirect which state directory (and thus which socket,
  which ledger) it talks to.
- `client::ensure_daemon_connection` → `spawn_daemon_detached` quietly
  starts a parallel same-user daemon whenever the configured socket
  can't be reached — an unauthenticated fallback path that would need to
  be disabled or hardened for this option to hold.
- A writable socket-path directory allows socket hijack.
- Today's daemon architecture assumes one daemon per user, not
  per-principal isolation inside a shared daemon.

## Founder decisions — two independent prerequisites, neither resolved here

1. **For any CI-based principal to be real**, the agent must operate
   under a GitHub credential that is *not* an org owner (a separate
   service account, or a fine-grained token with no administration
   scope), and `main` needs a ruleset/branch-protection rule with no
   bypass for that credential. Until both hold, no CI claim is
   separate from the agent — this is an access-control/infrastructure
   decision, not an engineering one this ticket can make.
2. **Who owns the ledger of record**: (a) a separate-OS-user daemon with
   a sudo-authenticated human attest path — a genuine, multi-PR
   re-architecture of install paths, state-dir resolution, the
   client's auto-spawn fallback, and socket/ledger file permissions; or
   (b) a remote append-only log with its own signer. This is a product/
   infrastructure decision about which operational cost is acceptable,
   not something to default into by building partial plumbing now.

Neither decision is made by this ADR. No PR should implement either
option until one is chosen.

## Item 4 ("prove an agent self-claim cannot become authoritative"), precisely stated

`outcome_authority_not_wired_live.rs` plus `outcome_authority_canary.rs`
prove a narrower, already-true claim: **the daemon's own protocol path**
never grants authority while `DaemonConfig::outcome_authority` is `None`.
The founder's broader statement — "an agent self-claim cannot become
authoritative" — is **false on this deployment, and cannot be made true
on it**: a raw `INSERT` via a second SQLite connection, dropping the
migration-0018 trigger then `DELETE`-ing a row, or a direct `UPDATE
receipts SET outcome_json = ...` all succeed today, because the governed
agent's OS user owns the same files the daemon does. This is exactly why
ADR-0017 keeps authoritative production completion structurally
disabled — it is not a gap to be quietly closed by more careful Rust
code, it is the actual, correctly-identified limit of a same-OS-user
deployment.

### The activation gate: a two-UID red-team harness

Proving item 4 for real requires a test that runs with the SAME OS
permissions the governed agent would have, attempting concrete bypasses,
and asserting all of them fail. This cannot be modeled in-process (a
single-process Rust test cannot simulate cross-UID file permission
enforcement) — it needs two real, disposable, unprivileged OS users on a
throwaway machine:

- **Setup:** a disposable GitHub-hosted Linux or macOS runner (both have
  passwordless sudo for setup). Create throwaway users `lg-provider`
  and `lg-agent`. Run the provider process as `lg-provider`.
- **Attacker:** `sudo -u lg-agent env -i ...` — no sudo, no shared
  group, no inherited environment.
- **Attacks, every one of which must fail:**
  - read the signing key file, the daemon config, or the ledger file;
  - write to the ledger directly via `sqlite3`;
  - drop the append-only trigger, then delete or update a row;
  - kill the provider's daemon and bind the same socket path;
  - redirect via `LIBRA_GOVERNOR_STATE_DIR`/`XDG_STATE_HOME`;
  - trigger the client's auto-spawn fallback;
  - replace the daemon binary or its config file;
  - read `/proc/<pid>/environ` or `/proc/<pid>/mem` for the provider
    process;
  - `ptrace` the provider process;
  - get the provider to sign an arbitrary agent-supplied claim (the
    oracle attack);
  - test whether a shared/cached sudo ticket leaks across the two
    users (don't assume isolation — verify it).
- **Positive control:** a legitimate provider-signed claim still
  verifies end to end, so a passing run isn't just "everything is
  broken/unreachable."

This harness is the activation gate for whichever principal is chosen —
build it alongside that principal's implementation, not before (its
attack list depends on which escape hatches that principal's design
actually closes). It proves Unix permission semantics hold on the test
runner; it does **not** audit a specific developer's own machine state
(shell rc files, cached sudo tickets, existing file permissions) — that
remains a deployment-time checklist, not a one-time test.

**Optional, not blocking:** an in-process characterization test in
`crates/ledger/tests/` exercising raw `INSERT`, drop-trigger-then-delete,
and `UPDATE receipts` against today's same-process ledger, named so it
asserts the boundary does **not** hold (e.g.
`a_same_user_raw_sqlite_write_bypasses_the_daemon_entirely_today`). This
documents the current, accepted limitation as a visible, deliberately
red test rather than an unstated assumption — so a future attempt to
flip it is a reviewed act, not a silent regression.

## Decision

1. No separate-principal option is implemented yet. The two founder
   prerequisites above (non-owner CI credential + branch protection;
   choice between separate-OS-user vs. remote-log ownership of the
   ledger) must be decided first.
2. `DaemonConfig::outcome_authority` stays `None` in every real
   deployment; the existing static guard is unchanged.
3. The two-UID red-team harness is the activation gate for whichever
   principal is eventually chosen, built alongside that principal's
   implementation.
4. Suppression (not just forgery) is now an explicit part of the threat
   model: whichever principal is chosen must own the ledger of record,
   not only the signing key.

## Amendments to ADR 0017

- **Decision 1's description of the code is stale**, corrected here:
  an unverified push constructs `AttestationSource::Unverified {
  claimed_source_id }`, not `Agent`; the production gate is
  `DaemonConfig::outcome_authority` (hardcoded `None` at
  `crates/cli/src/daemon_cmd.rs::run`), not a field on
  `ExtensionConfig`.
- **Decision 2's "ADR-0016 leaves room to revisit" trigger has now
  fired for the cross-principal case.** HMAC is acceptable only when
  the verifier and the signer are the same principal (today's
  structurally-disabled case). Any verification that crosses principals
  (a separate OS user, a remote service, CI) must use asymmetric
  signing (e.g. Ed25519) or an equivalent transparency-log scheme (e.g.
  GitHub's OIDC/Sigstore for CI); which scheme is deferred to whichever
  principal decision is made.
- **"Asymmetric signing doesn't close direct ledger writes" needs
  qualifying.** True for consumers on the agent's own machine reading
  the local ledger. False for an off-machine auditor that independently
  re-verifies persisted, signed claims against a transparency log —
  but even that auditor cannot detect suppression (a row that should
  exist but was deleted before it was ever pushed).
- **Suppression is added to the threat model explicitly** (see
  "Finding" section above): the separate principal must own the ledger
  of record, not merely hold the signing key.

## Consequences

### Gained

- A precise, evidence-based rejection of every "cheap" separate-principal
  option, so no PR this session builds crypto/signer/harness code that
  would silently decide the founder's architecture choice for them.
- A concrete, falsifiable activation gate (the two-UID harness) that any
  future principal implementation must pass before authoritative
  production completion can be re-enabled.
- Suppression now explicit in the threat model, closing a real gap in
  ADR-0017's original analysis.

### Accepted costs

- Numeric ceiling calibration (HORO-1727 AC1-6) remains blocked exactly
  as ADR-0017 already concluded — this ADR does not unblock it, it
  explains precisely what would.
- Two decisions now sit explicitly with the founder/infrastructure
  owner rather than with engineering: a non-owner CI credential +
  branch protection, and separate-OS-user vs. remote-log ownership of
  the ledger.
