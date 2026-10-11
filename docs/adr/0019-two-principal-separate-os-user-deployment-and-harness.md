# ADR 0019: Two-Principal Deployment (Separate OS User for the Outcome Provider and Ledger of Record) and Its Disposable Harness

- **Status:** Proposed. This is a design only. Every code path it describes lands default-off, and stays off until an independent review accepts the harness evidence.
- **Date:** 2026-10-11
- **Ticket:** HORO-1835 (design). The founder decision is recorded on HORO-1749.
- **Extends:** ADR-0017 and ADR-0018. See "Amendments and cross-references" at the end.

## Context

ADR-0018 left two founder decisions open. Decision 2 asked who owns the ledger of record. The founder has now answered it (recorded on HORO-1749): the authoritative Outcome Provider and the Ledger of Record run as a **separate OS security principal**. A helper process running under the same agent-controlled account is explicitly rejected, because it is not a separate authority.

The threat model must cover eleven attack classes:

1. forgery;
2. suppression or deletion of an authoritative completion;
3. ledger replacement;
4. DB trigger removal;
5. socket impersonation;
6. executable replacement;
7. state-directory overrides;
8. client-side daemon auto-spawn or fallback;
9. credential or signing-oracle misuse;
10. restart conditions across the privilege boundary;
11. race conditions across the privilege boundary.

The founder set three hard constraints on this pass:

- **No real accounts or host changes.** Do not create persistent OS accounts, alter host privileges, move the live ledger, change the authentication environment, or activate a privileged production daemon. All proof happens in disposable, isolated environments.
- **Default-off.** Every new trusted-completion code path is default-off until independent review accepts the evidence.
- **No silent scope loss.** The v0.0.4 launcher, calibrated-budget, and real-pacing work (HORO-1749; ADR-0015, ADR-0016) stays in scope and is not traded away here.

This ADR changes no code. On the default deployment, `DaemonConfig::outcome_authority` stays `None` and `outcome_authority_not_wired_live.rs` is untouched.

## Decision summary

1. **The whole daemon and the whole ledger move to the provider principal, not just the attestation table.** The agent becomes a pure protocol client.
   - `grant_renewal` reads `task_budgets`, `task_budget_renewals`, `contracts` and `outcome_attestations` inside one `BEGIN IMMEDIATE` transaction.
   - If only `outcome_attestations` moved, the agent could still `INSERT` a renewal or raise its own ceiling in a ledger it owns. Protecting the completion row would then buy nothing.
   - Rule: every table that any admission or renewal decision reads is owned by the provider.
2. **Authority comes from a kernel-attested peer UID, not a key.**
   - An authoritative attestation is accepted only over a connection whose kernel-reported peer UID is the provider UID. The kernel reports it via `SO_PEERCRED` on Linux and `getpeereid` on macOS.
   - The request goes through a new, dedicated `Request::AttestOutcome` verb. In production, only the human operator reaches that UID, via `sudo -u libra-provider /usr/local/libexec/libra-governor/libra-governor outcome attest`.
   - The local trusted path has no signing key at all, so there is no signing oracle to abuse.
3. **Agent-UID peers get non-authoritative operations only.**
   - `RecordOutcome` from the agent stays `Unverified`, exactly as it is today.
   - `AttestOutcome` from the agent returns `Error` and writes nothing.
   - No request verb signs anything or makes anything authoritative on the agent's behalf.
4. **The provider never dereferences a filesystem path the agent names.**
   - In trusted mode, `Preflight.cwd` recon is disabled and `Finalize.transcript_path` is ignored.
   - Otherwise the provider would act as a confused deputy reading its own private files at the agent's direction.
5. **Paths, binary and config are pinned by root.**
   - Trusted mode ignores `LIBRA_GOVERNOR_STATE_DIR`, `XDG_STATE_HOME` and `HOME` entirely.
   - It refuses to start unless an sshd-`StrictModes`-style ownership and permission walk passes.
   - It never shuts down because a client asked it to, even through the existing protocol-version-mismatch path.
6. **Default-off is a non-default Cargo feature, `trusted-principal`.**
   - The release binary cannot enter trusted mode.
   - `config.json` cannot enable it.
   - Removing the gate is a separate, reviewed decision.
7. **A disposable Linux-container harness proves each threat fails closed.**
   - Throwaway users exist only inside a `docker run --rm` container.
   - Every attack runs three times: a positive control, a same-UID sensitivity control that must succeed, and the two-UID run that must fail.
8. **Work splits into this ADR plus three PRs** (see §6). PR-1 builds harness infrastructure against today's binary with no product-code change.

## 1. Deployment shape

### Principals

| Principal | Production name | Harness name | Role |
|---|---|---|---|
| Provider | `libra-provider` (system user, no login shell, not in any admin/sudo group) | `lg-provider` | Runs the daemon. Owns the ledger, state and socket directories. |
| Agent | a dedicated non-admin user (see R6) | `lg-agent` | Runs the governed coding agent and its hooks. Protocol client only. |
| Client group | `libra-clients` | `lg-clients` | Membership grants permission to reach the socket, nothing more. The agent is a member; the provider owns the directory. |
| Operator | the human, via `sudo -u libra-provider ...` | container `root`, as orchestrator and human stand-in | Sole judge of completion (Q2). Starts and stops the provider. |

### Filesystem layout

The harness uses this same layout, so no path translation is needed.

| Path | Owner:group | Mode | Why |
|---|---|---|---|
| `/usr/local/libexec/libra-governor/libra-governor` | `root:root` | `0755` | The provider's executable. Every ancestor directory is root-owned and not group- or other-writable. The agent's own `~/.local/bin` copy is irrelevant to the provider. |
| `/etc/libra-governor/trusted-principal.json` | `root:root` | `0644` | Pinned config: `provider_uid`, `provider_id`, `agent_uids`, state and socket paths, timeouts. JSON, matching `config_file.rs`'s no-new-dependency rationale. This is never `config.json` in a state directory. |
| `/var/lib/libra-governor/` | `libra-provider:libra-provider` | `0700` | State dir. Holds `ledger.sqlite3` and its `-wal`/`-shm` files (`0600`), `daemon.log`, and `daemon.pid`. The agent cannot traverse it. |
| `/run/libra-governor/` | `libra-provider:libra-clients` | `2710` | Socket dir. Group members can traverse it but cannot list, create, unlink or rename entries. The setgid bit makes the socket inherit the group. |
| `/run/libra-governor/daemon.sock` | `libra-provider:libra-clients` | `0660` | Linux requires write permission on the socket to `connect`. On macOS the design does not rely on socket-file modes; the directory and peer-cred checks below carry the boundary. |

### Launch and startup checks

The provider is started only by root or a service manager:

- production: a systemd `User=libra-provider` unit, or a launchd LaunchDaemon with `UserName`;
- harness: `setpriv --reuid --regid --init-groups --reset-env --no-new-privs`.

It runs with a scrubbed environment and a fixed argv: `daemon run --trusted-principal-config /etc/libra-governor/trusted-principal.json`. No client ever spawns it.

At startup, trusted mode **refuses to start**, logging the reason and exiting non-zero, unless every one of these checks passes:

1. `getuid() == provider_uid`, and that UID is not 0. Trusted mode does no privilege dropping of its own; the launcher does it.
2. The config file is root-owned and not group- or other-writable.
3. `current_exe()`, the config, the state dir and the socket dir pass an ancestor walk. Every component must be owned by root or the provider and must not be group- or other-writable. The one allowed exception is the socket dir's own group bits, exactly as specified above.
4. The state dir is owned by the provider with mode `0700`.
5. After migrations, `sqlite_master` contains all four append-only triggers:
   - `trg_outcome_attestations_no_update`
   - `trg_outcome_attestations_no_delete`
   - `trg_task_budget_renewals_no_update`
   - `trg_task_budget_renewals_no_delete`

The trigger check runs after migrations and fires on an existing database: `crates/ledger/src/migrations.rs` applies only versions above `MAX(version)` in `schema_migrations`, so a dropped trigger is **not** silently recreated by 0017/0018's `CREATE TRIGGER IF NOT EXISTS`.

Config validation also rejects a `provider_id` that starts with `unverified:`. That prefix is the stored `source_id` namespace for agent pushes, and the `(task_id, source_id, idempotency_key)` dedup key depends on the two namespaces never colliding. If they collided, an agent push could front-run the operator's attestation as a `Duplicate` or `IdempotencyKeyReused`.

Trusted mode never calls `paths::state_dir()`. Every path comes from the pinned config.

**Trusted computing base rule.** The TCB is exactly the set of executables that ever run as the provider UID. It is enumerated:

- the libexec `libra-governor` binary, in `daemon run` and `outcome attest` modes;
- the service manager or `sudo` that launches it;
- nothing else.

In trusted mode the provider executes no subprocess: no `secret_command`, no extensions, no webhooks.

Trusted mode also runs the daemon with a reduced feature set: gateway, extensions, recon and transcript usage are off (see R9). The trusted config has no fields for them, so a misconfiguration cannot turn them on.

### Socket, peer identity and the accept loop

The provider binds `/run/libra-governor/daemon.sock`. Only the provider (and root) can create entries in that directory, so two things that used to be races are now provider-only operations:

- `bind_or_detect_running`'s stale-socket `remove_file`;
- the window between bind and chmod.

Neither is reachable from the agent.

**Peer classification.** For every accepted connection, the daemon reads the peer UID through `libc` (already a workspace dependency):

- `uid == provider_uid` is a **Provider** peer;
- `uid` in `agent_uids` is an **Agent** peer;
- anything else, including root, is closed before a byte is read.

The harness reads the ledger directly as root, not over the socket.

**Per-connection read and write timeouts** (from config, e.g. 5 s) bound how long one idle agent connection can hold the serial accept loop. Today there is no server-side timeout, so a single silent connection blocks the daemon indefinitely.

**No client-triggered shutdown.** Today `serve` breaks its loop when a client sends a newer `protocol_version` (`server.rs` around lines 386–395, `handle_connection`'s `shut_down_after_reply`). Combined with auto-spawn, that would let the agent kill the provider and race a replacement. In trusted mode the daemon answers `Error` and keeps serving.

### Authentication, in both directions

**Agent to provider.**
- The agent presents no secret. Its kernel-reported UID is its identity.
- Everything the agent sends is a non-authoritative input: `Preflight`, `ToolInvoked`, `Finalize`, `Status`, and `RecordOutcome`, which is stored as `Unverified`.
- The agent cannot select a provider identity. `AttestOutcome` stores `provider_id` from the pinned config, never from the request.

**Provider authenticating "the real completion signal".**
- The provider does not verify the work itself. It authenticates the attestor: an `AttestOutcome` arriving from a Provider-class peer.
- That peer is the operator, running `sudo -u libra-provider /usr/local/libexec/libra-governor/libra-governor outcome attest` with stdin JSON in the same shape as `outcome record`. It is always an absolute path and never `PATH`-resolved: anything that runs as the provider UID can write the ledger.
- The attest CLI checks `getuid() == provider_uid` and runs the same `current_exe()` ancestor walk as the daemon. It then verifies that the server's peer UID is also `provider_uid` before writing a byte.
- It exits 0 **only** if the response is `Recorded` with `authoritative: true`. Any other response, error or timeout exits non-zero, so a failed attestation is never silent.

**Honest clients** are the attest CLI and operator queries. In trusted mode they:
- take the socket path and expected provider UID from the pinned config;
- ignore environment overrides;
- never auto-spawn;
- verify the server's peer UID before sending anything.

The agent's own hooks use the same client code, but only as hygiene. **The agent can always run a different client or its own daemon**, and doing so only changes the agent's own view. It cannot reach or alter the provider's record.

### Restart and socket recreation

Restarts come only from root or the service manager. During the window between the old process exiting and the new one binding:

- agent `connect` attempts get `ENOENT` or `ECONNREFUSED`;
- the agent cannot create, rename or unlink anything in either directory;
- `kill(2)` against the provider fails with `EPERM`;
- the pidfile cannot be read.

The new process re-runs the full set of startup checks above before binding. SQLite WAL recovery runs as the provider. A committed authoritative row survives `kill -9` (tested as T10).

### Default-off mechanism and the static guard

- `crates/cli` and `crates/daemon` gain a `trusted-principal` Cargo feature. It is not listed in `default`.
  - The `--trusted-principal-config` flag, the peer-cred classifier, `AttestOutcome` dispatch and the attest CLI exist only under `#[cfg(feature = "trusted-principal")]`.
  - `scripts/install.sh` builds with `cargo install --path crates/cli --locked` and never passes the feature. Neither does any release workflow.
  - **Workspace feature unification.** Under `resolver = "2"`, `cargo build --workspace` unifies features across members. So **no workspace manifest may enable `trusted-principal` on a dependency**, outside the forwarding line inside `crates/cli`'s own `[features]` table.
    - The harness crate has no Cargo dependency on `libra-governor-cli` or `-daemon`. It drives the binary only through `std::process::Command` and stdin JSON.
    - The feature-enabled binary is built only inside the container, with `cargo build -p libra-governor-cli --features trusted-principal`.
- The `AttestOutcome` construction of `AttestationSource::Provider` lives in `crates/daemon/src/outcome_authority_wiring.rs`, the guard's single allowlisted file, under that same `cfg`.
- `outcome_authority` (the HMAC path) stays `None` in trusted mode too. Trusted mode does not use it.
- New guard tests, each added as a deliberate, reviewed change:
  - `trusted-principal` is absent from every crate's `default` features, and no manifest names it except `crates/cli`'s and `crates/daemon`'s own `[features]` tables;
  - neither the install script nor any release workflow references it;
  - every `AttestationSource::Provider` construction in `daemon/src` remains inside the allowlisted file.
- **Why a Cargo feature, despite ADR-0003 §1's preference for runtime gating.**
  - ADR-0003's concern was security code that is never compiled or tested in CI. Here, the harness CI job builds and tests with the feature on, and `cargo clippy --all-features` keeps it linted.
  - What we gain is a property runtime gating cannot give: the shipped artifact provably cannot enter trusted mode before review accepts the evidence.
  - CI does not run `--all-features` today. The harness workflow adds `cargo clippy -p libra-governor-cli -p libra-governor-daemon --features trusted-principal --all-targets -- -D warnings` as an explicit step.

## 2. Q1, Q2 and Q3 applied to this shape

| Question | Answer for this shape | What is traded away |
|---|---|---|
| **Q1: who holds the signing authority?** | No transferable key exists on the local trusted path. Authority is a kernel-attested peer UID equal to `provider_uid`, which only root or the operator's `sudo -u` can assume. The HMAC `verify_claim` path stays unconfigured. This is consistent with ADR-0018's rule: signer and "verifier" are the same principal, and in fact no key is needed. | No portable proof: an off-machine auditor cannot re-verify a row. That remains the job of the remote-log option, which still requires asymmetric signing per ADR-0018. |
| **Q2: who judges the claim is true?** | The human operator, by choosing to run `outcome attest`. No automated judge is used: running agent-written tests as the provider was rejected in ADR-0018, and CI is still blocked by ADR-0018's founder prerequisite 1. | Throughput and automation. The task-to-work mapping and the evidence references the operator reviews are agent-produced (R5). **No harness can prove Q2.** It proves only that nobody else can use the attestation channel. |
| **Q3: who owns the ledger of record, verifier and consumers?** | The provider owns the whole `ledger.sqlite3` (and sidecars), the daemon process, the trigger set, and every consumer, including `grant_renewal`, which stays unwired. | In trusted mode, agent-side conveniences that read the ledger or filesystem directly stop working (R9). |

## 3. Threat-by-threat

"Harness" names the test in §5. Each row's defense is independent of the agent's cooperation.

| # | Threat | Defense | Harness proof | Residual |
|---|---|---|---|---|
| T1 | Forgery | Authority requires a Provider-class peer. Agent-UID `AttestOutcome` returns `Error`. Agent `RecordOutcome`, even with a `signed_claim`, is stored `Unverified`. The ledger file cannot be opened by the agent. | Agent sends `AttestOutcome`, sends `RecordOutcome` with a self-made `signed_claim`, and runs `sqlite3 ledger.sqlite3 "INSERT ... authoritative=1"`. All fail; the snapshot shows zero agent-originated `authoritative=1` rows. | None at this layer. |
| T2 | Suppression or deletion | The agent cannot open, unlink or replace the ledger (dir `0700`). Triggers block `UPDATE`/`DELETE` even for the provider. No protocol verb deletes attestations. | Operator attests `completed`. The agent then tries `DELETE`, a trigger drop plus `DELETE`, `rm`, `truncate`, and an idle-connection flood during a second attest. The row survives. The attest CLI either exits 0 with the row present or exits non-zero; **never exit 0 with the row absent**. | **Omission is not defeated (R1).** If the operator never attests, the row is legitimately absent. Availability-delay DoS is bounded and loud (R4). |
| T3 | Ledger replacement | State dir is provider `0700` and its ancestors are root-owned, so rename, symlink or hardlink swaps are impossible for the agent. The state dir is created by root before first start, so there is no pre-creation window. | Agent tries `mv`/`ln -s`/`ln` on the ledger and its directory, and tries planting a ledger at the path before the provider's first start. All return `EACCES`/`EPERM`. Ledger inode and content hash are unchanged. | None at this layer. |
| T4 | DB trigger removal | The agent cannot open the DB. The startup check refuses to start if any of the four triggers is missing, which fails closed even against provider-side damage. | Agent runs `sqlite3 ... "DROP TRIGGER ..."` and gets `EACCES`. Root drops a trigger offline and restarts the provider; the provider refuses to start. | None at this layer. |
| T5 | Socket impersonation | The socket dir is not writable by the agent, so it cannot pre-create, rebind or unlink. Honest clients verify the server's peer UID before sending. | Agent tries to create or rename `daemon.sock` (`EACCES`). With the attest CLI's env polluted toward an agent-owned socket, the CLI refuses on peer-UID mismatch and the agent's fake server logs **0 bytes received**. | The agent's own hooks can be pointed at its own fake daemon; that only fools the agent (R3). |
| T6 | Executable replacement | The provider binary sits on a root-owned path with root-owned ancestors. The startup walk checks `current_exe()`. Only root launches the provider; the agent's `~/.local/bin` is never consulted. | Agent tries to overwrite or rename the provider binary (`EACCES`) and replaces its own client binary. The provider's `/proc/<pid>/exe` hash is unchanged and it still serves. **PATH shadowing:** the agent plants a `libra-governor` early on a `PATH` it controls. The operator's attest invocation (absolute path) never executes it; a sentinel file the shadow binary would write is never created. | Shared libraries are root-owned system paths (the `rusqlite` dependency builds SQLite in, via its `bundled` feature). |
| T7 | State-directory overrides | Trusted mode never reads `LIBRA_GOVERNOR_STATE_DIR`, `XDG_STATE_HOME` or `HOME`. The launcher scrubs the environment. Agent-side overrides only redirect the agent to its own daemon. | (a) Provider started with an agent-controlled `LIBRA_GOVERNOR_STATE_DIR` in its env still uses the pinned dir. (b) Agent sets the override, runs its own daemon and "attests" there; the provider snapshot is unchanged. | None for the record. See R3 for governance coverage. |
| T8 | Auto-spawn or fallback | Trusted-mode clients never spawn. Even if the agent forces `daemon run --trusted-principal-config ...`, it refuses (UID mismatch) and bind fails (`EACCES`). | Provider stopped; agent hook runs. No `lg-agent`-owned daemon process appears, and the hook reports unavailable. The forced trusted-mode start exits non-zero. | A default-mode same-user daemon the agent starts is the T7(b) case: harmless to the record. |
| T9 | Credential or signing-oracle misuse | No local signing key and no sign verb exist. `/proc/<pid>/environ` and `mem`, and cross-UID `ptrace`, are denied by the kernel. The agent has no sudo, and the provider never executes agent-writable commands (no `secret_command` in trusted mode). | Agent tries `/proc/<pid>/environ`, `/proc/<pid>/mem`, `ptrace` (via `gdb -p` or a small helper), `sudo -n true`, and `sudo -n -u lg-provider true`. All fail. A static check finds no signing call reachable from a request handler. | A cached operator sudo ticket, or shell-rc capture of the operator's password, on a real machine (R6). The container has neither. |
| T10 | Restart across the boundary | Restarts come only from root or the service manager. Startup checks re-run on each start. WAL durability holds. Client-triggered shutdown is removed. | (a) Operator attests, root sends `kill -9`, root restarts; the row is present. (b) Agent sends a newer-protocol envelope; the provider is still alive. (c) During the down-window the agent loops create, bind and connect attempts in both dirs; all fail. | None at this layer. |
| T11 | Races across the boundary | Only provider or root can create directory entries, which closes the stale-socket unlink and bind-to-chmod races. `BEGIN IMMEDIATE` serializes ledger writes. Per-connection timeouts bound accept-loop starvation. | (a) Agent spins connect and create attempts against the socket dir while root restarts the provider 50 times; the pinned path is never served by a non-provider peer. (b) Concurrent agent `RecordOutcome` and operator `AttestOutcome` on the same task and idempotency key both land, with only the provider's row authoritative. | **Revision churn (R2)**: the agent can make an attested completion stop applying to the *current* revision by sending an ordinary `Preflight`. |
| extra | Confused deputy | The provider never opens an agent-named path (recon off, `transcript_path` ignored). | Agent sends `Preflight{cwd: "/var/lib/libra-governor"}` and `Finalize{transcript_path: <ledger path>}`. The responses show recon or usage unavailable. A FIFO sentinel the agent places at the named path never gets an opener. | Functional cost (R9). |

## 4. Residual risks, stated plainly

- **R1. Omission.** If the operator never attests, there is no authoritative row, and anything that grants on absence (a future `grant_renewal` caller) would grant. Ledger ownership defeats deletion, not non-creation. This is bounded only by Q2, and is one more reason `grant_renewal` stays unwired.
- **R2. Revision churn** (ADR-0017 Decision 5, still open). `next_revision` runs on every `Preflight`, so completion scoped to "the current revision" stops binding after one more prompt. The harness ships a deliberately named characterization test, `a_preflight_after_attestation_unscopes_completion_today`. It asserts the gap exists, so closing it later is a visible, reviewed act. `renewal_not_wired_live.rs` remains the guard.
- **R3. Hooks are cooperative.** The agent can skip, rewrite or redirect its own hooks. This design protects the provider's record, not the agent's willingness to report to it. Only the enforcement gateway (ADR-0003), with the upstream credential held by the provider, would enforce spending. That is out of scope here.
- **R4. Availability.** The serial accept loop lets the agent *delay* an attestation by keeping connections queued. Timeouts bound each one, and the attest CLI's exit contract makes the failure loud, never silent. If the harness shows delays the operator cannot tolerate, a separate provider-only admin listener is the follow-up. It is not built now.
- **R5. Q2 is not provable.** The operator decides which `task_id` and `plan_id` the work corresponds to, from agent-produced identifiers and evidence references.
- **R6. This dev machine cannot host the design as it stands.** The governed agent runs as `bryant`, a local admin who can capture the operator's next sudo password through shell rc files (ADR-0018 facts). The separate-user boundary holds only when the agent runs as a dedicated, non-admin, non-sudoer user distinct from the operator.
  - That user-account change is a founder or infrastructure decision. The constraints on this pass forbid making it, and the harness does not exercise it.
  - A deployment-time checklist (agent UID not in admin, wheel or sudo groups; no cached tickets; operator's rc files not agent-writable) is required, not a one-time test.
- **R7. Root and the kernel are trusted.** Container escape, kernel exploits and root compromise are out of scope.
- **R8. macOS is unproven.** The Linux harness does not prove macOS semantics (`getpeereid`, socket-mode enforcement, no `/proc`). Phase 2 runs the same catalog on a disposable `macos-latest` GitHub runner with `sysadminctl`-created users, **never on the dev Mac**.
- **R9. Trusted mode degrades functionality.** Recon, transcript-measured usage, gateway and extensions are off in trusted mode. Calibration and measured-usage work continues on the default deployment, which is unchanged.
  - Client-side recon and usage parsing would restore these. They have no trust cost, because both inputs are agent-asserted anyway.
  - **Client-side usage parsing is a prerequisite for calibration, not an optional nicety.** Calibration evidence needs both authoritative completion, which only trusted mode can produce, and transcript-measured actuals, which only the default mode produces today. Until usage reaches the provider over the protocol, no single deployment can produce both.
- **R10. CI is not independent evidence.** ADR-0018 established that the agent's credential is an org owner and `main` is unprotected. A green harness job is therefore corroboration only. The independent reviewer must reproduce it locally with `harness/two-principal/run.sh`.

## 5. The disposable two-principal harness

### Isolation rules (non-negotiable)

- The harness runs **only** inside a disposable Linux container: `docker run --rm --network none`.
  - No `--privileged`, no added capabilities, no Docker socket mount.
  - Host mounts are read-only, or there are none (binaries are built inside the image).
- The same image runs in CI (`ubuntu-latest`, `docker run`) and locally (Docker Desktop's VM).
- Users, groups and directories exist only in the container's own `/etc/passwd` and filesystem, and vanish with it. No host user, group, sudoers entry, launchd or systemd unit is ever touched.
- `provision.sh` (the only script that runs `groupadd`/`useradd`) refuses to run unless **all three** hold: `id -u` is 0, `/.dockerenv` exists, and `LG_TWO_PRINCIPAL_DISPOSABLE=1` is set (baked into the image's `ENV`).
- All harness Rust tests are `#[ignore]`, so `cargo test --workspace` on a host never runs them. If run with `--ignored` without the same marker, they panic. They never silently skip.

### Layout

```
harness/two-principal/
  Dockerfile        # FROM rust:<pinned digest>-bookworm; apt: sqlite3, util-linux (setpriv), procps, gdb
                    # build stage: cargo build --release -p libra-governor-cli --features trusted-principal
                    #              cargo test -p libra-governor-two-principal-harness --no-run
                    # runtime: installs binary at /usr/local/libexec/libra-governor/ root:root 0755
  provision.sh      # disposable-marker check, then: groupadd lg-clients; useradd -r -s /usr/sbin/nologin lg-provider;
                    # useradd -m -G lg-clients lg-agent (no sudo); dirs + modes per §1;
                    # writes /etc/libra-governor/trusted-principal.json; also a same-UID control layout
                    # /srv/lg-sameuid owned by lg-agent (sensitivity controls)
  run.sh            # host entry point: docker build && docker run --rm ...; exit code = verdict;
                    # prints JUnit/plain log + ledger dumps + exe hashes to stdout
crates/two-principal-harness/   # publish = false, test-only; NO Cargo dep on cli/daemon (drives the binary via Command only)
  src/lib.rs        # fixture: Provider::start/stop/kill9/restart (as lg-provider via setpriv, env -i),
                    # as_agent(cmd) / as_provider(cmd) (setpriv --reuid --regid --init-groups --reset-env --no-new-privs),
                    # LedgerSnapshot (root, `sqlite3 -readonly`): outcome_attestations, task_budget_renewals,
                    # sqlite_master triggers, file inode + sha256; disposable-marker assertion
  tests/t01_forgery.rs ... tests/t11_races.rs, tests/x_confused_deputy.rs,
  tests/x_proc_ptrace_sudo.rs, tests/r2_revision_churn_characterization.rs
```

### Roles at runtime

- **root:** the orchestrator, and the stand-in for the human operator's `sudo -u`. Root is never an attacker.
- **lg-provider:** the daemon, plus the attest CLI when root invokes it as this user.
- **lg-agent:** the attacker. It has a real `HOME` and the real client binary, and every attack step runs as this user with no capabilities and `no_new_privs`.

### Per-test pattern

Tests run serially (`--test-threads=1`), and root re-provisions a fresh state dir for each test. Every threat test runs three passes:

1. **Positive control.** The legitimate flow works end to end: an agent `Preflight` creates a task, the operator attests `completed`, and the snapshot shows the authoritative row. A pass therefore cannot mean "everything is unreachable".
2. **Sensitivity control.** The *same* attack, run against the `/srv/lg-sameuid` layout (agent owns the ledger, as on today's deployment), **must succeed**. This proves the test really performs the attack in this environment.
   - The `ptrace` and `/proc` checks are the key case: the agent attaching to its *own* process proves the container's seccomp and Yama settings are not what blocks the two-UID attempt.
3. **Two-principal run.** The attack fails with the expected errno or `Error` response, **and** the before and after ledger snapshots match (apart from rows the test itself expected), **and** the provider is still alive with an unchanged exe hash.

### Orchestration and teardown

- **Locally:** `harness/two-principal/run.sh` runs `docker build` then `docker run --rm`. Teardown is container removal; nothing persists on the host.
- **CI:** a new workflow, `.github/workflows/two-principal-harness.yml`, triggered by `workflow_dispatch` plus a path filter on `crates/daemon/**`, `crates/cli/**`, `crates/ledger/**` and `harness/**`. It just calls `run.sh`.
  - This changes CI definitions, so it needs explicit review before merge (global policy).

## 6. Scope call: split it

This is too large for one PR and mixes evidence-gathering with gated product code. The recommended sequence (each PR under about 500 lines where possible):

1. **This ADR** (HORO-1835).
2. **PR-1: harness infrastructure plus OS-permission threats, no product-code change.**
   - Today's binary runs as `lg-provider` with a root-pinned `LIBRA_GOVERNOR_STATE_DIR` and a `0700` state dir. The agent cannot even reach the socket.
   - PR-1's positive control is therefore a task and outcome created by a provider-UID client, not an agent `Preflight`.
   - That is enough to prove T3, T4 (the agent half), T6, the `/proc`/`ptrace`/`sudo` parts of T9, and `kill`/pidfile denial, each with positive and sensitivity controls.
   - Includes `Dockerfile`, `provision.sh`, `run.sh`, the fixture crate and the CI workflow.
3. **PR-2: trusted-principal mode, no authority yet** (feature-gated).
   - Server: pinned config and the startup checks (including the trigger check), env-ignore, socket dir and peer-cred classification, per-connection timeouts, no client-triggered shutdown, no agent-named paths.
   - Client: pinned paths, no spawn, server peer-UID verification.
   - Harness: T5, T7, T8, T10(b)(c), T11(a), confused deputy.
   - If it exceeds 500 lines, split into server half and client half.
4. **PR-3: `AttestOutcome` and the attest CLI** (feature-gated).
   - Authority construction in the allowlisted wiring file, plus the guard additions from §1.
   - Harness: T1, T2, T4 (startup refusal), T9 (no-oracle static check), T10(a), T11(b), the authoritative positive control, and the R2 characterization test.
5. **Independent review of the harness evidence**, reproduced locally by the reviewer. Removing the feature gate is a **separate decision** after that review, and also requires R6's deployment decision before any real use.

None of this touches the v0.0.4 launcher, calibrated-budget, or pacing work (HORO-1749, ADR-0015, ADR-0016). Those continue on the unchanged default deployment.

This work does **not** unblock HORO-1727's calibration evidence. That needs all three of:

1. the review step above;
2. R6;
3. client-side usage parsing (R9), so that trusted mode can also produce measured actuals.

The third is a real v0.0.4 calibrated-budget dependency and should be ticketed as such, not dropped.

## Amendments and cross-references

- **ADR-0018, "Founder decisions" item 2:** resolved as option (a), separate OS user (HORO-1749). Item 1 (non-owner CI credential plus branch protection) is unchanged and still open.
- **ADR-0018, "Escape hatches":** each is now mapped to a closing mechanism.

  | ADR-0018 escape hatch | Closed by |
  |---|---|
  | `~/.local/bin` install path | root-owned libexec path plus the startup walk |
  | `LIBRA_GOVERNOR_STATE_DIR` / `XDG_STATE_HOME` | pinned config, env ignored |
  | `ensure_daemon_connection` → `spawn_daemon_detached` | no-spawn trusted client, UID-mismatch refusal |
  | writable socket directory | provider-owned `2710` dir plus peer-cred |
  | one-daemon-per-user assumption | one provider daemon per host per configured agent set (multi-tenant isolation is out of scope) |

- **ADR-0018, "two-UID red-team harness":** superseded in detail by §5.
  - New: sensitivity controls, the protocol-mismatch shutdown vector, the confused-deputy paths, and the never-exit-0-with-row-absent attest contract.
  - Still holds: "build it alongside the principal". PR-1 lands first, but only for attacks that need no design-dependent code.
- **ADR-0018, Amendment on ADR-0017 Decision 2** (asymmetric signing required across principals): clarified. The local trusted path's authority check does not cross principals and uses no key. Asymmetric signing remains required for the remote-log and CI options.
- **ADR-0017, Decision 1:** unchanged for the default deployment, where authoritative completion stays structurally disabled. This ADR adds a second, non-default, feature-gated deployment mode in which it can be enabled only after review.
- **ADR-0017, Decision 5 and ADR-0018's suppression finding:** now explicitly bounded by R1 and R2. Ledger ownership closes deletion, replacement and trigger removal, but not omission or revision churn.

## Consequences

### Gained

- A concrete deployment shape whose claimed boundary rests on kernel-enforced facts: directory modes, peer credentials, and cross-UID `/proc`, `ptrace` and `kill` denial. It reuses the existing daemon and protocol, with no new cryptography.
- An executable, falsifiable acceptance gate for each of the eleven founder threats, built so that a pass cannot come from a broken environment, thanks to the positive and sensitivity controls.
- Early evidence (PR-1) with zero product-code risk.

### Accepted costs

- Trusted mode gives up recon, transcript-measured usage, gateway and extensions until client-side follow-ups exist (R9).
- Omission, revision churn, cooperative hooks and Q2 remain open, and are named here rather than implied closed (R1, R2, R3, R5).
- Real deployment needs an account-model change on the actual machine (R6). That is not decided or performed here.
