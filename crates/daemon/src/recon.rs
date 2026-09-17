//! Bounded, read-only reconnaissance of a repository.
//!
//! Everything here is a pure filesystem *read*: no writes, no shelling
//! out, no destructive commands, no spawned subprocesses — see the
//! module-level test asserting the walk never creates or modifies a
//! path. Reconnaissance is explicitly time- and size-capped
//! ([`ReconBudget`]); when the budget is exhausted before enough signal
//! is gathered, callers get a [`ReconOutput`] with `truncated: true` and
//! low confidence rather than a silently expanded scope.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use libra_governor_protocol::Confidence;

/// Directory names never descended into. Chosen because they are large,
/// derived, and essentially never where "the affected area" of a prompt
/// actually lives.
const IGNORED_DIR_NAMES: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".venv",
    "venv",
    "__pycache__",
    ".next",
    ".cargo",
    ".terraform",
    ".turbo",
    "vendor",
];

/// Project marker file -> the test/build command it implies. Checked
/// against every file name seen during the walk (not just the repo
/// root), since a marker can live in a subproject.
const BUILD_MARKERS: &[(&str, &str)] = &[
    ("Cargo.toml", "cargo test"),
    ("package.json", "npm test"),
    ("pyproject.toml", "pytest"),
    ("go.mod", "go test ./..."),
    ("Gemfile", "bundle exec rspec"),
];

/// Hard caps on one reconnaissance run.
#[derive(Debug, Clone, Copy)]
pub struct ReconBudget {
    pub max_duration: Duration,
    pub max_files: usize,
    pub max_depth: usize,
}

impl Default for ReconBudget {
    fn default() -> Self {
        Self {
            max_duration: Duration::from_secs(3),
            max_files: 2_000,
            max_depth: 6,
        }
    }
}

/// The result of one bounded reconnaissance run.
#[derive(Debug, Clone, PartialEq)]
pub struct ReconOutput {
    pub files_scanned: usize,
    pub dirs_scanned: usize,
    pub likely_affected_paths: Vec<String>,
    pub detected_test_commands: Vec<String>,
    pub truncated: bool,
    pub confidence: Confidence,
    pub reason: Option<String>,
    pub elapsed: Duration,
}

/// Runs bounded reconnaissance against `root` for `prompt`, never
/// exceeding `budget`. Returns a low-confidence, explicitly-reasoned
/// result rather than an error when `root` does not exist or evidence is
/// otherwise insufficient — recon never panics and never errors out to
/// its caller.
pub fn run_recon(root: &Path, prompt: &str, budget: &ReconBudget) -> ReconOutput {
    let start = Instant::now();
    let deadline = start + budget.max_duration;

    if !root.is_dir() {
        return ReconOutput {
            files_scanned: 0,
            dirs_scanned: 0,
            likely_affected_paths: vec![],
            detected_test_commands: vec![],
            truncated: false,
            confidence: Confidence::Low,
            reason: Some(format!(
                "cwd {} is not an accessible directory",
                root.display()
            )),
            elapsed: start.elapsed(),
        };
    }

    let prompt_tokens = tokenize(prompt);

    let mut files_scanned = 0usize;
    let mut dirs_scanned = 0usize;
    let mut likely_affected_paths: Vec<String> = Vec::new();
    let mut test_commands: Vec<&'static str> = Vec::new();
    let mut truncated = false;

    // Explicit worklist rather than a recursive walk, so the depth and
    // budget checks below apply uniformly and the whole thing stays
    // trivially non-recursive (no stack-depth concerns on a deep tree).
    let mut worklist: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];

    'walk: while let Some((dir, depth)) = worklist.pop() {
        if Instant::now() >= deadline {
            truncated = true;
            break;
        }
        if depth > budget.max_depth {
            continue;
        }

        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue, // unreadable dir: skip, do not fail the whole run
        };
        dirs_scanned += 1;

        for entry in entries {
            if Instant::now() >= deadline {
                truncated = true;
                break 'walk;
            }
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            if file_type.is_dir() {
                if IGNORED_DIR_NAMES.contains(&name_str.as_ref()) {
                    continue;
                }
                worklist.push((path, depth + 1));
                continue;
            }

            if !file_type.is_file() {
                continue;
            }

            files_scanned += 1;
            if files_scanned >= budget.max_files {
                truncated = true;
                break 'walk;
            }

            for (marker, command) in BUILD_MARKERS {
                if name_str.as_ref() == *marker && !test_commands.contains(command) {
                    test_commands.push(command);
                }
            }

            if let Ok(relative) = path.strip_prefix(root) {
                let relative_str = relative.to_string_lossy().to_lowercase();
                if prompt_tokens
                    .iter()
                    .any(|token| relative_str.contains(token.as_str()))
                {
                    likely_affected_paths.push(relative.to_string_lossy().to_string());
                }
            }
        }
    }

    likely_affected_paths.truncate(20);
    let detected_test_commands: Vec<String> =
        test_commands.into_iter().map(str::to_string).collect();

    let (confidence, reason) = classify_confidence(
        files_scanned,
        truncated,
        &likely_affected_paths,
        &detected_test_commands,
    );

    ReconOutput {
        files_scanned,
        dirs_scanned,
        likely_affected_paths,
        detected_test_commands,
        truncated,
        confidence,
        reason,
        elapsed: start.elapsed(),
    }
}

fn classify_confidence(
    files_scanned: usize,
    truncated: bool,
    likely_affected_paths: &[String],
    detected_test_commands: &[String],
) -> (Confidence, Option<String>) {
    if files_scanned == 0 {
        return (
            Confidence::Low,
            Some("repository appears empty: no files found".to_string()),
        );
    }
    if truncated && likely_affected_paths.is_empty() && detected_test_commands.is_empty() {
        return (
            Confidence::Low,
            Some("reconnaissance budget exhausted before establishing any signal".to_string()),
        );
    }
    if likely_affected_paths.is_empty() && detected_test_commands.is_empty() {
        return (
            Confidence::Low,
            Some(
                "insufficient evidence: no recognized project structure and no prompt-to-path \
                 matches"
                    .to_string(),
            ),
        );
    }
    if !likely_affected_paths.is_empty() && !detected_test_commands.is_empty() {
        return (Confidence::High, None);
    }
    (Confidence::Medium, None)
}

/// Lowercased, alphanumeric-only tokens of length >= 3, used for the
/// deterministic prompt-to-path keyword match. Never calls an LLM.
fn tokenize(prompt: &str) -> Vec<String> {
    prompt
        .split(|c: char| !c.is_alphanumeric())
        .map(|w| w.to_lowercase())
        .filter(|w| w.len() >= 3)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write_file(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn recon_detects_cargo_project_and_matches_prompt_path() {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir.path().join("Cargo.toml"), "[package]\nname=\"x\"");
        write_file(&dir.path().join("src/login.rs"), "// login logic");

        let output = run_recon(dir.path(), "fix the login bug", &ReconBudget::default());

        assert_eq!(output.detected_test_commands, vec!["cargo test"]);
        assert!(
            output
                .likely_affected_paths
                .iter()
                .any(|p| p.contains("login")),
            "expected a login-related path match, got {:?}",
            output.likely_affected_paths
        );
        assert_eq!(output.confidence, Confidence::High);
        assert!(!output.truncated);
    }

    #[test]
    fn recon_ignores_denylisted_directories() {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir.path().join("node_modules/pkg/index.js"), "noop");
        write_file(&dir.path().join("target/debug/build.log"), "noop");

        let output = run_recon(dir.path(), "anything", &ReconBudget::default());

        assert_eq!(output.files_scanned, 0);
    }

    #[test]
    fn recon_never_writes_to_the_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir.path().join("a.txt"), "hello");
        let before: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();

        run_recon(dir.path(), "anything", &ReconBudget::default());

        let after: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(before, after, "recon must never modify the working tree");
    }

    #[test]
    fn recon_reports_low_confidence_with_reason_for_empty_repo() {
        let dir = tempfile::tempdir().unwrap();
        let output = run_recon(dir.path(), "anything", &ReconBudget::default());
        assert_eq!(output.confidence, Confidence::Low);
        assert!(output.reason.is_some());
    }

    #[test]
    fn recon_reports_low_confidence_for_nonexistent_root() {
        let output = run_recon(
            Path::new("/definitely/does/not/exist/libra-recon-test"),
            "anything",
            &ReconBudget::default(),
        );
        assert_eq!(output.confidence, Confidence::Low);
        assert!(output
            .reason
            .unwrap()
            .contains("not an accessible directory"));
    }

    #[test]
    fn recon_stops_at_a_near_zero_time_budget() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..50 {
            write_file(&dir.path().join(format!("dir{i}/file.txt")), "x");
        }
        let budget = ReconBudget {
            max_duration: Duration::from_nanos(1),
            ..ReconBudget::default()
        };
        let output = run_recon(dir.path(), "anything", &budget);
        // A ~0ns budget must trip the deadline check before the walk
        // finishes, deterministically, rather than depending on how fast
        // the walk happens to run on this machine.
        assert!(output.truncated);
    }

    #[test]
    fn recon_stops_at_a_small_file_count_cap() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..20 {
            write_file(&dir.path().join(format!("file{i}.txt")), "x");
        }
        let budget = ReconBudget {
            max_files: 5,
            ..ReconBudget::default()
        };
        let output = run_recon(dir.path(), "anything", &budget);
        assert!(output.truncated);
        assert!(output.files_scanned <= 5);
    }
}
