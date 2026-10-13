# Quota-observation extension docs (HORO-1856)

Non-numbered docs (deliberately not ADRs or `docs/extensions/` — that
latter name is taken: ADR-0005's existing loopback-HTTP
business-context/policy/events extension system lives in
`crates/extension` and is a completely different mechanism from the one
documented here). Mirrors the style of the existing
[`docs/quota-source-fixture.md`](../quota-source-fixture.md).

## Status banner — read this before anything else below

> **Experimental, 0.x, preview-only.** This documents
> `crates/quota-observation`, an **observation-only** library crate: it
> never makes an admission, pacing, or enforcement decision, and never
> will under this design — see the crate's own `lib.rs` module docs for
> the authoritative scope statement.
>
> **On `main`.** `crates/quota-observation` (HORO-1854, PR #135) and its
> Phase A conformance kit (HORO-1855, PR #137) are merged. Referenced
> here by PR number for history, not because anything is still pending.
>
> **No CLI exists for this protocol yet.** There is no `register`,
> `approve`, or `observe` command anywhere in this codebase today. Every
> operator-facing step described in these docs that would normally be a
> CLI invocation is instead written against `crates/quota-observation`'s
> own public library API, and is explicitly labelled **"pending
> HORO-1870"** at the point it applies. HORO-1870 owns the CLI slice
> (register/inspect/approve/enable/disable/uninstall) and is now
> unblocked (its dependencies, HORO-1854 and HORO-1855, are merged).

## What's here

| Doc | Covers |
|---|---|
| [`author-guide.md`](author-guide.md) | The one-shot stdio runtime contract, the `describe`/`snapshot` wire operations, and real, fixture-verified CLI examples of driving a sample extension directly (extension-side only — no Libra daemon/CLI involved). |
| [`trust-and-lifecycle.md`](trust-and-lifecycle.md) | Approval digests, what each one binds, the `endpoint_owner`/`consent` fields, pool identity, disable/rollback/uninstall. |
| [`builder-prompt.md`](builder-prompt.md) | A copy-pasteable template (for a human or an LLM) to build a new extension against this contract from scratch. |
| [`preview-delivery.md`](preview-delivery.md) | (HORO-1857) Platform support matrix, download integrity, and the clean-install procedure the smoke test in `crates/quota-observation/tests/preview_clean_install.rs` exercises. |
| [`../../examples/quota-observation-extension/`](../../examples/quota-observation-extension/) | A runnable reference sample extension implementing this contract. |

## Relationship to the design doc and the crate itself

These docs describe the **accepted HORO-1853 design-v2 contract** as
`crates/quota-observation` (HORO-1854) and its conformance kit
(HORO-1855) actually implement it today on `main` — not an aspirational
future version. Where
the crate has a known, disclosed gap against the original design plan
(see `crates/quota-observation/tests/README.md`'s `G1`–`G11` catalog),
these docs say so explicitly rather than describing the gap as closed.
