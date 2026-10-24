# Egress inventory (HORO-2218)

Companion to [ADR-0026 "Egress inventory"](adr/0026-local-monthly-shared-budget.md).
Audited at `main` 7af0f89 across the Rust workspace, `providers/`, `scripts/`,
`integrations/`, `harness/`, `tools/` and `.github/workflows/`.

## Bottom line

- **Libra's own runtime has exactly two network-capable crates**:
  `crates/gateway` (opt-in, allowlisted LLM upstream) and `crates/extension`
  (loopback-literal webhooks). Every other workspace crate has no network
  dependency and no client or socket-dial code. A static test enforces this.
- **No telemetry, update checker, or batch/monthly upload exists.** Monthly
  upload is a separate, unbuilt workstream (ADR-0026).
- **Evidence export is local-only for every profile.** The adapter refuses
  every transport other than `LocalOnly`; `evidence-report` and
  `dogfood-evidence export` write local files and make no network call.
- **Not enforced by Libra:** Claude Code's own telemetry, WebFetch, and
  `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` are *host* traffic. Libra does
  not set or enforce them. `doctor` reports whether
  `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` is set (slice S8, HORO-2217):
  presence only, in its own environment and in the `settings.json` env block;
  it never prints the value or any other variable. `doctor`'s `telemetry`
  finding separately states that Libra itself has no telemetry code path.

## Authorized vs unauthorized

| Class | Status | Authorizer |
|---|---|---|
| LLM gateway traffic to the allowlisted upstream | Authorized, **opt-in** (off by default); carries prompts and responses by design because the agent routes them there | Operator sets `ANTHROPIC_BASE_URL` to the gateway and enables `[gateway]` |
| Loopback extension webhooks (business context, policy, events, outcome) | Authorized, opt-in; carries task/session ids, `repo_key`, `cwd`, outcome data, never prompt text (field set pinned in `crates/extension/src/wire.rs`) | Operator configures `[extensions]` |
| Operator-run provider processes (`providers/`) calling Jira/GitHub | Outside Libra's runtime; started and configured by the operator, not by the daemon | Operator |
| Telemetry, crash reports, update checks | **None exist** | n/a |
| Ledger/evidence batch upload (manual or monthly) | **Does not exist**; refused at config load by the adapter | n/a |

## Pathways

| # | Pathway | Where | Destination rule | Authorized by | Can carry ledger/evidence/work data? | Pinning test |
|---|---|---|---|---|---|---|
| 1 | Gateway outbound HTTPS client | `crates/gateway/src/server.rs:99-121` (client), `proxy.rs:663-684` (URL build) | Scheme `https` (plaintext only for a flagged loopback literal), host must be in `upstream_host_allowlist` (default `api.anthropic.com`), no userinfo/path, port 443 unless opted in, redirects never followed, URL is config + constant route path only (`config.rs:272-305`) | Operator config and routing | Yes: request/response bodies and the upstream credential, by design. The ledger stores no body/header columns | `crates/gateway/tests/ssrf_and_config.rs`, `config.rs` unit tests, `proxy_lifecycle.rs` |
| 2 | Gateway listener | `crates/gateway/src/server.rs:146` | Bind address must be loopback (`config.rs:247`) | Operator | Inbound only | `ssrf_and_config.rs` (non-loopback bind) |
| 3 | Extension webhook client | `crates/extension/src/client.rs:86` (single `TcpStream::connect`, no redirects, no DNS name), reused by `dispatcher.rs:71` | URL host must be exactly `127.0.0.1` or `[::1]`, scheme `http`, no userinfo/query/fragment (`config.rs:195-241`) | Operator `[extensions]` | Yes, to a loopback process the operator runs (ids, `repo_key`, `cwd`, outcomes) | `crates/extension/src/config.rs` tests (including HORO-2218 spelling and smuggling cases on all three surfaces) |
| 4 | Daemon constructs the extension client | `crates/daemon/src/server.rs:221` | Uses only validated config from #3 | Operator | As #3 | as #3, `crates/daemon/tests/extension_integration.rs` |
| 5 | Secret/credential helper subprocesses | `crates/gateway/src/credential.rs:143`, `crates/extension/src/secret.rs:113` | Operator-chosen program; Libra imposes no network rule on it | Operator config | Receives no ledger data; returns a secret | `credential.rs`/`secret.rs` unit tests (behaviour, not egress) |
| 6 | Quota-source executable | `crates/quota-observation/src/runner.rs:234` | Operator-chosen host CLI, environment cleared | Operator | None sent by Libra; host CLI's own traffic is host traffic | `quota-observation` conformance tests |
| 7 | Launch wrapper child | `crates/cli/src/run_cmd.rs:398` | Runs the user's own command unchanged | User | Child inherits the environment; its traffic is the user's | `launch_run.rs` |
| 8 | Codex app-server child | `crates/cli/src/codex_app_server_client.rs:121` | Spawns the operator's `codex`; talks to it over stdio | Operator | Stdio only; Codex's own network is host traffic | `codex_verify_trust_never_implicit.rs` |
| 9 | Daemon IPC | `crates/cli/src/client.rs:57`, `crates/daemon/src/server.rs:396` | Unix-domain socket only | n/a (local) | Local | daemon tests |
| 10 | Daemon self-spawn / process control | `crates/cli/src/client.rs:170`, `daemon_cmd.rs:290-454` (`kill`, `ps`, `sh`) | Local processes | n/a | No | n/a |
| 10a | Daemon host-adapter launches | `crates/daemon/src/host_runtime/exec.rs:96-101` (`VerifiedLaunch`), `:353` (`execve`), `:931-935` (`fork`) | Runs a recorded, trust-verified executable; Libra applies no network rule to the child | Operator trust decision | Child receives only its launch request; the child's own traffic is host traffic | `crates/daemon/tests/execution_owner.rs`, `host_registry_dispatch.rs` |
| 11 | Evidence export | `crates/cli/src/evidence_report_cmd.rs`, `dogfood_evidence_cmd.rs`, `crates/evidence-adapter/src/adapter.rs:56-100` | `TransportPolicy::LocalOnly` only; `ManualWindow`/`MonthlyWindow` refused at config load | n/a | Writes local files only | `adapter_fixtures.rs` (DFC-ADAPT-08 and the exhaustive HORO-2218 test), `no_network_symbols.rs` in `evidence-adapter` and `quota-source`, module scans in the two CLI files, `evidence_report_privacy.rs` |
| 12 | Python providers | `providers/common/safe_https_client.py`, `providers/jira/`, `providers/github/` | Operator-supplied base URL, https, SSRF-validated and IP-pinned; inbound side loopback-only | Operator starts them | Jira/GitHub lookups and outcome posts | `providers/common/test_safe_https_client.py`, `test_libra_provider_runtime.py` |
| 13 | `scripts/install.sh` | `scripts/install.sh:69` (`cargo install --path crates/cli --locked`) | Builds from a local clone, but cargo fetches crates from the registry, so this **does hit the network**; the script has no download code of its own | **Operator action**, not runtime | Registry fetch only; sends no ledger data | `crates/cli/tests/install_script.rs` (help only) |
| 14 | `harness/two-principal/Dockerfile` | `apt-get update`/`install` at line 67 | Image build for the dev harness | Developer | Not shipped | n/a |
| 15 | `experiments/phase0` (Python) | `experiments/phase0/src/phase0/sources/swebench.py`, `s3_trajs.py` (dataset sources) | Workspace-excluded research code | Researcher | Not shipped | n/a |
| 16 | CI/release workflows | `.github/workflows/*` | GitHub-hosted | Maintainers | Not part of the shipped runtime | n/a |

Checked and absent: `reqwest`, `ureq`, `curl`, `isahc`, `surf`, `attohttpc`,
`tungstenite`, `native-tls`, `openssl` (none in `Cargo.lock`); `UdpSocket`,
`lookup_host`, `to_socket_addrs`; `Command::new` of `curl`/`wget`/`ssh`/`scp`/
`git`/`nc`; any background timer that phones home.

## What is enforced by code and tests

- Static guard `crates/cli/tests/egress_static_guard.rs` (runs in `cargo test`):
  - manifests are read structurally with `cargo metadata --no-deps --offline`,
    so inline and multi-line tables, multi-line `features` arrays,
    `package =` renames, `workspace = true` inheritance and target-specific
    tables are all resolved by cargo. No workspace crate other than `gateway`
    and `extension` may depend on a network-capable crate (an explicit list
    plus the `opentelemetry*`, `sentry*`, `aws-*`, `gix*`, `hickory-*` families;
    `tokio` with `net` or `full`; `nix` with `socket`/`net`). The one exception
    is the `daemon` dev-dependencies `hyper`, `hyper-util` and `tokio`, used to
    host a loopback fake provider in integration tests (not linked into the
    shipped binary);
  - no other crate's `src/`, `tests/`, `benches/`, `examples/`, `build.rs`
    contains, after whitespace removal (so line breaks inside a path do not
    hide it), a client constructor, socket dial (including `libc::connect`,
    `libc::getaddrinfo`, `rustix::net`), DNS lookup call, a `Command::new` of
    a network program (`curl`, `wget`, `ssh`, `git`, `gh`, `pip`, ...), or of a
    shell/interpreter/`cargo` given network-looking arguments;
  - `ProviderClient`, `post_signed`, `libra_governor_extension::client` and
    `libra_governor_gateway::server` may be named outside `gateway`/`extension`
    only in `crates/daemon/src/server.rs` (the allowlist is checked for
    staleness). `daemon` and `cli` link `gateway` and `extension`, so their
    binaries are network-capable transitively; the guard constrains who may
    call the entry points, not that linkage;
  - `Cargo.lock` contains none of the forbidden HTTP-client, TLS, telemetry
    and cloud-SDK crate names (a name-based check, not a dependency-tree walk);
  - only `daemon` and `cli` may depend on `gateway`/`extension`;
  - every `crates/*` directory is a workspace member.
- Gateway and extension destination rules and the evidence transport policy,
  as pinned by the tests in the table.

## What is NOT enforced

- The guard is textual. Known limits: a type aliased with `use ... as`, a
  command string built at runtime, an operator-configured helper program, a
  child process's own traffic, a renamed or vendored copy of a forbidden crate
  in `Cargo.lock` (the lock check is name-based), and the dependencies of the
  allowlisted crates beyond their reviewed stack.
- `gateway` and `extension` source are exempt from the source scan; their
  behaviour is pinned by their own tests, not by the guard.
- The gateway allowlist is operator-extensible; an operator who adds a host
  authorizes it. `localhost` is accepted as a loopback name for the opt-in
  plaintext upstream (and still needs to be allowlisted), so it relies on
  local name resolution; the extension surfaces refuse it. The gateway's
  `is_loopback_host` accepts all of `127.0.0.0/8` (and `::1`) plus `localhost`;
  the extension accepts only the literals `127.0.0.1` and `[::1]`.
- The corporate/personal `Profile` is a data stamp on exported events. It
  changes no network behaviour; non-local transport is refused for every
  profile because the adapter config has no profile input at all.
- Claude Code telemetry, WebFetch and `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC`
  are not enforced by Libra. `doctor` reports only whether
  `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC` is set (slice S8, HORO-2217;
  delivered, reporting only, presence only). Telemetry and WebFetch are
  neither read nor reported.
- Nothing prevents a user from pointing the agent at a different base URL;
  the gateway only governs traffic routed through it.

## Changing the allowlists

Editing `DEP_ALLOWLIST`, `SOURCE_ALLOWLIST`, `FALSE_POSITIVE_EXCEPTIONS` or
`GATEWAY_EXTENSION_DEPENDENTS` in the guard is a deliberate egress decision:
update this document and ADR-0026 in the same change.
