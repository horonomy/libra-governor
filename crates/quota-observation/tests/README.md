# HORO-1855 conformance kit — Phase A matrix

This is the conformance matrix for `crates/quota-observation`'s Phase A
black-box integration test suite, mapping every ID from the accepted
HORO-1853 design-v2 contract's conformance plan (`P01`–`P10`,
`N01`–`N57`, and the 7 compatibility-matrix rows) to either a test
function in this directory, a disclosed gap (`G<n>`), or an explicit
out-of-scope reason.

**Source of the ID catalog**: the positive/negative fixture plan and
compatibility matrix are `design-v1--conformance-and-dx.md` (HORO-1853
attachment 10165 — unchanged by the design-v2 schema/example fixes,
since those only touched schemas/examples/prose, not the test-ID
catalog itself). Retrieved this session via the Jira REST attachment
API (same authenticated path HORO-1853's own acceptance comment
describes using).

**On gap numbering**: this session could not recover an authoritative
`G1`–`G11` catalog from the original opus-architect design pass — it
is not in any Jira issue body, PR #135's description, or session
memory for HORO-1854/1855. The one gap number given explicitly in this
ticket's own implementation instructions (`G3`, the duplicate-key
issue below) is preserved exactly as given. Every other gap found
while writing these tests is numbered `G4` onward to avoid colliding
with that anchor. **`G1` and `G2` are reserved but undefined here** —
reconcile against the original design pass if its text becomes
available, rather than assuming these numbers are unused.

## Phase A vs Phase B

Phase A (this kit): black-box integration tests that pass against
`crates/quota-observation` exactly as merged in HORO-1854 (PR #135).
Phase B (explicitly deferred): tests for gaps `G1`–`G3` specifically
are called out in the HORO-1855 ticket as waiting on HORO-1854 PR #135
follow-up fixes; the remaining gaps below (`G4`–`G11`) are new findings
from writing this kit and have no scheduled follow-up ticket yet.

**2026-10-12 update**: this branch was rebased onto 10 new commits
pushed to HORO-1854's branch after an adversarial security review of
PR #135 (not yet merged to `main` at rebase time). That review fixed
**G3** and **G6** outright (see their entries below and N02/N33/N34
above) rather than deferring them further — both are now closed, with
Phase A tests added for the newly-correct behavior instead of the
previously-documented gap. `G1`/`G2`/`G4`/`G5`/`G7`–`G11` are unaffected
by that review and remain open exactly as before.

## P01–P10 (positive fixtures)

| ID | Result | Covered by |
|---|---|---|
| P01 | Static describe, exact common version, no auth/source network | `conformance_process.rs::describe_through_invoke_parses_as_a_bootstrap_response` |
| P02 | Scenario A exact used/limit | `conformance_semantic.rs::p02_scenario_a_exact_used_and_limit_preserved` |
| P03 | Scenario B separate units/windows, no conversion | `conformance_semantic.rs::p03_scenario_b_separate_units_and_windows_no_conversion_performed` |
| P04 | Scenario C large integer + overdraw + partial/missing | `conformance_semantic.rs::p04_scenario_c_large_integer_and_overdraw_survive_binding_partial_coverage` |
| P05 | Undisclosed reading, complete coverage possible | `conformance_semantic.rs::p05_undisclosed_reading_still_yields_complete_coverage` — evaluator-Indeterminate half out of scope (no evaluator in this crate) |
| P06 | Utilization >100% preserved, never clamped | `conformance_semantic.rs::p06_utilization_over_100_percent_preserved_not_clamped` — evaluator-Blocking half out of scope |
| P07 | Original source time preserved, host max-age applied | `conformance_semantic.rs::p07_n46_original_source_time_preserved_host_applies_max_age_not_acquisition_time` |
| P08 | New epoch accepted only if coherent; old values never reused | out of scope: this crate has no cache/reuse mechanism at all (one-shot transport, nothing persisted between calls), so "old values reused" cannot occur by construction. Partially illustrated by `shared_pool_anti_vacuity.rs::attack6_host_environment_never_reaches_the_child_and_the_captured_request_carries_no_identity_fields` (two different epochs, independently evaluated) |
| P09 | Zero/zero reading, not divide-by-zero or healthy | `conformance_semantic.rs::p09_zero_used_zero_limit_does_not_panic_and_is_preserved` — evaluator-Blocking half out of scope |
| P10 | All source failures missing, partial, no empty-healthy | `conformance_semantic.rs::p10_all_requested_ids_missing_is_partial_with_no_constraints`; subprocess-level repeat in `shared_pool_anti_vacuity.rs::attack7_all_missing_is_partial_with_zero_constraints` |

## N01–N57 (negative/control fixtures)

| ID | Description | Covered by / disposition |
|---|---|---|
| N01 | Malformed JSON | `conformance_wire.rs::n01_malformed_json_is_rejected` (+ control) |
| N02 | Duplicate keys | Derived structs: `conformance_wire.rs::duplicate_key_on_a_derived_struct_is_rejected` (+ control). Manual types (`Reading`/`Unit`/`Window`): **gap G3 closed** (commit `7c6af33` on the rebased HORO-1854 branch) — `conformance_wire.rs::duplicate_key_on_unit_a_manual_type_is_now_rejected`, `duplicate_key_on_reading_a_manual_type_is_now_rejected` (+ control) |
| N03 | Second stdout frame / trailing bytes | `conformance_wire.rs::n03_second_frame_after_the_first_is_rejected_as_trailing_output` |
| N04 | No LF / CRLF / BOM | CRLF: `conformance_wire.rs::crlf_terminated_frame_is_rejected_as_trailing_output_not_accepted` (+ control, single LF). No-LF: not a negative case — `parse_single_frame` accepts a frame with no trailing LF by design (confirmed in `runner.rs`'s own unit tests), so no test asserts rejection. BOM: untested — **gap G10** |
| N05 | Invalid Unicode / lone surrogate | `conformance_wire.rs::n05_invalid_utf8_bytes_in_the_frame_are_rejected`, `n05_lone_surrogate_escape_in_a_json_string_is_rejected` |
| N06 | 65537-byte response | `conformance_process.rs::n06_oversized_response_is_rejected` (+ control) |
| N07 | Nesting deeper than 12 | **gap G4** — no nesting-depth cap exists anywhere in `wire.rs` |
| N08 | More than 64 items | **gap G5** — no array/collection-length cap exists (e.g. `constraint_ids`, `capabilities`) |
| N09 | stderr flood | `conformance_process.rs::n09_stderr_flood_does_not_abort_a_successful_invocation` — documented divergence: stderr is discarded (`Stdio::null()`), so "excess aborts" does not apply here; acceptable since nothing downstream reads stderr |
| N10 | Timeout/hang | `conformance_process.rs::n10_n12_n13_frame_then_hold_times_out_within_its_short_deadline` (+ control); subprocess-level repeat in `shared_pool_anti_vacuity.rs::attack7_valid_frame_while_grandchild_holds_stdout_times_out_with_no_bound_result` |
| N11 | Valid frame then non-zero exit | `conformance_process.rs::n11_valid_frame_then_nonzero_exit_is_a_failure` (+ control); repeat via `observe()` in `shared_pool_anti_vacuity.rs::attack7_valid_frame_plus_nonzero_exit_yields_no_bound_result` |
| N12 | Valid frame without EOF | Same test as N10 (`n10_n12_n13_frame_then_hold_times_out_within_its_short_deadline`) — a backgrounded grandchild holding stdout open is exactly "no EOF" |
| N13 | Descendant pipe retained | Same test as N10/N12 |
| N14 | Cancel/disabling during collection | `conformance_process.rs::n14_cancellation_flag_stops_a_hanging_invocation_promptly` (+ control) |
| N15 | Executable changed between describe/snapshot | `conformance_process.rs::n15_executable_changed_between_describe_and_snapshot_invalidates_trust` (+ control) — also covers compat row 6 |
| N16 | Unrelated FD leakage | Out of scope for now, per instruction: unverified, noted as future work |
| N17 | Config loader env injection rejected | `conformance_process.rs::n17_env_injected_in_the_host_process_never_reaches_the_child` |
| N18 | Extra secret-like field | `conformance_wire.rs::n18_extra_secret_like_field_on_the_envelope_is_rejected` (+ control) |
| N19 | Result + error both present | `conformance_wire.rs::n19_result_and_error_both_present_is_rejected_by_validate` (+ control) |
| N20 | Unknown version/error/unit | `conformance_wire.rs::n20_unknown_bootstrap_operation_is_rejected`, `n20_unknown_error_code_is_rejected`, `n20_unknown_unit_kind_is_rejected` |
| N21 | Quantity as JSON number | `conformance_wire.rs::n21_quantity_as_json_number_is_rejected` (+ control) |
| N22 | Leading zero/negative/fraction/exponent/overflow | `conformance_wire.rs::n22_quantity_rejects_leading_zero_negative_fraction_exponent_and_overflow`; the i64::MAX boundary specifically: `i64_max_plus_one_is_rejected_i64_max_itself_is_accepted` |
| N23 | Wrong metric/unit | **gap G8** (part 1) — no cross-check between `metric` and `unit` exists |
| N24 | remaining > limit | **gap G8** (part 2) — no cross-check between a `remaining` reading's `value` and `limit` ordering exists |
| N25 | Remaining missing limit | `conformance_wire.rs::n25_remaining_reading_missing_limit_is_rejected` (+ control) |
| N26 | Utilization with a limit field | `conformance_wire.rs::n26_utilization_reading_with_a_limit_field_is_rejected` (+ control, also proves overdraw-shaped basis points preserved) |
| N27 | Raw proprietary string fields outside alias constraints | **gap G11** — `provider`/`window_uuid`/similar alias strings have no format or allowlist validation; currently accepted as opaque strings by design |
| N28 | Wrong date calendar/leapsecond/local offset/null | Invalid-calendar half: `conformance_wire.rs::n28_invalid_calendar_date_passes_wire_shape_validation_untouched` (documents that wire-shape validation never parses dates — rejection, if any, happens later in `freshness::is_fresh`). Leapsecond/local-offset/null halves: not assigned to any file in this kit, untested |
| N29 | declared_reset_at <= observed_at | **gap G7** — no cross-check exists |
| N30 | Future source timestamp | `shared_pool_anti_vacuity.rs::attack7_stale_elapsed_and_future_readings_stay_present_but_not_fresh` (`observed_at_more_than_5s_future` scenario) |
| N31 | Stale/expired validity | `conformance_semantic.rs::n31_stale_past_max_staleness_is_not_fresh` (+ control) |
| N32 | valid_until already elapsed | `conformance_semantic.rs::n32_valid_until_already_elapsed_is_not_fresh_even_within_max_staleness` (+ control) |
| N33 | Wrong context selector | **gap G6 closed** (commit `7400315` on the rebased HORO-1854 branch, B3) — `conformance_semantic.rs::n33_wrong_context_selector_rejects_the_whole_response` (+ control `n33_n34_control_matching_context_and_authority_is_accepted`) |
| N34 | Wrong principal/authority/scope handle | **gap G6 closed**, same commit — whole-response half: `conformance_semantic.rs::n34_wrong_authority_rejects_the_whole_response`, `n34_wrong_principal_handle_rejects_the_whole_response`; per-constraint scope-handle half: `conformance_semantic.rs::scope_mismatch_is_rejected_into_missing_not_silently_accepted` (+ `freshness.rs`'s own unit tests `scope_handle_mismatch_is_rejected_into_missing`, `principal_handle_mismatch_on_a_principal_scoped_constraint_is_rejected`) |
| N35 | Epoch changes during collection -> whole-context error | `conformance_semantic.rs::n35_context_changed_error_returns_no_result` — surfaces via the ordinary error envelope (`ErrorCode::ContextChanged`), no separate "whole context" path exists or is needed |
| N36 | Unrequested constraint | `conformance_semantic.rs::n36_unrequested_constraint_id_is_rejected` |
| N37 | Duplicate ids across success/missing | `conformance_semantic.rs::n37_duplicate_id_across_constraints_and_missing_is_rejected` |
| N38 | Missing required id without reason | `conformance_semantic.rs::n38_requested_id_in_neither_constraints_nor_missing_is_rejected` |
| N39 | False complete with missing | `conformance_semantic.rs::n39_false_complete_with_nonempty_missing_is_rejected` |
| N40 | Partial without missing | `conformance_semantic.rs::n40_partial_with_empty_missing_is_rejected` |
| N41 | Same owner bound to inconsistent UUID | Out of scope: obsolete under design-v2, replaced by `shared_pool_anti_vacuity.rs` items 1–2 (`attack1_*`, `attack2_*`) |
| N42 | Rebound identity with reused old UUID | Out of scope: same as N41 |
| N43 | Owner name in ID not eligible for log | Out of scope: belongs to the HORO-1870 (CLI slice) |
| N44 | Empty configured expected set | `conformance_semantic.rs::n44_empty_configured_expected_set_is_rejected_at_load_time` (+ control) |
| N45 | Context reset, no old-healthy fallback | Out of scope: no caching layer exists in this one-shot crate; the property holds vacuously by construction (there is nothing to fall back to) |
| N46 | Current acquisition time substituted for old source | Same test as P07: `conformance_semantic.rs::p07_n46_original_source_time_preserved_host_applies_max_age_not_acquisition_time` |
| N47 | Wall clock rollback extends lifetime | Out of scope per instruction (monotonic `r_end`) |
| N48 | context_valid_until earlier than reading validity | `conformance_semantic.rs::n48_context_valid_until_earlier_than_reading_valid_until_still_expires_it` (+ control) |
| N49 | Passed reset, fabricated refill | Out of scope per instruction (evaluator-related) |
| N50 | Source soft limit retyped as hard permission | `conformance_semantic.rs::n50_soft_upstream_enforcement_is_never_retyped_as_hard` |
| N51 | Synthetic secret in stderr/upstream error/raw payload doesn't reach diagnostics | Out of scope: belongs to HORO-1870 (CLI slice) |
| N52 | Machine-readable local quota output not auto-uploaded | Out of scope per instruction (CLI/doctor, HORO-1870) |
| N53 | Doctor passive mode, no process | Out of scope per instruction (same as N52) |
| N54 | Doctor explicit probe, describe-only, no network | Out of scope per instruction (same as N52) |
| N55 | Known Blocking survives other unavailable | Out of scope per instruction (evaluator-related, with N49) |
| N56 | No gateway/daemon/ledger mutation | Out of scope per instruction: workspace guards, covered elsewhere |
| N57 | Existing quota replay guard still passes | Out of scope per instruction: same as N56 |

## Compatibility matrix

| Row | Host offered | Extension supports | Expected | Covered by / disposition |
|---|---|---|---|---|
| 1 | 1.0 | 1.0 | Select 1.0 | Out of scope: only one protocol version (`libra.quota-observation/0.1`) is implemented today; multi-version negotiation is a future-versions scenario per the design doc itself ("future versions hypothetical, not shipped") |
| 2 | 1.0 | 1.1 only | Incompatible, no snapshot | Out of scope: same as row 1 |
| 3 | 1.0+1.1 (future) | 1.0 | Select 1.0, old fixtures retained | Out of scope: same as row 1 |
| 4 | 1.0+1.1 (future) | 1.0+1.1 | Select 1.1 only if both implement its schema | Out of scope: same as row 1 |
| 5 | 1.0 | Bootstrap unknown | Incompatible/malformed, no permissive fallback | Partially covered: `conformance_wire.rs::n20_unknown_bootstrap_operation_is_rejected` demonstrates the malformed half |
| 6 | 1.0 | 1.0 describe / changed package snapshot | Identity change, invalidate | `conformance_process.rs::n15_executable_changed_between_describe_and_snapshot_invalidates_trust` |
| 7 | 1.0 | quota.snapshot missing | Unsupported capability | **gap G9** — `BootstrapResult.capabilities` is never cross-checked against what a snapshot call actually requires; nothing in this crate reads that field back |

## Disclosed gaps (G1–G11)

| Gap | Description | Status |
|---|---|---|
| G1 | *Reserved — undefined in this session; see note above* | — |
| G2 | *Reserved — undefined in this session; see note above* | — |
| G3 | `Reading`/`Unit`/`Window`'s manual `Deserialize` impls materialize through `serde_json::Value` first, which silently keeps only the last of a duplicate key — unlike derive-generated structs, which reject a duplicate key unconditionally (serde's standard `visit_map` codegen) | **Closed** by commit `7c6af33` on the rebased HORO-1854 branch (PR #135 follow-up): the manual impls now dispatch through a private `#[serde(deny_unknown_fields)]`-derived flat helper struct per type, whose native duplicate-field rejection applies before this module sees the value. See N02 above |
| G4 | No JSON nesting-depth cap (N07) | New finding this session; no follow-up ticket yet |
| G5 | No array/collection-length cap, e.g. `constraint_ids`/`capabilities` (N08) | New finding this session; no follow-up ticket yet |
| G6 | `verify_response_identifiers` never compares a response's `context`/scope-handle fields against the registration's own (N33/N34) | **Closed** by commit `7400315` on the rebased HORO-1854 branch (B3): `apply_account_binding` now calls `verify_context_binding` (whole-response context/authority check) before any per-constraint binding, and the per-constraint path additionally checks `unit`/`metric`/`scope`, not just `account_fingerprint`. See N33/N34 above |
| G7 | No cross-check that `window.declared_reset_at` is after `observed_at` (N29) | New finding this session; no follow-up ticket yet |
| G8 | No cross-check between `metric`/`unit` compatibility (N23), nor between a `remaining` reading's `value` and `limit` ordering (N24) | New finding this session; no follow-up ticket yet |
| G9 | `BootstrapResult.capabilities` is declared but never cross-checked against what a snapshot call requires (compat row 7) | New finding this session; no follow-up ticket yet |
| G10 | N04's BOM sub-case is untested; CRLF is covered and no-LF is confirmed-accepted-by-design | New finding this session; no follow-up ticket yet |
| G11 | `provider`/`window_uuid`/similar alias strings have no format or allowlist validation (N27); accepted as opaque strings by design today | New finding this session; no follow-up ticket yet |

## Founder-mandated shared-pool anti-vacuity attacks

Tracked in `shared_pool_anti_vacuity.rs`, not part of the P/N numbering
(added by the 2026-10-12 "shared-pool conformance" implementation
clarification on HORO-1855, after the original P/N catalog was
written):

1. `attack1_eight_registrations_sharing_one_pool_identity_collide` (+ 2 controls)
2. `attack2_field_isolation_table` (+ exhaustiveness guard + 2 controls)
3. `attack3_eight_concurrent_sessions_are_all_correct_and_never_corrupt_each_others_frames` (concurrency correctness; the "never 500x8 pools" multi-registration property is attack1's, not this one — see its own section comment), `attack3_anti_replay_control_one_sessions_frame_is_rejected_as_another_sessions_response`
4. `attack4_disabled_duplicate_loads_alongside_the_enabled_original`, `attack4_invoke_refuses_the_disabled_copy_and_flipping_enabled_in_memory_is_refused_at_the_manifest_gate` (does not by itself prove the manifest digest covers `enabled` — see `identity.rs`'s own `flipping_enabled_without_reapproval_trips_the_manifest_digest` unit test for that property)
5. `attack5_wrong_account_fingerprint_is_rehomed_to_missing_and_never_echoed_in_the_request`
6. `attack6_host_environment_never_reaches_the_child_and_the_captured_request_carries_no_identity_fields`
7. `attack7_every_error_code_yields_extension_error_and_no_bound_result`, `attack7_all_missing_is_partial_with_zero_constraints`, `attack7_stale_elapsed_and_future_readings_stay_present_but_not_fresh`, `attack7_valid_frame_plus_nonzero_exit_yields_no_bound_result`, `attack7_valid_frame_while_grandchild_holds_stdout_times_out_with_no_bound_result`

## Fixture inventory

| Path | Purpose |
|---|---|
| `common/mod.rs` | Shared harness: `approve`, `sample_registration`, `observe`, process-serialization lock, fixed clock |
| `fixtures/fictional-ext/quota-ext.sh` | The fictional POSIX-sh extension every test drives |
| `fixtures/scenarios/{a,b,c}-response.json` | The design-v2 scenario A/B/C snapshot responses, `request_id` replaced with `__REQUEST_ID__` |
| `fixtures/scenarios/auth-required-response.json` | A minimal `auth_required` error envelope |
| `fixtures/scenarios/describe-response.json` | A minimal successful bootstrap/describe response |

Every other response template used by a test (error-code sweep,
oversized response, wrong-account fixture, freshness-boundary
scenarios, etc.) is generated inline by the test that needs it, since
committing one file per such case would multiply the fixture count
without adding reusable value — each is a small, single-purpose string
literal next to the assertion it supports.
