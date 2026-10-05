use libra_governor_protocol::host_event::{
    validate_bounded_json, validate_host_json, HostBindingReason,
};

#[test]
fn profiles_apply_independent_byte_depth_and_node_limits() {
    let input = br#"{"a":[1,2]}"#;
    assert!(validate_bounded_json(input, input.len(), 2, 16).is_ok());
    assert_eq!(
        validate_bounded_json(input, input.len() - 1, 2, 16)
            .unwrap_err()
            .reason,
        HostBindingReason::InputTooLarge
    );
    assert_eq!(
        validate_bounded_json(input, input.len(), 1, 16)
            .unwrap_err()
            .reason,
        HostBindingReason::InputTooDeep
    );
    assert_eq!(
        validate_bounded_json(input, input.len(), 2, 1)
            .unwrap_err()
            .reason,
        HostBindingReason::TooManyNodes
    );
}

#[test]
fn all_profiles_reject_duplicate_keys_and_extra_documents() {
    for input in [
        br#"{"outer":{"same":1,"same":2}}"#.as_slice(),
        br#"{"outer":1} {"second":2}"#.as_slice(),
        &[0xff],
    ] {
        assert!(validate_bounded_json(input, 4096, 32, 4096).is_err());
        assert!(validate_host_json(input).is_err());
    }
    assert_eq!(
        validate_bounded_json(br#"{"a":{"x":1,"x":2}}"#, 4096, 32, 4096)
            .unwrap_err()
            .reason,
        HostBindingReason::DuplicateJsonKey
    );
}

#[test]
fn wider_protocol_profile_does_not_weaken_original_host_limits() {
    let input = serde_json::to_vec(&serde_json::json!({"large":"a".repeat(70_000)})).unwrap();
    assert_eq!(
        validate_host_json(&input).unwrap_err().reason,
        HostBindingReason::InputTooLarge
    );
    assert!(validate_bounded_json(&input, 1_048_576, 32, 65_536).is_ok());
    let input = format!("{}0{}", "[".repeat(17), "]".repeat(17));
    assert_eq!(
        validate_host_json(input.as_bytes()).unwrap_err().reason,
        HostBindingReason::InputTooDeep
    );
    assert!(validate_bounded_json(input.as_bytes(), 65_536, 32, 4096).is_ok());
}
