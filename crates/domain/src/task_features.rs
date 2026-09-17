//! [`TaskFeatures`] — preflight-knowable, structurally scalar features of
//! one task, used by `libra-governor-estimator` to bucket local
//! [`crate::ExecutionReceipt`] history by task class (HORO-1130).
//!
//! # Privacy
//!
//! Every field here is an irreversible derived scalar (a count, a
//! boolean, a length, a hash, or a small closed-set enum) — never raw
//! prompt text, never a raw file path. This extends, rather than
//! weakens, the privacy invariant documented in `crate::lib` docs: none
//! of these fields can be inverted back into the prompt or repository
//! content that produced them. `repo_key` in particular is a truncated
//! SHA-256 digest of the canonicalized repository root path, not the
//! path itself, so it identifies "the same repo, again" without leaking
//! directory structure into the ledger.

use serde::{Deserialize, Serialize};

/// The feature-derivation logic version every produced [`TaskFeatures`]
/// is tagged with. Bump any time a field's derivation changes in a way
/// that would make old and new `TaskFeatures` rows non-comparable for
/// bucketing purposes.
pub const FEATURE_SCHEMA_VERSION: &str = "fs-v1";

/// Preflight-knowable features of one task, derived deterministically
/// from reconnaissance output, the prompt, and the repository root —
/// never from anything only known after execution (no outcome, no
/// actual duration, no post-hoc tool-call count). See the leakage test
/// at the bottom of this file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskFeatures {
    /// SHA-256 of the canonicalized repository root path, hex-truncated.
    /// Identifies "the same repo again" without storing the path itself.
    pub repo_key: String,
    pub topology: BuildTopology,
    pub model: Option<String>,
    pub prompt_char_len: u32,
    pub prompt_line_count: u32,
    pub prompt_code_block_count: u32,
    pub prompt_has_traceback: bool,
    pub prompt_filepath_token_count: u32,
    pub prompt_numeric_token_count: u32,
    pub likely_affected_path_count: u32,
    pub detected_test_command_count: u32,
    pub feature_schema_version: String,
}

/// The dominant build/dependency tooling detected in a repository at
/// reconnaissance time. `Mixed` when more than one is detected, `Unknown`
/// when none is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildTopology {
    Cargo,
    Npm,
    Python,
    Go,
    Ruby,
    Mixed,
    Unknown,
}

/// Which tier of the estimator's hierarchical backoff ladder a produced
/// [`crate::Estimate`] was actually computed from, most to least
/// specific. See `libra-governor-estimator` crate docs for the ladder
/// itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BucketTier {
    RepoTopologyModel,
    RepoTopology,
    Repo,
    Topology,
    Global,
    ColdStart,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> TaskFeatures {
        TaskFeatures {
            repo_key: "abc123".to_string(),
            topology: BuildTopology::Cargo,
            model: Some("claude-sonnet-5".to_string()),
            prompt_char_len: 42,
            prompt_line_count: 3,
            prompt_code_block_count: 1,
            prompt_has_traceback: false,
            prompt_filepath_token_count: 2,
            prompt_numeric_token_count: 1,
            likely_affected_path_count: 4,
            detected_test_command_count: 1,
            feature_schema_version: FEATURE_SCHEMA_VERSION.to_string(),
        }
    }

    /// Leakage test: `TaskFeatures` must only ever carry preflight-knowable
    /// derived scalars. This is enforced structurally (the type has no
    /// field for outcome, actual duration, or a raw tool-call count) but
    /// this test pins the exact field set down so a future field addition
    /// cannot silently reintroduce a leak without a reviewer noticing the
    /// diff here.
    #[test]
    fn task_features_field_set_contains_no_outcome_or_actual_fields() {
        let json = serde_json::to_value(sample()).unwrap();
        let fields: std::collections::BTreeSet<&str> =
            json.as_object().unwrap().keys().map(|k| k.as_str()).collect();

        let expected: std::collections::BTreeSet<&str> = [
            "repo_key",
            "topology",
            "model",
            "prompt_char_len",
            "prompt_line_count",
            "prompt_code_block_count",
            "prompt_has_traceback",
            "prompt_filepath_token_count",
            "prompt_numeric_token_count",
            "likely_affected_path_count",
            "detected_test_command_count",
            "feature_schema_version",
        ]
        .into_iter()
        .collect();

        assert_eq!(fields, expected, "TaskFeatures field set changed — verify no outcome/actual/post-hoc field was added");

        for leaked in [
            "outcome",
            "actual_duration_secs",
            "actual_usage",
            "tool_call_count",
            "prompt",
            "prompt_text",
            "raw_prompt",
            "file_paths",
            "likely_affected_paths",
        ] {
            assert!(
                !fields.contains(leaked),
                "TaskFeatures must never carry a post-hoc or raw-content field: {leaked}"
            );
        }
    }

    #[test]
    fn task_features_round_trips_through_json() {
        let features = sample();
        let json = serde_json::to_string(&features).unwrap();
        let round_tripped: TaskFeatures = serde_json::from_str(&json).unwrap();
        assert_eq!(features, round_tripped);
    }
}
