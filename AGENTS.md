# AGENTS.md — libra-governor

Instructions for Codex and other coding-agent tools working in this
repository. This is a concise, agent-tool-agnostic pointer to the same
conventions documented in `CLAUDE.md`, `PRODUCT.md`, `ARCHITECTURE.md`,
and `CONTRIBUTING.md` — read those for full detail.

## North Star

> Never start work you are unlikely to afford to finish.

Estimate the full Definition of Done before admission, reserve enough
resources to finish, and re-plan only when the expected benefit of
replanning exceeds its own cost, switching cost, and delay. See
`PRODUCT.md` for the full North Star, Golden Journeys, and non-goals.

## Build / test / lint commands

```bash
cargo build --workspace
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

All four must pass before a change is considered complete.

## Architecture

`Claude Code -> deterministic hooks/statusline -> local Governor daemon -> optional enforcement gateway -> LLM provider`

- `crates/daemon` is the source of truth (state machine, ledger, policy).
- `crates/cli` is a thin client (`libra-governor` binary).
- `crates/domain`, `crates/protocol`, `crates/ledger`, `crates/estimator`
  are library crates with no independent policy authority.
- `integrations/claude-code` hooks are deterministic and carry no policy
  logic of their own.
- MCP is explain/query/manual-control only — never an enforcement
  boundary.
- Full prompt/source/tool output stays local by default.

Full detail: `ARCHITECTURE.md` and `docs/adr/0001-initial-architecture.md`.

This is a bootstrap-only ticket (HORO-1118): every crate is a placeholder
with no real domain logic. Do not implement estimator, ledger, or
admission-policy logic against this scaffold — that work is scoped to
follow-up tickets.

## Workflow conventions

- One branch per ticket: `<version-or-phase>/<ticket-id>/<short_summary>`,
  branched from `main`.
- Small, atomic commits, Gitmoji format: `<emoji> (<scope>): <summary>.`
- PRs only — no direct pushes to `main`. PR title:
  `[<ticket ID>] <emoji> (<scope>): <summary>`, body per
  `.github/PULL_REQUEST_TEMPLATE.md`.
- Merge strategy: merge commit only — never squash or rebase.

## Security

No real credentials anywhere in this repository, including test
fixtures — use env vars or a secret store instead. See `SECURITY.md`.
