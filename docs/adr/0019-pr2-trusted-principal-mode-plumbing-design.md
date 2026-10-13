# ADR 0019 — PR-2 Design: Trusted-Principal Mode Plumbing (No Authority Yet)

- **Status:** Proposed design for implementation. No code in this document is merged.
- **Date:** 2026-10-11
- **Ticket:** HORO-1835 (PR-2 of ADR-0019 §6)
- **Depends on:** ADR-0019 (merged to `main`), PR-1 harness infrastructure (`v0.0.4/HORO-1835/harness_infra_pr1`).
  The PR-2 branch was cut from the PR-1 branch, which predates ADR-0019's merge. Before opening PR-2, merge `origin/main`
  into it (merge commit, never a rebase or force-push) so ADR-0019 and this file sit side by side.

This document turns ADR-0019 §6 "PR-2" into implementation-ready decisions. Where it deviates from or tightens the ADR,
the deviation is called out with its reason. Everything here is behind the non-default `trusted-principal` Cargo
feature. Default-mode behavior must stay byte-identical.

## 0. Decisions at a glance

| Topic | Decision |
|---|---|
| Config | `TrustedPrincipalConfig`, JSON, `deny_unknown_fields`, 6 fields. Loaded only from an explicit path (daemon) or the compile-time pinned path (client). Never from env. |
| Server entry | `libra-governor daemon run --trusted-principal-config <abs-path>` (cfg-gated match arm). A default build sends that argv to the existing fallback arm, which exits 2. |
| Startup checks | One function, `validate_trusted_startup(&cfg, config_path) -> Result<LedgerStore, StartupError>`. It runs ADR checks 1 through 5 in order and returns the already-migrated ledger. Bind happens only after it succeeds. |
| Peer identity | `peer_uid(&UnixStream) -> io::Result<u32>`: `SO_PEERCRED` on Linux, `getpeereid` on Apple, `Unsupported` elsewhere. Classified right after `accept`, before any byte is read. Unknown peers (including root) are dropped. |
| Accept loop | New `trusted_principal::serve_trusted`. The existing `serve` and `handle_connection` are not modified. The only shared-code change is widening `server::dispatch` to `pub(crate)`. |
| Timeouts | One total per-connection deadline (not per read call) plus a request-size cap, enforced by a trusted-only reader wrapper. `wire.rs` is untouched. |
| Protocol mismatch | In trusted mode, a mismatch returns `Error` and the daemon keeps serving. No shutdown path exists in `serve_trusted`. |
| Confused deputy | A request-boundary sanitizer (`strip_agent_named_paths`), written as an exhaustive `match` over `Request`, blanks every agent-named path before `dispatch`. That covers 4 sinks, including two that ADR-0019 does not list. |
| Client | Inside `client.rs`'s existing connect functions: if the pinned config exists, connect only to its socket, verify the server's peer UID before returning the stream, and never spawn. If the config is absent, behavior is unchanged. If it is present but invalid, the client refuses. |
| Cargo | `daemon`: `trusted-principal = []`. `cli`: `trusted-principal = ["libra-governor-daemon/trusted-principal"]`. Neither crate gets a `default` entry. |
| Split | Three stacked PRs: 2a server core, 2b server race/restart evidence, 2c client. See §9. |

## 1. `TrustedPrincipalConfig`

New module tree, entirely `#[cfg(feature = "trusted-principal")]`:

```
crates/daemon/src/trusted_principal/
  mod.rs       // TrustedPrincipalConfig, PINNED_CONFIG_PATH, load + field validation, ConfigError
  startup.rs   // validate_trusted_startup, path-chain walk, StartupError
  peer.rs      // peer_uid, PeerClass, classify
  serve.rs     // bind_trusted_socket, serve_trusted, DeadlineReader, strip_agent_named_paths
```

`crates/daemon/src/lib.rs` gains one line: `#[cfg(feature = "trusted-principal")] pub mod trusted_principal;`

```rust
/// Compile-time pinned location the *client* reads. The daemon takes its
/// path from argv instead. See §5 for why the two differ.
pub const PINNED_CONFIG_PATH: &str = "/etc/libra-governor/trusted-principal.json";
pub const SOCKET_FILE_NAME: &str = "daemon.sock";
pub const MAX_REQUEST_BYTES: u64 = 8 * 1024 * 1024; // memory bound, not a traffic shaper

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustedPrincipalConfig {
    pub provider_uid: u32,
    pub agent_uids: Vec<u32>,
    /// Stored as the provider source_id by PR-3's AttestOutcome. Validated now.
    pub provider_id: String,
    pub state_dir: PathBuf,   // e.g. /var/lib/libra-governor
    pub socket_dir: PathBuf,  // e.g. /run/libra-governor ; socket = socket_dir/daemon.sock
    pub connection_timeout_ms: u64,
}

impl TrustedPrincipalConfig {
    /// Reads `path` (no env, no HOME, no XDG), parses it, and runs `validate_fields`.
    /// It does NOT check file ownership; `validate_trusted_startup` (server)
    /// and `client_pinned_endpoint` (client) each do that themselves.
    pub fn load(path: &Path) -> Result<Self, ConfigError>;
    fn validate_fields(&self) -> Result<(), ConfigError>;
    pub fn socket_path(&self) -> PathBuf;     // socket_dir.join(SOCKET_FILE_NAME)
    pub fn ledger_path(&self) -> PathBuf;     // state_dir.join("ledger.sqlite3")
    pub fn log_path(&self) -> PathBuf;        // state_dir.join("daemon.log")
    pub fn connection_timeout(&self) -> Duration;
}
```

`validate_fields` rejects each of the following with a distinct `ConfigError` variant:

- `provider_uid == 0`.
- `agent_uids` is empty, contains 0, or contains `provider_uid`.
- `provider_id` is empty, longer than 128 bytes, or starts with `unverified:` (ADR-0019 §1: dedup-namespace collision).
- `state_dir` or `socket_dir` is not absolute or is not lexically normal (any `.` or `..` component).
- `state_dir == socket_dir`, or either one is nested inside the other.
- `connection_timeout_ms` is outside `100..=60_000`. A zero timeout is an `EINVAL` from `set_*_timeout`, and `None` would mean "block forever" (see `cli/src/client.rs`'s own comment on `connect_only_with_budget`).

There are deliberately no fields for gateway, extensions, policy preset, recon budget, `secret_command`, or `outcome_authority`. `deny_unknown_fields` makes adding any of them a loud parse failure, which satisfies ADR-0019 §1: "a misconfiguration cannot turn them on". There is no `schema_version` field. `deny_unknown_fields` already makes a newer-shape file fail loudly on an older binary.

Example (the harness writes exactly this):

```json
{"provider_uid":10001,"agent_uids":[10002],"provider_id":"operator:lg-provider",
 "state_dir":"/var/lib/libra-governor","socket_dir":"/run/libra-governor","connection_timeout_ms":5000}
```

## 2. Peer-credential classification

`crates/daemon/src/trusted_principal/peer.rs`:

```rust
/// Kernel-attested effective UID of the peer on a connected Unix stream.
/// Server side: the client's UID at connect(). Client side: the server's UID at listen().
pub fn peer_uid(stream: &UnixStream) -> std::io::Result<u32>;

#[cfg(any(target_os = "linux", target_os = "android"))]
// getsockopt(fd, SOL_SOCKET, SO_PEERCRED, &mut libc::ucred, &mut len); rc != 0 -> last_os_error;
// len != size_of::<ucred>() -> InvalidData; Ok(cred.uid)

#[cfg(target_vendor = "apple")]
// libc::getpeereid(fd, &mut uid, &mut gid); rc != 0 -> last_os_error; Ok(uid)

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple")))]
// Err(io::ErrorKind::Unsupported). Compiles everywhere and fails closed.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerClass { Provider, Agent }

/// None means the peer is closed before a byte is read (ADR-0019 §1). That includes root.
pub fn classify(uid: u32, cfg: &TrustedPrincipalConfig) -> Option<PeerClass>;
```

- `libc` is already a direct dependency of both `daemon` and `cli` (workspace `libc = "0.2"`). Use it only for the two FFI calls above. For `getuid`/`geteuid` in startup checks, use the already-enabled `rustix` `process` feature (safe API).
- `std::os::unix::net::UnixStream::peer_cred` is still unstable. Do not use it.
- **Where it plugs in:** `serve_trusted`'s accept loop, immediately after `listener.incoming()` yields `Ok(stream)` and before any `set_*_timeout` or read:
  ```rust
  let class = match peer_uid(&stream).ok().and_then(|uid| classify(uid, trusted)) {
      Some(class) => class,
      None => { log::append_line(&config.log_path, "trusted: dropped unclassified peer"); continue; }
  };
  ```
  Dropping `stream` closes it. PR-2 logs the class per connection and does not otherwise branch on it. PR-3 threads `PeerClass` into the `AttestOutcome` gate.
- Do not interpolate UIDs into log, panic or error strings. PR-1 had to remove exactly this for CodeQL `rust/cleartext-logging` (commit `2b5980b`). Log the class name only.

## 3. Startup sequence

`crates/daemon/src/trusted_principal/startup.rs`:

```rust
pub fn validate_trusted_startup(
    cfg: &TrustedPrincipalConfig,
    config_path: &Path,
) -> Result<LedgerStore, StartupError>;
```

Ordered calls inside it. The first failure returns, and nothing is created on disk before step 4 passes:

1. `check_process_identity(cfg)`: `rustix::process::getuid()` and `geteuid()` both equal `cfg.provider_uid`, and that value is not 0. No privilege dropping here; the launcher does it.
2. `check_config_file(config_path)`: after `canonicalize`, `symlink_metadata` reports a regular file owned by uid 0 with `mode & 0o022 == 0`.
3. `check_path_chain(path, owners: &[0, cfg.provider_uid])` for each of `std::env::current_exe()?`, `config_path`, `cfg.state_dir`, and `cfg.socket_dir`:
   - **Canonicalize first, then walk** every component from the leaf up to `/` with `symlink_metadata`.
   - Each component must be owned by one of `owners` and have `mode & 0o022 == 0`.
   - Canonicalizing is required because macOS `/etc` and `/var` are root-owned symlinks into `/private`. It is safe because every resolved ancestor is then verified to be non-writable by the agent.
   - These are root-pinned paths, not agent-named ones, so canonicalizing them is not a confused-deputy read.
   - Verified against the PR-1 image: `/`, `/usr`, `/usr/local`, `/usr/local/libexec/libra-governor`, `/etc`, `/var`, `/var/lib` and `/run` are all `755 root:root`, so the walk passes there. Debian's historical `root:staff 2775 /usr/local` is not present in `rust:1.90-bookworm`.
4. `check_state_dir(cfg)`: owner is `provider_uid` and `mode & 0o7777 == 0o700`.
   `check_socket_dir(cfg)`: owner is `provider_uid`, `mode & 0o022 == 0` (no group or other write), and `mode & 0o007 == 0` (no other access). That accepts `2710` without requiring it byte-for-byte; ADR-0019 §1's "group bits exception" is just the group-traverse bit.
5. `LedgerStore::open(cfg.ledger_path())`, which applies migrations, then `check_append_only_triggers(&ledger)`. That compares `ledger.trigger_names()?` against `libra_governor_ledger::APPEND_ONLY_TRIGGERS`. Any missing name returns `StartupError::MissingTrigger(&'static str)`, and the ledger is dropped.

Return `Ok(ledger)`. The caller binds only after this, and `serve_trusted` reuses this same `LedgerStore` without reopening it.

**Testability split.** Steps 2 to 4 are a thin probe (`symlink_metadata` calls) feeding a pure evaluator: `fn evaluate_chain(components: &[(PathBuf, u32 /*uid*/, u32 /*mode*/, FileKind)], owners: &[u32]) -> Result<(), StartupError>`. Unit-test that evaluator without root, on Linux and macOS. The real probe is exercised by the harness.

**Ledger addition (not feature-gated, read-only, no behavior change).** Add to `crates/ledger/src/store.rs`:

```rust
pub fn trigger_names(&self) -> Result<Vec<String>, LedgerError>
// SELECT name FROM sqlite_master WHERE type = 'trigger' ORDER BY name
```

Also add `pub const APPEND_ONLY_TRIGGERS: [&str; 4]` next to the migrations and re-export it from `lib.rs`. Unit test: after `open_in_memory`, all four are present; after `DROP TRIGGER` plus reopen of a file DB, one is reported missing. This is also the regression proof for ADR-0019's "migrations do not recreate a dropped trigger". This is needed because `LedgerStore.conn` is `pub(crate)` and `daemon` has no `rusqlite` dependency.

**`StartupError`** is a `thiserror` enum with one variant per check: `ProcessIdentity`, `ConfigFile`, `PathChain { path }`, `StateDir`, `SocketDir`, `Ledger(LedgerError)`, `MissingTrigger(&'static str)`, `Io`. On failure, `run_trusted` prints the error to **stderr only** and exits 2. It does not write to `daemon.log`, because the state dir is not validated at that point.

## 4. Server behavior under the feature

### 4.1 Entry point (cli)

`crates/cli/src/main.rs`, a new arm that does not appear in `USAGE`/`--help` (the positioning guard watches `main.rs`):

```rust
#[cfg(feature = "trusted-principal")]
["daemon", "run", "--trusted-principal-config", path] =>
    std::process::exit(daemon_cmd::run_trusted(std::path::Path::new(path))),
```

`crates/cli/src/daemon_cmd.rs`:

- **Refactor first, with no behavior change.** Extract the single `DaemonConfig { .. }` literal in `run()` into `fn daemon_config(socket_path, ledger_path, log_path, policy, gateway, extensions) -> DaemonConfig`, which still hardcodes `outcome_authority: None`. This keeps the factual premise in `outcome_authority_not_wired_live.rs`'s doc comment true: exactly one production `DaemonConfig` construction site, in `daemon_cmd.rs`. The guard needs no edit.
- `#[cfg(feature = "trusted-principal")] pub fn run_trusted(config_path: &Path) -> i32`:
  1. `TrustedPrincipalConfig::load(config_path)`.
  2. `validate_trusted_startup(&cfg, config_path)`, which returns the `ledger`.
  3. `std::env::set_current_dir("/")`, so any relative path resolution is pinned to a root-owned directory (see §4.4).
  4. `daemon_config(cfg.socket_path(), cfg.ledger_path(), cfg.log_path(), default_admission_policy(), None, None)`. **`config.json` is never read.**
  5. `bind_trusted_socket(&cfg)`.
  6. Pidfile write to `cfg.state_dir` (same `PidRecord`).
  7. `serve_trusted(listener, ledger, &config, &cfg)`.

  It never calls `paths::*`.

### 4.2 Bind (`serve.rs`)

```rust
pub fn bind_trusted_socket(cfg: &TrustedPrincipalConfig) -> Result<UnixListener, DaemonError>
```

- Call the existing `server::bind_or_detect_running(&cfg.socket_path())` unchanged. Its stale-socket `remove_file` is now a provider-only operation, because nobody else can write `socket_dir`. Then call `set_permissions(socket, 0o660)`.
- **This is required, not cosmetic.** The default path hardens the socket to `0600`, and on Linux the agent needs write permission on the socket inode to `connect`. Without the `0660` step, every agent-facing harness test would pass with `EACCES` for the wrong reason.
- The group comes from the setgid `socket_dir` (`lg-clients`).
- Between the `0600` and `0660` chmods, only the provider can connect, which is harmless.

### 4.3 Accept loop, timeouts, no shutdown (`serve.rs`)

```rust
pub fn serve_trusted(listener: UnixListener, mut ledger: LedgerStore,
                     config: &DaemonConfig, trusted: &TrustedPrincipalConfig) -> Result<(), DaemonError>
```

- `server::reconcile_stale_reservations(&mut ledger, config)` (already `pub(crate)`).
- **No** `start_gateway` and **no** `start_extensions`. Both config fields are `None` anyway, but the calls are omitted entirely, so the provider spawns no subprocess and no thread.
- For each accepted stream: classify (§2), then compute `deadline = Instant::now() + trusted.connection_timeout()`, then call `handle_trusted_connection(stream, &mut ledger, &mut current_task, config, deadline)`. Log errors and continue. **There is no `break` anywhere in the loop.**

`handle_trusted_connection`:

```rust
stream.set_write_timeout(Some(trusted_timeout))?;
let reader = BufReader::new(DeadlineReader { stream: stream.try_clone()?, deadline }.take(MAX_REQUEST_BYTES));
let response = match wire::read_message::<_, RequestEnvelope>(reader) {
    Ok(env) if env.protocol_version != PROTOCOL_VERSION => Response::Error { message: /* same text as default */ },
    Ok(mut env) => { strip_agent_named_paths(&mut env.request); server::dispatch(env.request, ledger, current_task, config) }
    Err(e) => { log; Response::Error { message: "malformed request".into() } }
};
wire::write_message(&stream, &ResponseEnvelope { protocol_version: PROTOCOL_VERSION, response })?;
```

- `DeadlineReader` implements `Read` by setting `set_read_timeout(Some(deadline - now))` before every `read`, and returns `TimedOut` once the deadline has passed. This closes the slow-drip hole: `SO_RCVTIMEO` is per call, so a client sending one byte every 4.9 s would otherwise hold the serial loop forever.
- `.take(MAX_REQUEST_BYTES)` bounds `read_line`'s otherwise unbounded `String`.
- Shared-code change: `server::dispatch` goes from private to `pub(crate)`. That is visibility only. `serve` and `handle_connection` (and its 20 test call sites) are not touched, so default mode's `shut_down_after_reply` behavior is byte-identical.

### 4.4 Agent-named paths: `strip_agent_named_paths`

```rust
pub(crate) fn strip_agent_named_paths(request: &mut Request)
```

An **exhaustive `match` over all 9 `Request` variants, with no `_ =>` arm**. `Request` is not `#[non_exhaustive]`, so a future path-bearing variant becomes a compile error in feature builds. The same applies to the inner `match` over all 4 `ExecutionOperation` variants. The sinks it neutralizes:

| Sink | Where the deref happens today | Action |
|---|---|---|
| `Preflight.cwd` | `server.rs::handle_preflight` → `recon::run_recon(cwd)` (line ~839), **and** `features::derive_task_features` → `features::repo_key` → `cwd.canonicalize()` (features.rs:36), on every Preflight | `*cwd = PathBuf::new()` |
| `Finalize.transcript_path` | `server.rs::handle_finalize` → `usage::observe_transcript_window` (line ~1859) | `*transcript_path = None` |
| `ExecutionOwner` → `Prompt.cwd` | `execution_owner.rs:72` → `run_recon`, then `handle_preflight_prepared` → `repo_key` | `*cwd = PathBuf::new()` |
| `ExecutionOwner` → `Stop.transcript_path` | Ignored today (it passes `None` at execution_owner.rs:221) | `*transcript_path = None` anyway, for defense in depth |

- ADR-0019 names only the first two. **The `repo_key` canonicalize and the ExecutionOwner Prompt recon are additional confused-deputy sinks found in this pass.**
- **Audit completeness.** Non-test `server.rs`, `execution_owner.rs`, `contract.rs`, `features.rs`, `*_authority.rs`, `host_binding.rs`, `log.rs` and `pidfile.rs` were searched for `exists`/`is_dir`/`is_file`/`metadata`/`symlink_metadata`/`OpenOptions`/`File::open`/`read_to_string`/`read_dir`/`canonicalize`/`fs::`. Outside `recon.rs` and `usage.rs`, the only hits are on paths the provider chooses itself: the socket's own remove/chmod, `log_path`, the pidfile, and `handle_doctor`'s `config.json` exists-check beside `socket_path` (server.rs:2401). These are audited and harmless.
- **Why the empty path as the sentinel.**
  - `stat("")` and `realpath("")` return `ENOENT` by POSIX without resolving anything, and no principal can create an entry named `""`.
  - `run_recon` returns early on `!root.is_dir()` (recon.rs:117), before any `join`.
  - The alternative, a child of the provider's private state dir, would point any future `cwd.join(..)` at the provider's own secrets. That is the exact confused deputy this rule exists to prevent.
  - Step 3 of `run_trusted` pins the process cwd to `/`, so even a future relative join would resolve under a root-owned directory.
- **Costs, added to R9.**
  - Every trusted task gets `repo_key = sha256("")[..16]`, so estimator repo-bucketing collapses to a single repo.
  - The Preflight recon `reason` reads `cwd  is not an accessible directory`.
  - Neither cost is a trust issue.
- Rejected alternative: a mode field on `DaemonConfig`, consulted inside `handle_preflight`, `handle_finalize` and `execution_owner`. It would touch about 36 struct literals across 18 files, including `outcome_authority_wiring.rs`'s test fixture. A process-global flag would leak across unit tests in one test binary. Boundary sanitization keeps every shared handler untouched.

### 4.5 Environment audit (T7)

Env reads reachable from `serve_trusted`:

- `paths.rs` (`LIBRA_GOVERNOR_STATE_DIR`, `XDG_STATE_HOME`, `HOME`): never called in trusted mode. This is enforced by the guard in §6.
- `ledger/src/write.rs` `LIBRA_GOVERNOR_DOGFOOD_PROFILE`: a label read from the provider's own launcher-scrubbed env, not a path and not agent-controlled. Out of scope.
- `host_runtime/*`: not reachable from `dispatch`.

## 5. Client behavior under the feature

New file `crates/cli/src/trusted_client.rs` (cfg-gated):

```rust
pub(crate) struct PinnedEndpoint { socket_path: PathBuf, provider_uid: u32 }

/// Ok(None)  iff symlink_metadata(PINNED_CONFIG_PATH) is NotFound, so default behavior is unchanged.
/// Err(..)   for any other failure: unreadable, wrong owner or mode, unparsable, invalid fields.
///           Fail closed. Never fall back to default mode once the file exists.
pub(crate) fn pinned_endpoint() -> Result<Option<PinnedEndpoint>, ClientError>;

/// connect(), then peer_uid(&stream) == provider_uid, checked BEFORE the stream is
/// returned, so no caller can write a byte first. Then set read/write timeouts.
pub(crate) fn connect_verified(ep: &PinnedEndpoint, io_budget: Duration) -> Result<UnixStream, ClientError>;
```

Hooks in `crates/cli/src/client.rs`, as an early return at the top of each public connect function. Callers are unchanged.

```rust
#[cfg(feature = "trusted-principal")]
if let Some(ep) = crate::trusted_client::pinned_endpoint()? {
    return crate::trusted_client::connect_verified(&ep, /* REQUEST_TIMEOUT or `budget` */);
}
```

- This goes in `connect_only`, `connect_only_with_budget` (with `budget`, after its zero-budget check) and `ensure_daemon_connection`. `fire_and_forget` is covered through `connect_only`.
- In `ensure_daemon_connection`, the early return means `spawn_daemon_detached` is unreachable in trusted mode.
- The `socket_path` argument callers pass (derived from env by `paths::socket_path()`) is ignored in trusted mode.
- `ClientError` gains two cfg-gated variants: `TrustedConfig(String)` and `ServerIdentityMismatch`. The second has no UID payload in its `Display` (CodeQL note in §2).

`pinned_endpoint` validates the config file itself: canonicalize, then `check_path_chain(path, &[0])`, which requires root ownership all the way up, then `TrustedPrincipalConfig::load`. It does **not** walk `socket_dir`.

- The server already walks `socket_dir` at startup, and the client's authentication of the server is the peer UID.
- Not walking it is also what makes the client's peer-UID check falsifiable in the harness (T5c).

**Why the client uses a constant path and the daemon uses argv.**

- Hooks are invoked by the agent host with fixed argv, so they cannot carry a flag. A root-owned constant path is the only pin that needs no cooperation.
- The daemon is started by root or a service manager, which pass an explicit, auditable argv. That also lets harness tests start the daemon against deliberately broken configs.

**Consequence for the harness.** A config present at `/etc/libra-governor/trusted-principal.json` flips *every* client in the container into trusted mode. PR-1's `positive_control` (default-mode hook plus `outcome record` with `LIBRA_GOVERNOR_STATE_DIR`) would then panic. So:

- `provision.sh` must **not** write the config.
- Each PR-2 test installs it with a root-owned RAII guard and removes it on `Drop`.
- `reset_state_dirs()` also deletes any leftover config, so every test starts with it absent.

## 6. Cargo feature wiring and guards

`crates/daemon/Cargo.toml`:

```toml
[features]
# ADR-0019 §1: default-off; never add to `default`. Guarded by tests/trusted_principal_default_off_guard.rs.
trusted-principal = []
```

`crates/cli/Cargo.toml`:

```toml
[features]
# The only manifest line allowed to enable the daemon's feature (ADR-0019 §1, workspace feature unification).
trusted-principal = ["libra-governor-daemon/trusted-principal"]
```

- Neither crate gets a `default` key, and no other manifest names the feature.
- The harness crate keeps no dependency on `cli` or `daemon`. If its helper bins (§7) take `libc` for `SO_PEERCRED`, that is safe: `libc` has no such feature to unify.

**New guard file `crates/daemon/tests/trusted_principal_default_off_guard.rs`** (always compiled, runs in `cargo test --workspace`). It uses plain text scans in the same style as the existing guards:

1. `trusted_principal_is_declared_only_by_cli_and_daemon_and_never_default`. Across `Cargo.toml` and every `crates/*/Cargo.toml`, `trusted-principal` appears only in `crates/cli/Cargo.toml` and `crates/daemon/Cargo.toml`, and only on the two lines above. No `default = [...]` line contains it.
2. `no_install_script_or_non_harness_workflow_enables_trusted_principal`. Neither `scripts/install.sh` nor any `.github/workflows/*.yml` mentions it, except an explicit allowlist: `two-principal-harness.yml`, where ADR-0019 mandates the feature clippy step. `harness/two-principal/Dockerfile` is outside the scanned set by construction.
3. `trusted_mode_code_never_resolves_env_paths_or_spawns`. `crates/daemon/src/trusted_principal/**` and `crates/cli/src/trusted_client.rs` contain none of `paths::`, `env::var` or `Command::new`. This statically enforces "Trusted mode never calls `paths::state_dir()`" and "executes no subprocess" (ADR-0019 §1).

ADR-0019's third assertion ("every `AttestationSource::Provider` construction in `daemon/src` remains inside the allowlisted file") is **already enforced** by the existing, unmodified `outcome_authority_not_wired_live.rs`. PR-2 adds nothing it scans for, and PR-3 owns any extension of it.

- **Deviation from ADR-0019 §6:** the ADR places "guard additions" in PR-3. Guards 1 to 3 land in PR-2 because the feature flag lands in PR-2. Shipping a feature flag without its default-off guard would leave a review gap.
- **Trap:** the existing guards are substring scans that include comments. No new file under `daemon/src`, `cli/src` or `gateway/src` may spell any of these, even in a doc comment:
  - the two qualified authoritative variant paths of `AttestationSource`;
  - `authoritative:` immediately followed by a space and `true`;
  - `outcome_authority: Some(`;
  - `grant_renewal` (scanned by `renewal_not_wired_live.rs`).
  - New guard 3 also matches comments: `crates/daemon/src/trusted_principal/**` and `crates/cli/src/trusted_client.rs` must not spell `paths::`, `env::var` or `Command::new` even in prose. For example, write "never resolves the env-derived state directory", not a qualified function path.

**CI (each change needs explicit review per repo policy).** Changes go in `.github/workflows/two-principal-harness.yml` only, which keeps guard 2's allowlist to one file:

- **New job `trusted-principal-build`**, matrix `[ubuntu-latest, macos-latest]`, running:
  - `cargo clippy -p libra-governor-cli -p libra-governor-daemon --features trusted-principal --all-targets -- -D warnings`
  - `cargo test -p libra-governor-cli -p libra-governor-daemon --features trusted-principal`
- Running the whole `cli`/`daemon` suite with the feature compiled in (and no pinned config on the runner) is the mechanical proof that default-mode behavior is unchanged.
- The macOS leg is compile plus unit tests only. `peer_uid` on a `UnixStream::pair()` exercises the real `getpeereid` path. That is not running the harness on macOS: ADR-0019 R8's Phase 2 remains separate, and nothing runs on the dev Mac.
- `harness/two-principal/Dockerfile`'s build step becomes `cargo build --release -p libra-governor-cli --locked --features trusted-principal`.

## 7. Harness plan (`crates/two-principal-harness`)

**Fixture additions (`src/lib.rs`).** PR-1's API stays unchanged.

- Constants: `TRUSTED_CONFIG_PATH`, `PROVIDER_SOCKET_DIR = "/run/libra-governor"`, `PROVIDER_SOCKET`, `LG_CLIENTS_GID = 11000`.
- `TrustedConfigGuard::install(path, &ConfigSpec) -> Self`: writes `root:root 0644` and removes it on `Drop`.
- `reset_state_dirs()`: extended to also remove `TRUSTED_CONFIG_PATH` and recreate `/run/libra-governor` as `lg-provider:lg-clients 2710`.
- `TrustedDaemon::start(config_path, envs, timeout)`: `daemon run --trusted-principal-config <p>` as `lg-provider`. Readiness is `connect()` succeeding on `PROVIDER_SOCKET`. Root's connection is dropped unclassified, which is fine.
- `TrustedDaemon::start_expecting_refusal(principal, config_path, envs) -> Output`: asserts exit is non-zero within the timeout and that no socket appeared.
- `trusted_positive_control()`: an agent `Preflight` creates a task in the provider ledger, then provider-UID `outcome record` lands a row. That row is non-authoritative; authoritative attestation is PR-3.
  - In 2a/2b, **both halves** (the agent `Preflight` and the provider `RecordOutcome`) are sent with `lg-raw-client`. Before 2c no CLI client knows the pinned endpoint.
  - In 2c, it switches to the real `hook user-prompt-submit` and `outcome record`.
  - **No test ever sets `LIBRA_GOVERNOR_STATE_DIR` to `/run/libra-governor` or `/var/lib/libra-governor`.** CLI paths that call `paths::ensure_state_dir()`, such as the hook at `agent/run.rs:102`, would chmod that directory to `0700`. That strips `lg-clients` traverse, and every later agent test would then fail with `EACCES` for the wrong reason.

**Helper binaries** (`src/bin/`, std-only plus optional `libc`; the image has no socat and python is not guaranteed). Cargo builds them for integration tests and exposes them as `env!("CARGO_BIN_EXE_<name>")`.

- `lg-fake-server <socket> <count-file>`: binds, accepts in a loop, and records total bytes received.
- `lg-raw-client <socket> <json-line> [--print-peer-uid]`: sends one raw line, prints the reply, and optionally prints the server's peer UID.

**Test files.** Every test is `#[ignore = "runs only inside harness/two-principal/run.sh's disposable container"]`, starts with `assert_disposable(); reset_state_dirs();`, and uses the 3-pass pattern: positive, then sensitivity (same attack against the same-UID or default layout must succeed), then two-principal (must fail, ledger snapshot unchanged, provider alive).

| File | Test fns | Sensitivity control (must succeed) | Two-principal assertion |
|---|---|---|---|
| `t05_socket_impersonation.rs` (2c) | `t05a_agent_cannot_create_rename_or_unlink_in_the_socket_dir` | Agent creates, renames and binds in `/srv/lg-sameuid` | `touch`/`mv`/`rm`/`ln -s`/`lg-fake-server` bind in `/run/libra-governor` all fail; provider's socket inode unchanged |
| | `t05b_env_pollution_never_redirects_a_trusted_client` | Config absent: a client with `LIBRA_GOVERNOR_STATE_DIR` set to the fake dir delivers bytes to `lg-fake-server` (count > 0) | Config present: same env; client reaches the real provider and fake count == 0 |
| | `t05c_trusted_client_refuses_a_server_with_the_wrong_peer_uid` | Root-written config pointing at the agent's fake socket with `provider_uid = 10002` and `agent_uids = [10001]` (otherwise `validate_fields` rejects it before connect): fake count > 0 | Same config with `provider_uid = 10001`: client exits non-zero with identity mismatch; fake count == 0 |
| `t07_state_dir_override.rs` (2a) | `t07a_provider_ignores_state_dir_env` | Default-mode daemon given `LIBRA_GOVERNOR_STATE_DIR=/tmp/lg-agent-state` creates its ledger there | Trusted daemon given the same plus `XDG_STATE_HOME`/`HOME`: ledger only in `/var/lib/libra-governor`; `/tmp/lg-agent-state` empty |
| | `t07b_agent_side_override_only_redirects_the_agent` (2b) | Agent's own default daemon receives its outcome row | Provider `LedgerSnapshot` before == after |
| `t08_no_auto_spawn.rs` (2c, plus 2a for `t08b`) | `t08a_trusted_hook_never_spawns_a_daemon` | Config absent: `lg-agent` hook spawns an `lg-agent`-owned `daemon run` | Config present, provider stopped: hook reports unavailable; no `lg-agent` daemon process after `SPAWN_WAIT_BUDGET` plus margin |
| | `t08b_forced_trusted_start_as_agent_refuses` (2a) | The same command as `lg-provider` starts | As `lg-agent`: non-zero exit (UID check), no socket, no process; an agent-written config in its home is also refused (not root-owned) |
| `t10_restart.rs` (2a: `t10b`, 2b: `t10c`) | `t10b_newer_protocol_envelope_does_not_shut_down_the_provider` | Same envelope (`protocol_version: 4000000000`) to a same-UID default daemon makes it exit | Trusted provider replies `Error`, is still alive, and serves a following `Status` |
| | `t10c_down_window_create_bind_connect_all_fail` | Agent can bind or plant at the socket path in `/srv/lg-sameuid` | Provider `kill -9`'d, so a stale socket remains. An agent loop of create/bind/unlink in both dirs fails, and connect gets `ECONNREFUSED`. Root restarts; the new provider unlinks the stale socket, binds, and the socket owner is `lg-provider` |
| `t11_races.rs` (2b) | `t11a_pinned_socket_is_never_served_by_a_non_provider_peer` | In the same-UID layout, the agent's spinning `lg-fake-server` wins the path at least once across restarts | 50 root-driven `kill -9`/restart cycles while the agent spins bind/connect. Every successful `lg-raw-client --print-peer-uid` probe reports 10001 |
| `x_confused_deputy.rs` (2a) | `x_confused_deputy_provider_never_opens_agent_named_paths` | Same-UID default daemon: `Preflight` then, in the **same session**, `Finalize{transcript_path: <FIFO>}` opens the FIFO (watcher sees an opener). `Preflight{cwd:/srv/lg-sameuid}` scans > 0 files | Trusted provider: `Preflight{cwd:"/var/lib/libra-governor"}` returns `files_scanned == 0` with low confidence. A same-session `Finalize{transcript_path: <FIFO>}` must return `Finalized`; that response assertion is the guard, because `NoActiveTask` returns before the transcript read and would pass vacuously. The FIFO watcher sees no opener for 2 s, and `daemon.log` corroborates with usage `no_transcript_path` |
| `x_startup_checks.rs` (2a) | `x_startup_refuses_wrong_uid`, `..._group_writable_config`, `..._agent_writable_ancestor`, `..._state_dir_not_0700`, `..._socket_dir_other_accessible`, `..._missing_append_only_trigger` | Each run is preceded by a passing start on the correct layout | Each broken layout exits non-zero, with no socket and no log line. The trigger case: root drops `trg_outcome_attestations_no_delete` offline, then restarts |

**T5 deviation from ADR-0019 §3.** The ADR's T5 proof has the env-polluted client "refuse on peer-UID mismatch". With env ignored entirely, env pollution can no longer reach a fake server at all, which is a stronger outcome. The peer-UID check is therefore proven separately by `t05c` with a root-written config. This split is intentional, not a missed requirement.

The last row moves the startup-refusal half of ADR-0019's T4 proof from PR-3 into PR-2. The check's code lands in PR-2 and should not ship unproven. PR-3 then only references it.

## 8. Scope boundary: PR-2 must NOT

- Construct `AttestationSource`'s authoritative variants, add `Request::AttestOutcome`, or add an `outcome attest` CLI (PR-3).
- Touch `crates/daemon/src/outcome_authority_wiring.rs` or `crates/daemon/tests/outcome_authority_not_wired_live.rs`, or edit either guard's allowlist. Running `cargo test -p libra-governor-daemon --test outcome_authority_not_wired_live` must pass unmodified.
- Set `DaemonConfig::outcome_authority` to anything but `None`, or add a second production `DaemonConfig` literal (use the `daemon_config` helper).
- Reference `grant_renewal` from `daemon`/`cli`/`gateway`, or make any admission or renewal path read a new table. `renewal_not_wired_live.rs` must pass unmodified.
- Add `trusted-principal` to any `default` feature list, to `scripts/install.sh`, or to any workflow other than `two-principal-harness.yml`. Add no harness Cargo dependency on `cli` or `daemon`.
- Change `server::serve`, `server::handle_connection`, `server::bind_or_detect_running`, `paths.rs`, `wire.rs`, `PROTOCOL_VERSION`, or any `Request`/`Response` shape.
- Make `provision.sh` write the trusted config globally, or modify any PR-1 test file. **PR-1's tests must pass unmodified against the feature-enabled binary.**
- Advertise `--trusted-principal-config` in `USAGE`/`--help`.
- Run any harness test outside `harness/two-principal/run.sh`'s container, or run anything on the dev Mac beyond `cargo check`/`cargo test --features trusted-principal`.

## 9. Size and split

Estimated diff, including doc comments:

| Piece | Prod | Unit + guard tests | Harness |
|---|---|---|---|
| Server: config, startup, peer, serve, sanitizer, `run_trusted`, ledger `trigger_names`, Cargo | ~500 | ~300 | — |
| Client: `trusted_client.rs` + `client.rs` hooks | ~110 | ~60 | — |
| Fixtures + 2 helper bins + Dockerfile/workflow | — | — | ~300 |
| 7 harness test files | — | — | ~1,150 |

This does not fit one ~500-line PR. ADR-0019's two-way server/client split still leaves the server half at about 1,700 lines. **The recommendation is three stacked PRs, each shipping its own proof and each keeping PR-1 green.**

- **PR-2a: server core.** Prod about 500 plus unit and guard tests about 300 plus harness about 650.
  - Code: feature wiring, guards, config, startup checks, peer classification, `serve_trusted` (deadline, size cap, no shutdown), sanitizer, `run_trusted`, ledger read method, Dockerfile flag, and the workflow job.
  - Harness: `x_startup_checks` (including `t08b`), `t07a`, `t10b`, `x_confused_deputy`, plus fixtures and `lg-raw-client`.
- **PR-2b: server race and restart evidence.** No product code; harness only, about 450.
  - `t10c`, `t11a`, `t07b`, plus `lg-fake-server`.
  - Same character as PR-1: evidence against already-merged, still-gated code.
- **PR-2c: client.** Prod about 110 plus unit tests about 60 plus harness about 400.
  - Pinned endpoint, no spawn, server peer-UID verification.
  - Harness: `t05a`/`b`/`c` and `t08a`. `trusted_positive_control` switches to the real hook.

Order: 2a, then 2b and 2c (independent of each other; both stack on 2a). Product-code review load stays at or under about 500 lines per PR, and the overage is mechanical three-pass harness tests. If a reviewer accepts that rationale, 2a and 2b can merge as one PR.

## 10. Acceptance criteria (all three PRs)

- The four repo checks (`fmt`, `clippy --workspace --all-targets -D warnings`, `test --workspace`, `build --workspace`) are green **without** the feature.
- `cargo clippy` and `cargo test -p libra-governor-cli -p libra-governor-daemon --features trusted-principal` are green on Linux and macOS.
- `harness/two-principal/run.sh` passes locally and in CI. That includes all PR-1 tests unchanged plus the new tests for that PR.
- In a default build, `libra-governor daemon run --trusted-principal-config /x` exits 2. A cli integration test asserts this.
- `outcome_authority_not_wired_live.rs` and `renewal_not_wired_live.rs` are byte-identical to `main`.
- Independent reviewer reproduction (ADR-0019 R10) remains required before any feature-gate removal. That removal is a separate decision.
