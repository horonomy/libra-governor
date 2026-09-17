# CLAUDE.md — libra-governor

Repository-specific instructions for Claude Code working in this repo.
Global behavioral policy lives in `~/.claude/CLAUDE.md`; this file is the
project-specific layer described there.

## North Star

Read [`PRODUCT.md`](PRODUCT.md) before proposing any feature or design
change:

> Never start work you are unlikely to afford to finish.

Supporting invariant: estimate the full Definition of Done before
admission, reserve enough resources to finish, and re-plan only when the
expected benefit of replanning exceeds its own cost, switching cost, and
delay.

Architecture and component boundaries are recorded in
[`ARCHITECTURE.md`](ARCHITECTURE.md) and
[`docs/adr/0001-initial-architecture.md`](docs/adr/0001-initial-architecture.md).

## Repository identity

- **Language / runtime:** Rust (stable toolchain), Cargo workspace.
- **Binary:** `libra-governor` (in `crates/cli`).
- **Storage (planned):** SQLite, WAL mode, local-only by default.

## Build and test commands

```bash
cargo build --workspace
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Run all four before considering any change complete. CI runs the same
four checks (see `.github/workflows/ci.yml`) plus a gitleaks secret scan.

## Architecture constraints

- `crates/domain`, `crates/protocol`, `crates/ledger`, `crates/estimator`:
  library crates. No binary targets.
- `crates/daemon`: the source-of-truth state machine, ledger, and policy
  engine. Admission and policy decisions belong here, not in hooks, MCP,
  or the CLI.
- `crates/cli`: the `libra-governor` binary. Thin client over the daemon;
  no independent policy logic.
- `integrations/claude-code`: deterministic lifecycle hooks. No policy
  logic — hooks call the daemon and relay its decision.
- MCP surfaces (when added) are explain/query/manual-control only, never
  a security or enforcement boundary. See `ARCHITECTURE.md`.
- Full prompt/source/tool output stays local by default (privacy
  invariant) — do not introduce a code path that transmits this content
  off-machine without an explicit, separately reviewed decision.

This bootstrap ticket (HORO-1118) intentionally contains no real domain
logic — every crate is a placeholder scaffold. Do not add estimator,
ledger, or admission-policy logic here; that is scoped to follow-up
tickets (e.g. HORO-1124).

## Branch / commit / PR conventions

Mirrors [`CONTRIBUTING.md`](CONTRIBUTING.md) — read that file for full
detail. Summary:

- Branch: `<version-or-phase>/<ticket-id>/<short_summary>`, one worktree
  per ticket, branched from `main`.
- Commits: small, atomic, Gitmoji format `<emoji> (<scope>): <summary>.`
- PRs: title `[<ticket ID>] <emoji> (<scope>): <summary>`, filled out per
  `.github/PULL_REQUEST_TEMPLATE.md`.
- Merge strategy: **merge commit only** — never squash or rebase in this
  repository.
- No direct pushes to `main`. All changes land via PR.

## Security

No real credentials in any file, ever — including test fixtures. See
[`SECURITY.md`](SECURITY.md) for the reporting process and secret-handling
policy. This repository has GitHub secret scanning and push protection
enabled, plus a gitleaks CI step.
