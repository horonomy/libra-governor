# Contributing to Libra Governor

Thank you for your interest in contributing. This document encodes the
workflow rules used by both human contributors and AI coding agents
working in this repository.

## North Star

Read [`PRODUCT.md`](PRODUCT.md) first. Every change should serve the North
Star: *"Never start work you are unlikely to afford to finish."*

## Branching

One branch per ticket, created from `main`:

```
<version-or-phase>/<ticket-id>/<short_summary>
```

Example: `phase0/HORO-1118/bootstrap_repo`

Use one git worktree per ticket where practical, so multiple tickets can
be developed concurrently without branch-switching in a single working
tree.

## Commits

Make many small, atomic, logical commits rather than one large commit.
Each commit should represent one identifiable unit of work — a reviewer
should be able to understand what changed and why from the subject line
alone.

### Commit message format (Gitmoji)

```
<emoji> (<scope>): <summary>.
```

Example: `🔧 (workspace): Add minimal Rust workspace skeleton.`

| Emoji | Scope |
|---|---|
| `✨` | New feature |
| `🐛` | Bug fix |
| `♻️` | Refactor |
| `✅` | Tests |
| `📝` | Documentation |
| `🔧` | Configuration |
| `🔌` | Integrations |
| `🪝` | Hooks |
| `🚨` | Fix lint / type errors |
| `⬆️` | Dependency upgrade |
| `🗑️` | Delete / remove |

### What not to bundle in one commit

- A new module and its tests (two separate commits).
- Two unrelated fixes.
- A feature and an unrelated refactor.

## Pull requests

All changes land via pull request — **no direct pushes to `main`**.

### PR title format

```
[<ticket ID>] <emoji> (<scope>): <summary>
```

Example: `[HORO-1118] 🔧 (bootstrap): Bootstrap libra-governor repository scaffold`

### PR description

Use the repository's [pull request template](.github/PULL_REQUEST_TEMPLATE.md),
which covers: the linked Jira ticket, what changed, why, how to verify
(tests/commands run), security considerations, UI screenshot evidence
(if applicable), and known limitations.

### Merge strategy

This repository merges pull requests with a **merge commit** — never
squash or rebase — so that the atomic commit history described above
remains intact and reviewable after merge.

## Before opening a PR

Run, and ensure all pass:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace
cargo test --workspace
```

## Code of conduct

Be respectful and constructive. Report unacceptable behavior per
[`SECURITY.md`](SECURITY.md) if it involves a security or safety concern,
or to the repository maintainers otherwise.
