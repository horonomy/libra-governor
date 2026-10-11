//! Default-off static guards for the `trusted-principal` Cargo feature
//! (ADR-0019 §1/§6, PR-2a's design doc §6, HORO-1835). Always compiled —
//! runs in a plain `cargo test --workspace` with no feature flag, so it
//! actually proves the shipped default build cannot enter trusted mode.
//!
//! Three independent checks, each named for the ADR-0019 §1 guarantee it
//! defends:
//!
//! 1. `trusted-principal` is declared only by `crates/cli/Cargo.toml` and
//!    `crates/daemon/Cargo.toml`'s own `[features]` tables, and no
//!    manifest's `default` key names it.
//! 2. No install script or non-harness CI workflow references it.
//! 3. The trusted-mode source tree never resolves an env-derived path or
//!    spawns a subprocess (statically enforces "trusted mode never calls
//!    `paths::state_dir()`" and "executes no subprocess", ADR-0019 §1).
//!
//! # Why plain text scans, not a TOML/YAML parser
//!
//! Matches the established style of
//! `crates/ledger/tests/renewal_not_wired_live.rs` and
//! `crates/daemon/tests/outcome_authority_not_wired_live.rs`: a
//! lightweight, test-run-time scan of the real files on disk, not a
//! parser. Comments are stripped line-by-line before matching (TOML `#`,
//! YAML `#`) so a doc comment that merely *names* the feature (as
//! `crates/two-principal-harness/Cargo.toml` already does, explaining why
//! it carries no dependency that could unify it) does not itself trip
//! guard 1.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/daemon has a parent (crates/)")
        .parent()
        .expect("crates/ has a parent (the repo root)")
        .to_path_buf()
}

fn collect_files_with_ext(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        // Never descend into `target` — a build output directory can
        // contain arbitrary generated text that has nothing to do with
        // this guard's subject matter, and walking it is slow.
        if file_type.is_dir() {
            if path.file_name().and_then(|n| n.to_str()) == Some("target") {
                continue;
            }
            collect_files_with_ext(&path, ext, out);
        } else if file_type.is_file() && path.extension().and_then(|e| e.to_str()) == Some(ext) {
            out.push(path);
        }
    }
}

/// Strips a `#`-introduced line comment (TOML and YAML share this
/// convention), so a doc comment that merely names the feature in prose
/// does not match a substring scan meant to catch live declarations.
fn strip_hash_comment(line: &str) -> &str {
    match line.find('#') {
        Some(idx) => &line[..idx],
        None => line,
    }
}

#[test]
fn trusted_principal_is_declared_only_by_cli_and_daemon_and_never_default() {
    let root = repo_root();
    let mut manifests = Vec::new();
    collect_files_with_ext(&root.join("crates"), "toml", &mut manifests);
    // The workspace root's own Cargo.toml.
    manifests.push(root.join("Cargo.toml"));

    let allowed_lines = [
        "trusted-principal = []",
        r#"trusted-principal = ["libra-governor-daemon/trusted-principal"]"#,
    ];

    for manifest in &manifests {
        let relative = manifest.strip_prefix(&root).unwrap_or(manifest);
        let Ok(contents) = std::fs::read_to_string(manifest) else {
            continue;
        };
        for (lineno, raw_line) in contents.lines().enumerate() {
            let line = strip_hash_comment(raw_line).trim();
            if !line.contains("trusted-principal") {
                continue;
            }
            let is_allowed_declaration = (relative == Path::new("crates/cli/Cargo.toml")
                || relative == Path::new("crates/daemon/Cargo.toml"))
                && allowed_lines.contains(&line);
            assert!(
                is_allowed_declaration,
                "ADR-0019 §1: `trusted-principal` must only be declared (outside a comment) \
                 on the two allowed `[features]` lines in crates/cli/Cargo.toml and \
                 crates/daemon/Cargo.toml. Found an unexpected reference in {relative:?} \
                 line {}: {raw_line:?}",
                lineno + 1
            );
            assert!(
                !line.starts_with("default"),
                "ADR-0019 §1: `trusted-principal` must never appear in a `default` feature \
                 list. Found in {relative:?} line {}: {raw_line:?}",
                lineno + 1
            );
        }
    }
}

#[test]
fn no_install_script_or_non_harness_workflow_enables_trusted_principal() {
    let root = repo_root();

    // scripts/install.sh — never mentions the feature at all.
    let install_script = root.join("scripts").join("install.sh");
    if let Ok(contents) = std::fs::read_to_string(&install_script) {
        assert!(
            !contents.contains("trusted-principal"),
            "ADR-0019 §1: scripts/install.sh must never reference the trusted-principal feature"
        );
    }

    // Every workflow file, except the one explicit allowlisted file where
    // ADR-0019 mandates the feature-enabled clippy/test step.
    let allowed_workflow: &Path = Path::new(".github/workflows/two-principal-harness.yml");
    let mut workflows = Vec::new();
    collect_files_with_ext(
        &root.join(".github").join("workflows"),
        "yml",
        &mut workflows,
    );
    for workflow in &workflows {
        let relative = workflow.strip_prefix(&root).unwrap_or(workflow);
        if relative == allowed_workflow {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(workflow) else {
            continue;
        };
        assert!(
            !contents.contains("trusted-principal"),
            "ADR-0019 §1: only .github/workflows/two-principal-harness.yml may reference \
             the trusted-principal feature. Found a reference in {relative:?}"
        );
    }

    // `harness/two-principal/Dockerfile` is outside the scanned set by
    // construction (ADR-0019 §6's design doc §6) — it legitimately builds
    // the feature-enabled binary for the harness image.
}

#[test]
fn trusted_mode_code_never_resolves_env_paths_or_spawns() {
    let root = repo_root();
    let scanned_roots = [
        root.join("crates/daemon/src/trusted_principal"),
        root.join("crates/cli/src/trusted_client.rs"),
    ];

    // Substring needles. Deliberately matched even inside comments and
    // tests (design doc §6 "Trap"): this statically enforces "trusted
    // mode never calls paths::state_dir()" and "executes no subprocess"
    // for the whole module tree, no exceptions.
    let patterns = ["paths::", "env::var", "Command::new"];

    let mut files = Vec::new();
    for scanned in &scanned_roots {
        if scanned.is_dir() {
            collect_files_with_ext(scanned, "rs", &mut files);
        } else if scanned.is_file() && scanned.extension().and_then(|e| e.to_str()) == Some("rs") {
            files.push(scanned.clone());
        }
        // Not existing yet is fine — PR-2a may land before PR-2c's
        // `trusted_client.rs` exists, and this guard must not require it.
    }

    for file in &files {
        let relative = file.strip_prefix(&root).unwrap_or(file);
        let Ok(contents) = std::fs::read_to_string(file) else {
            continue;
        };
        for pattern in patterns {
            assert!(
                !contents.contains(pattern),
                "ADR-0019 §1: trusted-mode source must never resolve an env-derived state \
                 directory or spawn a subprocess, even in a comment or test fixture. Found \
                 {pattern:?} in {relative:?}"
            );
        }
    }
}
