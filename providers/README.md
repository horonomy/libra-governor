# First-Class Extension Providers (HORO-1173)

This directory contains first-class, production-usable implementations of
the Business Context / Outcome Provider roles defined by
[`docs/api/libra-extension-v1.yaml`](../docs/api/libra-extension-v1.yaml)
and [ADR 0005](../docs/adr/0005-local-extension-points.md). See
[ADR 0006](../docs/adr/0006-first-class-providers-reuse-local-extension-points.md)
for why these exist as loopback processes against the existing contract
rather than as new daemon capability.

`examples/local-providers/libra_example_provider.py` (HORO-1174) remains
the minimal, illustrative reference implementation. It now builds on
`providers/common/` too (refactored to eliminate real duplication a
SonarCloud quality gate caught between its original HMAC/ticket-key
logic and this work's extraction of the same logic) — its behavior on
the wire is unchanged, only its implementation now shares one copy of
that logic instead of keeping a second. The adapters here are real,
usable providers backed by a real external system.

## Layout

```
providers/
  common/
    libra_provider_runtime.py   # shared HMAC/replay/loopback server scaffolding
    safe_https_client.py        # SSRF-safe outbound HTTPS client
  jira/
    libra_jira_provider.py      # Jira Cloud Business Context + Outcome adapter
  github/
    libra_github_provider.py    # GitHub Business Context + Outcome adapter (read-only)
```

Every module is stdlib-only Python 3, matching this repository's existing
Python tooling convention. Every outbound network call goes through
`safe_https_client.fetch_json`/`post_json`, which accept an injectable
`opener` — no test file in this directory makes a real network call.

## Building a custom adapter without forking core

A future enterprise adapter (Linear, Asana, ServiceNow, an internal
tracker) needs only:

1. Read `docs/api/libra-extension-v1.yaml` for the exact
   `BusinessContextRequest`/`BusinessContextResponse`/`ExternalRef`
   schemas the daemon sends and expects.
2. Subclass `libra_provider_runtime.BaseProviderHandler`, override
   `route_table()` to wire `/libra/business-context` (and, if the
   provider also receives event delivery, `/libra/events`).
3. Use `safe_https_client.validate_https_base_url` once at startup on the
   provider's own configured base URL, then `fetch_json`/`post_json` for
   every real outbound call.
4. Call `libra_provider_runtime.run_server(...)` with a loopback host.

No Rust code, no protocol change, no daemon rebuild. This is exactly what
`libra_jira_provider.py` and `libra_github_provider.py` each do.

## Jira adapter (`providers/jira/libra_jira_provider.py`)

**Ticket resolution:** the real git branch name at the operator-supplied
`--workspace-root`, matched against the request's `cwd` as an equality
selector only (see ADR 0006 §4) — never a request-supplied ticket key.

**Business Context mapping** (`business_context_from_issue`):

| Wire field | Source | Notes |
|---|---|---|
| `priority` | `fields.priority.name`, mapped via `JIRA_PRIORITY_TO_WIRE_PRIORITY` | Unmapped priority names are omitted, never guessed |
| `deadline` | `fields.duedate`, rendered as end-of-day UTC RFC3339 | Omitted if the issue has no due date |
| `cost_center` | `fields.project.key` | |
| `advisory_criteria` | always `[]` | This adapter never parses issue text into completion criteria (R2) |
| `external_refs` | `[{"kind": "jira", "value": issue["self"]}]` | |

**Outcome push** (`push_outcome`): calls `libra-governor outcome record`
(the existing HORO-1174 CLI path) with `source_id: "jira"` — no new
daemon protocol surface.

**Write-back** (`post_execution_receipt_comment`): posts one comment to
the Jira issue recording the outcome. Off by default; enabled only with
`--enable-write-back`. This is the adapter's only mutating call.

**OAuth/token scopes:**
- `read:jira-work` always required.
- `write:jira-work` required only when `--enable-write-back` is passed.

## GitHub adapter (`providers/github/libra_github_provider.py`)

**Repo/branch resolution:** the real `git remote get-url origin` and
`git rev-parse --abbrev-ref HEAD` at `--workspace-root` — never
request-derived (ADR 0006 §4).

**Business Context mapping** (`business_context_from_pull_request`):
`cost_center` <- the PR's base repository `full_name`; `external_refs`
<- the PR's own `html_url` with `kind: "github"`.

**Outcome mapping** (`outcome_from_pull_request_and_checks`): objective
PR/check state only, never the PR's title or body text (which could
carry an unverified claim of success) —

| Condition | `ExecutionOutcome` |
|---|---|
| `merged: true` | `Completed` |
| `state: closed`, not merged | `Aborted` |
| combined check status `failure` | `Failed` |
| otherwise (open, pending) | no outcome yet |

**Read-only by design:** this adapter never mutates a PR, issue, or check
run — there is no `--enable-write-back` flag.

**OAuth/token scopes:** `pull_requests: read`, `checks: read`. No
`contents` or organization-level scope is required.

## Running the tests

```bash
python3 providers/common/test_safe_https_client.py
python3 providers/common/test_libra_provider_runtime.py
python3 providers/jira/test_libra_jira_provider.py
python3 providers/github/test_libra_github_provider.py
```

(`unittest discover` across all four directories at once is not used here
because each test file manipulates `sys.path` itself to import its
sibling module under test — see each file's own path-insertion lines.)
