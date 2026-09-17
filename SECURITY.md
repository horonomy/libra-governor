# Security Policy

## Reporting a vulnerability

If you discover a security vulnerability in Libra Governor, please report
it privately rather than opening a public issue.

- Preferred: use [GitHub's private vulnerability reporting](https://github.com/horonomy/libra-governor/security/advisories/new)
  for this repository (Security tab -> Report a vulnerability).
- If that is unavailable to you, open a minimal public issue asking a
  maintainer to reach out for a private channel — do not include
  exploit details or reproduction steps in the public issue itself.

Please include:

- A description of the vulnerability and its potential impact.
- Steps to reproduce, or a proof of concept, if available.
- The version/commit of the repository you tested against.

We aim to acknowledge reports within 5 business days. Coordinated
disclosure is appreciated — please give us reasonable time to investigate
and ship a fix before any public disclosure.

## Supported versions

This project is pre-1.0 and under active bootstrap. Security fixes are
applied to `main` only until a formal release/support policy is
published.

## Secret-handling policy

Libra Governor is a local data plane that, by design (see
[`ARCHITECTURE.md`](ARCHITECTURE.md)), keeps prompt content, source code,
and tool output local by default. Contributors and CI must uphold the
same discipline for credentials:

- **No real credentials, tokens, or connection strings in this
  repository, ever** — not in source, not in test fixtures, not in
  commit messages, not in CI logs.
- Test and fixture data must use clearly fake, non-functional values
  (e.g. `sk-fake-...`, `postgresql://user:pass@localhost/test`) that
  cannot be mistaken for or misused as a real secret.
- Any credential a workflow needs (CI provider tokens, publishing keys,
  etc.) must be supplied via environment variables or a secret store
  (e.g. GitHub Actions encrypted secrets) — never committed to the
  repository.
- This repository has GitHub secret scanning and push protection
  enabled. A blocked push due to a detected secret should be treated as
  a real finding: remove the secret from history and rotate it, rather
  than bypassing the protection.

## Dependency security

Dependency updates (Cargo crates and GitHub Actions) are tracked via
Dependabot (see [`.github/dependabot.yml`](.github/dependabot.yml)) on a
weekly cadence. CI runs `cargo clippy -- -D warnings` and `cargo build`
on every pull request.
