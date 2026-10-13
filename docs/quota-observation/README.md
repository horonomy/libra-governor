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
> **The operator CLI exists.** HORO-1870 shipped it, merged to `main`:
> `libra-governor quota-observation <register|approve|enable|disable|
> uninstall|doctor|inspect>`. `doctor` is fully passive (lists
> registrations from disk, no extension process ever starts);
> `doctor --probe` performs a `describe`-only bootstrap round trip per
> enabled+approved registration; `inspect` is the one command that
> performs a live, explicitly consented `snapshot` round trip and the
> only one that can report quota availability/coverage. See
> [`author-guide.md`](author-guide.md)'s "Operator CLI" section for the
> full command reference with real, verified examples.

## What's here

| Doc | Covers |
|---|---|
| [`author-guide.md`](author-guide.md) | The one-shot stdio runtime contract, the `describe`/`snapshot` wire operations, real fixture-verified examples of driving a sample extension directly (extension-side only — no Libra daemon/CLI involved), and the real `libra-governor quota-observation` operator CLI reference. |
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
