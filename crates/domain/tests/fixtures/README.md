# Vendored execution-identity conformance vectors

`execution-identity-conformance-vectors.json` is a byte-for-byte copy of the
Host Integration conformance vectors. Never edit it by hand.

| | |
|---|---|
| Source | `horonomy/.github`, `governance/product/execution-identity-conformance-vectors.json` |
| Pinned ref | `97872f547c1a5d0df351b5b62dcc57648f506578` (PR #110, on top of #109 `b959b537`) |
| `vectors_version` | `1` |
| sha256 | `c6ab9b978540e723a4d338f40b5218f6e355667c35dec456e9b7a650233b5a13` |
| Contract | `governance/product/execution-identity-contract.md` at the same ref |

Consumers: `crates/domain/tests/execution_identity_conformance.rs` (envelope,
cache keys, display ids, correlation) and `crates/cli/src/agent/identity_conformance.rs`
(hook payload to identity). Both pin the sha256 above and fail on drift.

## Update procedure

1. Pick the new `horonomy/.github` commit SHA.
2. `gh api -H "Accept: application/vnd.github.raw" "repos/horonomy/.github/contents/governance/product/execution-identity-conformance-vectors.json?ref=<SHA>" > crates/domain/tests/fixtures/execution-identity-conformance-vectors.json`
3. `shasum -a 256` the file; update the ref, sha256 and `vectors_version` in this
   README and the `PINNED_SHA256` constants in both runners.
4. Run both runners. A failing vector is a Libra defect to fix (or, if the
   contract changed, a documented follow-up); never weaken a vector.

## Known deviation

One normalizer vector is a documented, tested deviation rather than a match:
`claude-not-a-mapping` (payload `["s-1"]`). The reference emits an envelope
with every identity field absent; Libra refuses the whole hook event, because a
non-object names no field and derived struct decoders would otherwise read the
array positionally (`["s-1"]` becoming session `s-1`). The vector's actual
guarantee, that no session is ever attributed from a non-mapping, holds and is
asserted for every entry point (`JUSTIFIED_DEVIATIONS` in
`crates/cli/src/agent/identity_conformance.rs`). The other nine normalizer
vectors match exactly; since HORO-2197 that includes `claude-no-session`,
`claude-non-string-session` and `codex-heuristics-only`.
