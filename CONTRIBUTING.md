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

## If you run `libra-governor install` from a development build

`install` wires whatever `std::env::current_exe()` resolves to into the live
hook config (`~/.claude/settings.json` or `~/.codex/hooks.json`) verbatim,
permanently, until the next `install`. If you installed from a cargo build
output path (`target/debug/...` or `target/release/...` -- a shared
cross-worktree target dir makes this worse, since many worktrees' builds
land in the same path), a later `cargo build` into that same directory will
silently replace the binary every live hook invocation executes with
whatever happens to be on that branch at that moment, including
mid-development, unreviewed code. `install` now warns about this (see
`crates/cli/src/install_cmd.rs::warn_if_build_output_path`), but it does not
refuse -- some workflows genuinely want this. `libra-governor doctor` also
independently detects the resulting drift after the fact (`stale_runtime`
finding, comparing the running daemon's own executable checksum against the
one currently installed at the hook path) if a warning was missed.

If you are installing something meant to serve real hook traffic rather
than a disposable development session, copy the binary to a stable path
outside any cargo target directory first, and point `install` at that copy
instead of running it straight from `target/`.

## Code of conduct

Be respectful and constructive. Report unacceptable behavior per
[`SECURITY.md`](SECURITY.md) if it involves a security or safety concern,
or to the repository maintainers otherwise.
