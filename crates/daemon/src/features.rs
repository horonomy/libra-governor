//! Derives [`TaskFeatures`] (HORO-1130) from bounded reconnaissance
//! output, the prompt, and the repository root — the preflight-knowable
//! input `libra-governor-estimator::estimate_bucketed` buckets local
//! history by.
//!
//! Every derived field is an irreversible scalar (count, boolean,
//! length, hash) computed with plain string/byte operations — no regex
//! dependency, matching [`crate::recon`]'s own no-external-dependency
//! tokenization style, and no raw prompt text or file path is ever
//! carried forward. See `libra_governor_domain::TaskFeatures`'s module
//! docs for the structural privacy guarantee this rests on.

use std::path::Path;

use libra_governor_domain::{TaskFeatures, FEATURE_SCHEMA_VERSION};
use sha2::{Digest, Sha256};

use crate::recon::ReconOutput;

/// File extensions treated as evidence of a filepath-shaped token,
/// alongside any token containing a path separator (`/`).
const COMMON_SOURCE_EXTENSIONS: &[&str] = &[
    "rs", "py", "js", "ts", "tsx", "jsx", "go", "rb", "java", "c", "cc", "cpp", "h", "hpp", "toml",
    "yaml", "yml", "json", "md", "sql", "sh", "cfg", "ini", "lock",
];

/// SHA-256 of the canonicalized repository root path, hex-truncated to
/// 16 hex characters (64 bits). Falls back to hashing the given `root`
/// as-is when canonicalization fails (e.g. the path does not exist,
/// which is a real possibility for a hand-constructed test fixture or a
/// preflight run against a not-yet-created directory) — a truncated
/// digest of a non-canonical path is still a stable, deterministic key
/// for "this exact root string", just not resilient to path aliasing
/// (symlinks, `..`) in that fallback case.
pub fn repo_key(root: &Path) -> String {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    hex[..16].to_string()
}

/// Derives [`TaskFeatures`] for one preflight request. `model` is
/// `None` at preflight time in practice today — `Request::Preflight`
/// carries no model field (only `Request::Finalize` does, since the
/// harness's hook payload only exposes it at `Stop` time) — but the
/// parameter exists so a future preflight payload that does expose it
/// does not require a signature change here.
pub fn derive_task_features(
    recon: &ReconOutput,
    prompt: &str,
    root: &Path,
    model: Option<&str>,
) -> TaskFeatures {
    TaskFeatures {
        repo_key: repo_key(root),
        topology: recon.build_topology(),
        model: model.map(str::to_string),
        prompt_char_len: prompt.chars().count() as u32,
        prompt_line_count: prompt.lines().count() as u32,
        prompt_code_block_count: count_code_blocks(prompt),
        prompt_has_traceback: has_traceback(prompt),
        prompt_filepath_token_count: count_filepath_tokens(prompt),
        prompt_numeric_token_count: count_numeric_tokens(prompt),
        likely_affected_path_count: recon.likely_affected_paths.len() as u32,
        detected_test_command_count: recon.detected_test_commands.len() as u32,
        feature_schema_version: FEATURE_SCHEMA_VERSION.to_string(),
    }
}

/// Counts complete fenced Markdown code blocks (paired ` ``` ` markers).
/// An unpaired trailing fence is not counted as a block.
fn count_code_blocks(prompt: &str) -> u32 {
    (prompt.matches("```").count() / 2) as u32
}

/// Whether `prompt` contains a recognizable stack-trace/traceback
/// marker. A small, deterministic keyword set — never an LLM call.
fn has_traceback(prompt: &str) -> bool {
    let lower = prompt.to_lowercase();
    lower.contains("traceback (most recent call last)")
        || lower.contains("panicked at")
        || lower.contains("stack trace")
        || lower.contains("exception in thread")
}

/// Counts whitespace-separated tokens that plausibly denote a file
/// path: contains a `/` path separator, or ends in a common source
/// file extension.
fn count_filepath_tokens(prompt: &str) -> u32 {
    prompt
        .split_whitespace()
        .filter(|token| {
            let trimmed = token.trim_matches(|c: char| {
                !c.is_alphanumeric() && c != '.' && c != '/' && c != '_' && c != '-'
            });
            if trimmed.len() < 2 {
                return false;
            }
            if trimmed.contains('/') {
                return true;
            }
            trimmed.rsplit_once('.').is_some_and(|(_, ext)| {
                COMMON_SOURCE_EXTENSIONS.contains(&ext.to_lowercase().as_str())
            })
        })
        .count() as u32
}

/// Counts whitespace-separated tokens that are purely numeric once
/// leading/trailing punctuation is stripped.
fn count_numeric_tokens(prompt: &str) -> u32 {
    prompt
        .split_whitespace()
        .filter(|token| {
            let trimmed = token.trim_matches(|c: char| !c.is_ascii_digit());
            !trimmed.is_empty() && trimmed.chars().all(|c| c.is_ascii_digit())
        })
        .count() as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recon::{run_recon, ReconBudget};
    use libra_governor_domain::BuildTopology;
    use std::fs;

    fn write_file(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn repo_key_is_stable_for_the_same_canonicalized_path() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(repo_key(dir.path()), repo_key(dir.path()));
    }

    #[test]
    fn repo_key_differs_for_different_paths() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        assert_ne!(repo_key(dir_a.path()), repo_key(dir_b.path()));
    }

    #[test]
    fn repo_key_never_contains_the_raw_path() {
        let dir = tempfile::tempdir().unwrap();
        let key = repo_key(dir.path());
        assert_eq!(key.len(), 16);
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!key.contains(std::path::MAIN_SEPARATOR));
    }

    #[test]
    fn derive_task_features_counts_code_blocks_and_traceback() {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir.path().join("Cargo.toml"), "[package]\nname=\"x\"");
        let recon = run_recon(dir.path(), "fix the bug", &ReconBudget::default());

        let prompt = "It fails with:\n```\npanicked at 'boom', src/main.rs:1\n```\nSee src/main.rs and line 42.";
        let features = derive_task_features(&recon, prompt, dir.path(), Some("claude-sonnet-5"));

        assert_eq!(features.topology, BuildTopology::Cargo);
        assert_eq!(features.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(features.prompt_code_block_count, 1);
        assert!(features.prompt_has_traceback);
        assert!(features.prompt_filepath_token_count >= 1);
        assert!(features.prompt_numeric_token_count >= 1);
        assert_eq!(features.feature_schema_version, FEATURE_SCHEMA_VERSION);
    }

    #[test]
    fn derive_task_features_has_no_traceback_or_code_blocks_for_a_plain_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let recon = run_recon(dir.path(), "add a new feature", &ReconBudget::default());
        let features = derive_task_features(&recon, "add a new feature please", dir.path(), None);
        assert!(!features.prompt_has_traceback);
        assert_eq!(features.prompt_code_block_count, 0);
        assert_eq!(features.model, None);
    }
}
