//! HORO-1855 Phase A — pure wire-shape conformance: framing, schema
//! shape, and numeric negatives. These tests call only
//! `crates/quota-observation`'s public `wire` and `runner` types
//! directly; none of them spawn a subprocess, so none take
//! `common::SUBPROCESS_SERIAL`.
//!
//! See `tests/README.md` for the full P/N/compat matrix this file
//! covers, including gap G3 (duplicate JSON keys on the manual
//! `Reading`/`Unit`/`Window` types), now closed upstream and asserted
//! here rather than merely documented.

mod common;

use libra_governor_quota_observation::runner::{self, FrameError, MAX_RESPONSE_BYTES};
use libra_governor_quota_observation::wire::{
    BootstrapOperation, Coverage, MissingReason, Quantity, QuantityError, Reading,
    SnapshotResponse, Unit, WireValidationError,
};

fn scenario_a_body() -> String {
    std::fs::read_to_string(common::fixture_path("a-response.json"))
        .unwrap()
        .trim()
        .to_string()
}

// ---------------------------------------------------------------------
// N01 — malformed JSON
// ---------------------------------------------------------------------

#[test]
fn n01_malformed_json_is_rejected() {
    let error = runner::parse_single_frame::<SnapshotResponse>(b"not json at all").unwrap_err();
    assert!(matches!(error, FrameError::Invalid(_)));
}

#[test]
fn n01_control_well_formed_json_parses() {
    let body = scenario_a_body().replace("__REQUEST_ID__", "r1");
    let response: SnapshotResponse = runner::parse_single_frame(body.as_bytes()).unwrap();
    assert_eq!(response.request_id, "r1");
}

// ---------------------------------------------------------------------
// N03 — a second stdout frame / trailing bytes after the first value
// ---------------------------------------------------------------------

#[test]
fn n03_second_frame_after_the_first_is_rejected_as_trailing_output() {
    let body = scenario_a_body().replace("__REQUEST_ID__", "r1");
    let two_frames = format!("{body}{body}");
    let error = runner::parse_single_frame::<SnapshotResponse>(two_frames.as_bytes()).unwrap_err();
    assert!(matches!(error, FrameError::TrailingOutput));
}

// ---------------------------------------------------------------------
// CRLF line ending (not in the N-list by number, but exercises the
// same framing boundary as N03/N04): a frame terminated with "\r\n"
// has `\r` left over after the JSON value, which is not the single
// tolerated `\n` — confirmed empirically to produce TrailingOutput,
// not a silent accept.
// ---------------------------------------------------------------------

#[test]
fn crlf_terminated_frame_is_rejected_as_trailing_output_not_accepted() {
    let body = scenario_a_body().replace("__REQUEST_ID__", "r1");
    let crlf = format!("{body}\r\n");
    let error = runner::parse_single_frame::<SnapshotResponse>(crlf.as_bytes()).unwrap_err();
    assert!(matches!(error, FrameError::TrailingOutput));
}

#[test]
fn crlf_control_single_lf_terminated_frame_is_accepted() {
    let body = scenario_a_body().replace("__REQUEST_ID__", "r1");
    let lf = format!("{body}\n");
    let response: SnapshotResponse = runner::parse_single_frame(lf.as_bytes()).unwrap();
    assert_eq!(response.request_id, "r1");
}

// ---------------------------------------------------------------------
// N05 — invalid UTF-8 and a lone (unpaired) UTF-16 surrogate escape
// ---------------------------------------------------------------------

#[test]
fn n05_invalid_utf8_bytes_in_the_frame_are_rejected() {
    let mut body = scenario_a_body()
        .replace("__REQUEST_ID__", "r1")
        .into_bytes();
    // Splice an invalid UTF-8 byte sequence into the middle of the
    // frame — serde_json requires valid UTF-8 input.
    let mid = body.len() / 2;
    body.splice(mid..mid, [0xFF, 0xFE]);
    let error = runner::parse_single_frame::<SnapshotResponse>(&body).unwrap_err();
    assert!(matches!(error, FrameError::Invalid(_)));
}

#[test]
fn n05_lone_surrogate_escape_in_a_json_string_is_rejected() {
    // \uD800 is a lone high surrogate with no matching low surrogate —
    // invalid as a standalone Unicode scalar value. Embedded directly
    // in the authority string of an otherwise well-formed response.
    let body = scenario_a_body()
        .replace("__REQUEST_ID__", "r1")
        .replace("\"authority_a\"", "\"\\uD800\"");
    let error = runner::parse_single_frame::<SnapshotResponse>(body.as_bytes()).unwrap_err();
    assert!(matches!(error, FrameError::Invalid(_)));
}

// ---------------------------------------------------------------------
// N18 — an extra, secret-shaped field the schema does not declare
// ---------------------------------------------------------------------

#[test]
fn n18_extra_secret_like_field_on_the_envelope_is_rejected() {
    let body = scenario_a_body().replace("__REQUEST_ID__", "r1");
    let value: serde_json::Value = serde_json::from_str(&body).unwrap();
    let mut object = value.as_object().unwrap().clone();
    object.insert(
        "api_key".to_string(),
        serde_json::Value::String("not-a-real-secret".to_string()),
    );
    let with_secret = serde_json::to_string(&serde_json::Value::Object(object)).unwrap();
    let error = runner::parse_single_frame::<SnapshotResponse>(with_secret.as_bytes()).unwrap_err();
    assert!(matches!(error, FrameError::Invalid(_)));
}

#[test]
fn n18_control_the_same_envelope_without_the_extra_field_parses() {
    let body = scenario_a_body().replace("__REQUEST_ID__", "r1");
    let response: SnapshotResponse = runner::parse_single_frame(body.as_bytes()).unwrap();
    assert_eq!(response.extension_id, "examplecorp.quota");
}

// ---------------------------------------------------------------------
// N19 — a response that declares both a result and an error
// ---------------------------------------------------------------------

#[test]
fn n19_result_and_error_both_present_is_rejected_by_validate() {
    let body = scenario_a_body().replace("__REQUEST_ID__", "r1");
    let mut response: SnapshotResponse = runner::parse_single_frame(body.as_bytes()).unwrap();
    response.error = Some(libra_governor_quota_observation::wire::WireError {
        code: libra_governor_quota_observation::wire::ErrorCode::Unavailable,
        retry: libra_governor_quota_observation::wire::RetryKind::Never,
        retry_after_seconds: None,
    });
    let requested = vec!["tokens_month".to_string()];
    assert_eq!(
        response.validate(&requested).unwrap_err(),
        WireValidationError::BothResultAndError
    );
}

#[test]
fn n19_control_result_only_validates_cleanly() {
    let body = scenario_a_body().replace("__REQUEST_ID__", "r1");
    let response: SnapshotResponse = runner::parse_single_frame(body.as_bytes()).unwrap();
    let requested = vec!["tokens_month".to_string()];
    assert!(response.validate(&requested).is_ok());
}

// ---------------------------------------------------------------------
// N20 — unknown version / error / unit string
// ---------------------------------------------------------------------

#[test]
fn n20_unknown_bootstrap_operation_is_rejected() {
    let error = serde_json::from_str::<BootstrapOperation>("\"quota.snapshot\"").unwrap_err();
    assert!(!error.to_string().is_empty());
}

#[test]
fn n20_unknown_error_code_is_rejected() {
    let error_envelope = r#"{"protocol_version":"libra.quota-observation/0.1","request_id":"r1","extension_id":"examplecorp.quota","extension_version":"0.1.0","error":{"code":"not_a_real_code","retry":"never"}}"#;
    let error =
        runner::parse_single_frame::<SnapshotResponse>(error_envelope.as_bytes()).unwrap_err();
    assert!(matches!(error, FrameError::Invalid(_)));
}

#[test]
fn n20_unknown_unit_kind_is_rejected() {
    let error = serde_json::from_str::<Unit>(r#"{"kind":"bitcoin"}"#).unwrap_err();
    assert!(error.to_string().contains("unknown variant") || error.to_string().contains("bitcoin"));
}

// ---------------------------------------------------------------------
// N21 — a quantity encoded as a JSON number rather than a string
// ---------------------------------------------------------------------

#[test]
fn n21_quantity_as_json_number_is_rejected() {
    let error = serde_json::from_str::<Quantity>("600000").unwrap_err();
    assert!(error.to_string().contains("string"));
}

#[test]
fn n21_control_quantity_as_json_string_parses() {
    let value: Quantity = serde_json::from_str("\"600000\"").unwrap();
    assert_eq!(value.value(), 600_000);
}

// ---------------------------------------------------------------------
// N22 — leading zero / negative / fraction / exponent / overflow
// ---------------------------------------------------------------------

#[test]
fn n22_quantity_rejects_leading_zero_negative_fraction_exponent_and_overflow() {
    assert_eq!(
        Quantity::parse("0600000").unwrap_err(),
        QuantityError::LeadingZero
    );
    assert_eq!(
        Quantity::parse("-600000").unwrap_err(),
        QuantityError::NotDecimal
    );
    assert_eq!(
        Quantity::parse("600000.5").unwrap_err(),
        QuantityError::NotDecimal
    );
    assert_eq!(
        Quantity::parse("6e5").unwrap_err(),
        QuantityError::NotDecimal
    );
    assert_eq!(
        Quantity::parse("9223372036854775808").unwrap_err(),
        QuantityError::TooLarge
    );
}

// ---------------------------------------------------------------------
// i64::MAX + 1 — the exact overflow boundary, spelled out on its own
// since it is the single most load-bearing numeric boundary in the
// protocol (every wire quantity is i64::MAX-bounded).
// ---------------------------------------------------------------------

#[test]
fn i64_max_plus_one_is_rejected_i64_max_itself_is_accepted() {
    assert_eq!(
        Quantity::parse("9223372036854775808").unwrap_err(),
        QuantityError::TooLarge
    );
    assert_eq!(
        Quantity::parse("9223372036854775807").unwrap().value(),
        i64::MAX
    );
}

// ---------------------------------------------------------------------
// N25 — a `remaining` reading missing its required `limit` field
// ---------------------------------------------------------------------

#[test]
fn n25_remaining_reading_missing_limit_is_rejected() {
    let error =
        serde_json::from_str::<Reading>(r#"{"kind":"remaining","value":"120"}"#).unwrap_err();
    assert!(error.to_string().contains("limit"));
}

#[test]
fn n25_control_remaining_reading_with_limit_parses() {
    let reading: Reading =
        serde_json::from_str(r#"{"kind":"remaining","value":"120","limit":"1000"}"#).unwrap();
    assert_eq!(
        reading,
        Reading::Remaining {
            value: Quantity::parse("120").unwrap(),
            limit: Quantity::parse("1000").unwrap(),
        }
    );
}

// ---------------------------------------------------------------------
// N26 — a `utilization` reading carrying an (unsupported) `limit` field
// ---------------------------------------------------------------------

#[test]
fn n26_utilization_reading_with_a_limit_field_is_rejected() {
    let error = serde_json::from_str::<Reading>(
        r#"{"kind":"utilization","used_basis_points":"10500","limit":"10000"}"#,
    )
    .unwrap_err();
    assert!(error.to_string().contains("unknown field") || error.to_string().contains("limit"));
}

#[test]
fn n26_control_utilization_reading_without_limit_parses_and_preserves_overdraw() {
    let reading: Reading =
        serde_json::from_str(r#"{"kind":"utilization","used_basis_points":"10500"}"#).unwrap();
    match reading {
        Reading::Utilization { used_basis_points } => {
            assert_eq!(used_basis_points.as_str(), "10500");
            assert_eq!(used_basis_points.value(), 10_500);
        }
        _other => panic!("expected Utilization, got a different Reading variant"),
    }
}

// ---------------------------------------------------------------------
// Duplicate-key test on DERIVED structs (always rejected).
// ---------------------------------------------------------------------

#[test]
fn duplicate_key_on_a_derived_struct_is_rejected() {
    // `MissingConstraint` derives Deserialize with deny_unknown_fields;
    // repeating its `id` key exercises the derive's own duplicate-field
    // guard, independent of deny_unknown_fields.
    let error = serde_json::from_str::<libra_governor_quota_observation::wire::MissingConstraint>(
        r#"{"id":"c1","id":"c2","reason":"timeout"}"#,
    )
    .unwrap_err();
    assert!(error.to_string().contains("duplicate field"));
}

#[test]
fn duplicate_key_control_a_derived_struct_with_no_duplicate_parses() {
    let value: libra_governor_quota_observation::wire::MissingConstraint =
        serde_json::from_str(r#"{"id":"c1","reason":"timeout"}"#).unwrap();
    assert_eq!(value.id, "c1");
    assert_eq!(value.reason, MissingReason::Timeout);
}

// ---------------------------------------------------------------------
// Duplicate-key test on the manual types (gap G3, now closed): commit
// `7c6af33` ("Reject duplicate JSON keys on Unit/Window/Reading")
// replaced the `serde_json::Value`-buffering manual `Deserialize` impls
// for `Unit`/`Window`/`Reading` with dispatch through a private,
// `#[serde(deny_unknown_fields)]`-derived flat helper struct per type —
// the derive's own native duplicate-field rejection now applies before
// this module ever sees the value, closing the exact gap this file
// used to document as untested. See `tests/README.md`'s G3 entry.
// ---------------------------------------------------------------------

#[test]
fn duplicate_key_on_unit_a_manual_type_is_now_rejected() {
    let error =
        serde_json::from_str::<Unit>(r#"{"kind":"tokens","kind":"usd_cents"}"#).unwrap_err();
    assert!(error.to_string().contains("duplicate field"));
}

#[test]
fn duplicate_key_on_reading_a_manual_type_is_now_rejected() {
    let error =
        serde_json::from_str::<Reading>(r#"{"kind":"used","value":"1","limit":"2","value":"3"}"#)
            .unwrap_err();
    assert!(error.to_string().contains("duplicate field"));
}

#[test]
fn duplicate_key_control_unit_and_reading_with_no_duplicate_parse() {
    let unit: Unit = serde_json::from_str(r#"{"kind":"tokens"}"#).unwrap();
    assert_eq!(unit, Unit::Tokens);
    let reading: Reading =
        serde_json::from_str(r#"{"kind":"used","value":"1","limit":"2"}"#).unwrap();
    match reading {
        Reading::Used { value, limit } => {
            assert_eq!(value.value(), 1);
            assert_eq!(limit.value(), 2);
        }
        _other => panic!("expected Used, got a different Reading variant"),
    }
}

// ---------------------------------------------------------------------
// Invalid-calendar half of N28 ("wrong date calendar/leapsecond/local
// offset/null"): `observed_at`/`valid_until` are plain `String` fields
// at the wire-shape layer (wire.rs's own doc comment: this module
// performs only wire-shape validation, not freshness semantics). An
// impossible calendar date therefore parses and validates cleanly
// here — rejection only happens later, when crate::freshness tries to
// actually parse it as RFC3339 (`FreshnessError::InvalidTimestamp`).
// This test documents that split rather than asserting a rejection
// this layer was never designed to perform.
// ---------------------------------------------------------------------

#[test]
fn n28_invalid_calendar_date_passes_wire_shape_validation_untouched() {
    let body = scenario_a_body()
        .replace("__REQUEST_ID__", "r1")
        .replace("2027-01-18T12:00:00Z", "2027-02-30T00:00:00Z");
    let response: SnapshotResponse = runner::parse_single_frame(body.as_bytes()).unwrap();
    let requested = vec!["tokens_month".to_string()];
    assert!(response.validate(&requested).is_ok());
    let observed_at = &response.result.unwrap().constraints[0].observed_at;
    assert_eq!(observed_at, "2027-02-30T00:00:00Z");
}

// ---------------------------------------------------------------------
// i64::MAX re-serializes byte-identical, and 9007199254740993
// (2^53 + 1, beyond f64's exact-integer range) re-serializes
// byte-identical too — the scenario-C fixture's own org_credits value.
// ---------------------------------------------------------------------

#[test]
fn large_quantities_round_trip_the_canonical_string_byte_identical() {
    let i64_max = Quantity::parse("9223372036854775807").unwrap();
    assert_eq!(
        serde_json::to_string(&i64_max).unwrap(),
        "\"9223372036854775807\""
    );

    let beyond_f64_precision = Quantity::parse("9007199254740993").unwrap();
    assert_eq!(
        serde_json::to_string(&beyond_f64_precision).unwrap(),
        "\"9007199254740993\""
    );
    assert_eq!(beyond_f64_precision.value(), 9_007_199_254_740_993);
}

// ---------------------------------------------------------------------
// Response size cap sanity check used by conformance_process.rs's N06
// — kept here since it is a pure constant, not a process test.
// ---------------------------------------------------------------------

#[test]
fn response_cap_constant_is_64_kib() {
    assert_eq!(MAX_RESPONSE_BYTES, 64 * 1024);
}

#[test]
fn coverage_and_missing_reason_round_trip_scenario_c_overdraw_and_missing() {
    let body = std::fs::read_to_string(common::fixture_path("c-response.json"))
        .unwrap()
        .replace("__REQUEST_ID__", "r5");
    let response: SnapshotResponse = runner::parse_single_frame(body.as_bytes()).unwrap();
    let result = response.result.unwrap();
    assert_eq!(result.coverage, Coverage::Partial);
    assert_eq!(result.missing.len(), 1);
    assert_eq!(result.missing[0].reason, MissingReason::Unavailable);
    let project_budget = result
        .constraints
        .iter()
        .find(|c| c.id == "project_budget")
        .unwrap();
    match &project_budget.reading {
        Reading::Used { value, limit } => {
            assert_eq!(value.as_str(), "10500");
            assert_eq!(limit.as_str(), "10000");
            assert!(
                value.value() > limit.value(),
                "overdraw must be preserved, not clamped"
            );
        }
        _other => panic!("expected Used reading, got a different Reading variant"),
    }
}
